//! HTTP messaging API against an in-process broker, driven by reqwest, tokio-tungstenite and
//! (for interop) lapin on the AMQP port.

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use mq_core::log::FileStore;
use mq_core::meta::SqliteMeta;
use mq_core::{Broker, BrokerConfig};
use mq_http::{Config, HttpApi};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct H {
    broker: Arc<Broker>,
    api: Arc<HttpApi>,
    base: String,
    amqp: String,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

impl H {
    async fn start() -> H {
        let _ = tracing_subscriber::fmt().with_env_filter("mq_http=debug,mq_core=info").with_test_writer().try_init();
        let dir = tempfile::tempdir().unwrap();
        let meta = SqliteMeta::open(&dir.path().join("meta.db")).unwrap();
        let logs = FileStore::new(dir.path().join("logs"));
        let broker = Broker::start(BrokerConfig::default(), meta, logs).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api = HttpApi::start(broker.clone(), Config { listen: addr, ..Config::default() }).await.unwrap();
        tokio::spawn(mq_http::serve_api(api.clone(), listener));
        let amqp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let amqp_addr = amqp_listener.local_addr().unwrap();
        let b = broker.clone();
        tokio::spawn(async move {
            let _ = mq_amqp::serve_listener(b, amqp_listener, mq_amqp::ServerConfig { listen: amqp_addr, amqp10: None }).await;
        });
        H {
            broker,
            api,
            base: format!("http://{addr}"),
            amqp: format!("amqp://guest:guest@{amqp_addr}/%2f"),
            http: reqwest::Client::new(),
            _dir: dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let r = req.send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap();
        (status, if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) })
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(self.http.post(self.url(path)).basic_auth("guest", Some("guest")).json(&body)).await
    }

    async fn put(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(self.http.put(self.url(path)).basic_auth("guest", Some("guest")).json(&body)).await
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(self.http.get(self.url(path)).basic_auth("guest", Some("guest"))).await
    }

    async fn queue(&self, name: &str, body: Value) {
        let (s, v) = self.put(&format!("/v1/vhosts/%2F/queues/{name}"), body).await;
        assert!(s.is_success(), "{s} {v}");
    }

    async fn depth(&self, name: &str) -> (u64, u64) {
        let (_, v) = self.get(&format!("/v1/vhosts/%2F/queues/{name}")).await;
        (v["messages_ready"].as_u64().unwrap(), v["messages_unacked"].as_u64().unwrap())
    }

    async fn wait_depth(&self, name: &str, want: (u64, u64)) {
        for _ in 0..200 {
            if self.depth(name).await == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("queue {name}: depth {:?}, wanted {want:?}", self.depth(name).await);
    }

    async fn receive(&self, q: &str, body: Value) -> Vec<Value> {
        let (s, v) = self.post(&format!("/v1/vhosts/%2F/queues/{q}/receive"), body).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["messages"].as_array().unwrap().clone()
    }
}

#[tokio::test]
async fn health_auth_and_errors() {
    let h = H::start().await;
    let r = h.http.get(h.url("/v1/health")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["status"], "ok");

    let (s, v) = h.send(h.http.get(h.url("/v1/vhosts/%2F/queues/x"))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"]["code"], "unauthorized");
    let (s, _) = h.send(h.http.get(h.url("/v1/vhosts/%2F/queues/x")).basic_auth("guest", Some("wrong"))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    let (s, v) = h.get("/v1/vhosts/%2F/queues/missing").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["message"], "NOT_FOUND - no queue 'missing' in vhost '/'");
    let (s, v) = h.get("/v1/vhosts/nope/queues/x").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, v) = h.get("/v1/nothing").await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::NOT_FOUND, Some("not_found")));
    let (s, v) = h.post("/v1/vhosts/%2F/queues/x/messages", json!([1])).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // Declare is idempotent; inequivalent redeclares conflict like AMQP.
    let (s, v) = h.put("/v1/vhosts/%2F/queues/decl", json!({"type": "quorum"})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["type"], "quorum");
    let (s, _) = h.put("/v1/vhosts/%2F/queues/decl", json!({"type": "quorum"})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = h.put("/v1/vhosts/%2F/queues/decl", json!({"type": "classic"})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("inequivalent arg 'x-queue-type'"));

    // CORS preflight.
    let r = h
        .http
        .request(reqwest::Method::OPTIONS, h.url("/v1/vhosts/%2F/queues/decl/messages"))
        .header("origin", "https://app.example")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "authorization,content-type,idempotency-key")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    assert_eq!(r.headers()["access-control-allow-origin"], "*");
    assert!(r.headers()["access-control-allow-headers"].to_str().unwrap().contains("authorization"));
}

#[tokio::test]
async fn publish_single_and_batch_with_confirms() {
    let h = H::start().await;
    h.queue("orders", json!({})).await;
    let (s, v) = h.post("/v1/vhosts/%2F/queues/orders/messages", json!({"body": "hello", "id": "o-1"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({"id": "o-1", "routed": true, "confirmed": true}));

    let batch = json!({"messages": [
        {"routing_key": "orders", "body": {"n": 1}},
        {"routing_key": "orders", "body": "AAEC", "body_encoding": "base64"},
        {"routing_key": "orders", "body": "x", "properties": {"delivery_mode": 1, "content_type": "text/plain"}, "headers": {"k": "v"}},
    ]});
    let (s, v) = h.post("/v1/vhosts/%2F/exchanges/amq.default/publish", batch).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r["routed"] == true && r["confirmed"] == true && r["id"].is_string()));

    // Unroutable: confirmed (nothing to store) but not routed.
    let (s, v) = h.post("/v1/vhosts/%2F/exchanges/amq.direct/publish", json!({"routing_key": "nowhere", "body": "x", "mandatory": true})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((v["routed"].as_bool(), v["confirmed"].as_bool()), (Some(false), Some(true)));

    let (s, v) = h.post("/v1/vhosts/%2F/exchanges/missing/publish", json!({"body": "x"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["error"]["message"], "NOT_FOUND - no exchange 'missing' in vhost '/'");
    let (s, v) = h.post("/v1/vhosts/%2F/queues/missing/messages", json!({"body": "x"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, v) = h.post("/v1/vhosts/%2F/queues/orders/messages", json!({"messages": [{"body": "ok"}, {"body": 1, "body_encoding": "utf8"}]})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().starts_with("messages[1]:"), "{v}");

    h.wait_depth("orders", (4, 0)).await;
    let msgs = h.receive("orders", json!({"max": 10})).await;
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0]["body"], "hello");
    assert_eq!(msgs[0]["id"], "o-1");
    assert_eq!(msgs[0]["properties"]["delivery_mode"], 2);
    assert_eq!(msgs[1]["body"], r#"{"n":1}"#);
    assert_eq!(msgs[1]["properties"]["content_type"], "application/json");
    assert_eq!((msgs[2]["body"].as_str(), msgs[2]["body_encoding"].as_str()), (Some("AAEC"), Some("base64")));
    assert_eq!(msgs[3]["headers"]["k"], "v");
    assert_eq!(msgs[3]["properties"]["delivery_mode"], 1);
}

#[tokio::test]
async fn idempotency_key_and_message_id_dedup() {
    let h = H::start().await;
    h.queue("idem", json!({})).await;
    let post = |key: &'static str, body: Value| h.send(h.http.post(h.url("/v1/vhosts/%2F/queues/idem/messages")).basic_auth("guest", Some("guest")).header("Idempotency-Key", key).json(&body));
    let r1 = h.http.post(h.url("/v1/vhosts/%2F/queues/idem/messages")).basic_auth("guest", Some("guest")).header("Idempotency-Key", "k1").json(&json!({"body": "a"})).send().await.unwrap();
    assert!(r1.headers().get("idempotent-replayed").is_none());
    let v1: Value = r1.json().await.unwrap();
    let r2 = h.http.post(h.url("/v1/vhosts/%2F/queues/idem/messages")).basic_auth("guest", Some("guest")).header("Idempotency-Key", "k1").json(&json!({"body": "a"})).send().await.unwrap();
    assert_eq!(r2.headers()["idempotent-replayed"], "true");
    assert_eq!(r2.json::<Value>().await.unwrap(), v1);
    let (s, v) = post("k1", json!({"body": "different"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"]["code"], "idempotency_key_reused");
    h.wait_depth("idem", (1, 0)).await;

    // A key is scoped to the endpoint: the same key elsewhere is a new request.
    h.queue("idem2", json!({})).await;
    let (s, _) = h.send(h.http.post(h.url("/v1/vhosts/%2F/queues/idem2/messages")).basic_auth("guest", Some("guest")).header("Idempotency-Key", "k1").json(&json!({"body": "a"}))).await;
    assert_eq!(s, StatusCode::OK);
    h.wait_depth("idem2", (1, 0)).await;

    // Message ids dedupe without a key.
    let (_, a) = h.post("/v1/vhosts/%2F/queues/idem/messages", json!({"id": "same", "body": "x"})).await;
    let (_, b) = h.post("/v1/vhosts/%2F/queues/idem/messages", json!({"id": "same", "body": "x"})).await;
    assert_eq!(a["duplicate"], Value::Null);
    assert_eq!(b["duplicate"], true);
    assert_eq!(b["id"], "same");
    h.wait_depth("idem", (2, 0)).await;

    // Queues with engine dedup get the header too.
    h.queue("dedupq", json!({"arguments": {"x-message-deduplication": true}})).await;
    h.post("/v1/vhosts/%2F/queues/dedupq/messages", json!({"id": "d1", "body": "x"})).await;
    let m = h.receive("dedupq", json!({})).await;
    assert_eq!(m[0]["headers"]["x-deduplication-header"], "d1");
}

#[tokio::test]
async fn long_poll_wakes_on_publish() {
    let h = Arc::new(H::start().await);
    h.queue("lp", json!({})).await;
    let t = Instant::now();
    let empty = h.receive("lp", json!({"wait_ms": 300})).await;
    assert!(empty.is_empty());
    assert!(t.elapsed() >= Duration::from_millis(290), "{:?}", t.elapsed());

    let h2 = h.clone();
    let waiter = tokio::spawn(async move {
        let t = Instant::now();
        let m = h2.receive("lp", json!({"wait_ms": 10_000})).await;
        (m, t.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let published = Instant::now();
    h.post("/v1/vhosts/%2F/queues/lp/messages", json!({"body": "wake"})).await;
    let (m, total) = waiter.await.unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["body"], "wake");
    assert!(published.elapsed() < Duration::from_millis(500), "woke {:?} after publish", published.elapsed());
    assert!(total < Duration::from_secs(2));

    let (s, _) = h.post("/v1/vhosts/%2F/queues/lp/receive", json!({"max": 101})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h.post("/v1/vhosts/%2F/queues/lp/receive", json!({"wait_ms": 20_001})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn visibility_timeout_redelivers() {
    let h = H::start().await;
    h.queue("vis", json!({"type": "quorum"})).await;
    h.post("/v1/vhosts/%2F/queues/vis/messages", json!({"body": "m"})).await;
    let first = h.receive("vis", json!({"visibility_ms": 300})).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["delivery_count"], 0);
    assert_eq!(first[0]["redelivered"], false);
    assert!(h.receive("vis", json!({})).await.is_empty(), "invisible while leased");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let again = h.receive("vis", json!({"visibility_ms": 30_000})).await;
    assert_eq!(again.len(), 1);
    assert_eq!(again[0]["redelivered"], true);
    assert_eq!(again[0]["delivery_count"], 1);
    assert_eq!(again[0]["headers"]["x-delivery-count"], 1);

    let (s, v) = h.post("/v1/vhosts/%2F/queues/vis/ack", json!({"receipts": [first[0]["receipt"]]})).await;
    assert_eq!(s, StatusCode::GONE, "{v}");
    assert_eq!(v["error"]["code"], "receipt_expired");
    assert_eq!(v["results"][0]["ok"], false);
    let (s, v) = h.post("/v1/vhosts/%2F/queues/vis/ack", json!({"receipts": [again[0]["receipt"]]})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["succeeded"], 1);
    h.wait_depth("vis", (0, 0)).await;
}

#[tokio::test]
async fn ack_nack_extend_and_bad_receipts() {
    let h = H::start().await;
    h.queue("dlq", json!({})).await;
    h.queue("work", json!({"arguments": {"x-dead-letter-exchange": "", "x-dead-letter-routing-key": "dlq"}})).await;
    for i in 0..3 {
        h.post("/v1/vhosts/%2F/queues/work/messages", json!({"body": format!("m{i}")})).await;
    }
    let msgs = h.receive("work", json!({"max": 3, "visibility_ms": 400})).await;
    assert_eq!(msgs.len(), 3);
    let receipt = |i: usize| msgs[i]["receipt"].as_str().unwrap().to_string();

    // Extend m0 well past the original timeout.
    let (s, v) = h.post("/v1/vhosts/%2F/queues/work/visibility", json!({"receipts": [receipt(0)], "visibility_ms": 5000})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let fresh = v["results"][0]["receipt"].as_str().unwrap().to_string();
    assert_ne!(fresh, receipt(0));
    // m1 back to the queue now; m2 rejected → dead-lettered.
    let (s, v) = h.post("/v1/vhosts/%2F/queues/work/nack", json!({"receipts": [receipt(1)]})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _) = h.post("/v1/vhosts/%2F/queues/work/nack", json!({"receipts": [receipt(2)], "requeue": false})).await;
    assert_eq!(s, StatusCode::OK);
    h.wait_depth("dlq", (1, 0)).await;
    let dead = h.receive("dlq", json!({})).await;
    assert_eq!(dead[0]["body"], "m2");
    assert_eq!(dead[0]["headers"]["x-death"][0]["reason"], "rejected");

    tokio::time::sleep(Duration::from_millis(600)).await;
    let back = h.receive("work", json!({"max": 10, "visibility_ms": 30_000})).await;
    assert_eq!(back.len(), 1, "only m1 is visible; m0 was extended: {back:?}");
    assert_eq!(back[0]["body"], "m1");

    // Settled twice → stale; garbage → invalid; another queue's receipt → invalid.
    let (s, _) = h.post("/v1/vhosts/%2F/queues/work/ack", json!({"receipts": [fresh]})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = h.post("/v1/vhosts/%2F/queues/work/ack", json!({"receipts": [fresh]})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("receipt_stale")), "{v}");
    let (s, v) = h.post("/v1/vhosts/%2F/queues/work/ack", json!({"receipts": ["bogus"]})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid_receipt")));
    let (s, v) = h.post("/v1/vhosts/%2F/queues/dlq/ack", json!({"receipts": [back[0]["receipt"]]})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid_receipt")));
    // Partial success reports each receipt.
    let (s, v) = h.post("/v1/vhosts/%2F/queues/work/ack", json!({"receipts": [back[0]["receipt"], "bogus"]})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["succeeded"], 1);
    assert_eq!(v["results"][0]["ok"], true);

    // Delayed nack: invisible for the delay, then back.
    h.post("/v1/vhosts/%2F/queues/work/messages", json!({"body": "later"})).await;
    let m = h.receive("work", json!({})).await;
    let (s, _) = h.post("/v1/vhosts/%2F/queues/work/nack", json!({"receipts": [m[0]["receipt"]], "delay_ms": 300})).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.receive("work", json!({})).await.is_empty());
    let m = h.receive("work", json!({"wait_ms": 2000})).await;
    assert_eq!(m[0]["body"], "later");
    assert_eq!(m[0]["delivery_count"], 1);
    h.wait_depth("work", (0, 1)).await;
}

#[tokio::test]
async fn token_scopes() {
    let h = H::start().await;
    h.queue("orders", json!({})).await;
    h.queue("other", json!({})).await;
    let (s, t) = h.post("/v1/tokens", json!({"name": "edge", "vhost": "/", "scopes": ["publish:^orders$", "consume:^orders$"]})).await;
    assert_eq!(s, StatusCode::CREATED, "{t}");
    let token = t["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("roll_"));
    let id = t["id"].as_str().unwrap().to_string();
    let bearer = |m: reqwest::Method, path: &str| h.http.request(m, h.url(path)).bearer_auth(&token);

    let (s, v) = h.send(bearer(reqwest::Method::POST, "/v1/vhosts/%2F/queues/orders/messages").json(&json!({"body": "x"}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = h.send(bearer(reqwest::Method::POST, "/v1/vhosts/%2F/queues/other/messages").json(&json!({"body": "x"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"]["code"], "access_refused");
    let (s, v) = h.send(bearer(reqwest::Method::POST, "/v1/vhosts/%2F/exchanges/amq.direct/publish").json(&json!({"routing_key": "orders", "body": "x"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, v) = h.send(bearer(reqwest::Method::POST, "/v1/vhosts/%2F/queues/orders/receive").json(&json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 1);
    let (s, _) = h.send(bearer(reqwest::Method::POST, "/v1/vhosts/%2F/queues/other/receive").json(&json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = h.send(bearer(reqwest::Method::PUT, "/v1/vhosts/%2F/queues/orders").json(&json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "no manage scope");
    let (s, _) = h.send(bearer(reqwest::Method::POST, "/v1/tokens").json(&json!({"name": "x", "vhost": "/", "scopes": ["consume:.*"]}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "tokens can't mint tokens");

    // Browsers can't set headers on EventSource: the token also works as a query parameter.
    let r = h.http.get(h.url(&format!("/v1/vhosts/%2F/queues/orders/events?access_token={token}"))).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let mut ev = Events::new(r);
    assert_eq!(ev.next(Duration::from_secs(5)).await.unwrap().0, "open");
    drop(ev);

    // Listing never shows secrets.
    let (s, list) = h.get("/v1/tokens").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(list[0].get("token").is_none() && list[0].get("hash").is_none());

    // A user without permissions can't use a token beyond them.
    h.broker.put_user(mq_core::auth::User::new("bob", "pw", vec![])).await.unwrap();
    h.broker.set_permission("/", "bob", mq_core::auth::Permission { configure: "".into(), write: "".into(), read: "^orders$".into() }).await.unwrap();
    let (s, bt) = h.send(h.http.post(h.url("/v1/tokens")).basic_auth("bob", Some("pw")).json(&json!({"name": "b", "vhost": "/", "scopes": ["publish:.*", "consume:.*"]}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let btoken = bt["token"].as_str().unwrap();
    let (s, _) = h.send(h.http.post(h.url("/v1/vhosts/%2F/queues/orders/messages")).bearer_auth(btoken).json(&json!({"body": "x"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "bob has no write permission");
    let (s, _) = h.send(h.http.get(h.url("/v1/tokens")).basic_auth("bob", Some("pw"))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = h.send(h.http.delete(h.url(&format!("/v1/tokens/{id}"))).basic_auth("bob", Some("pw"))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "bob can't see guest's token");

    // Revocation and expiry.
    let (s, _) = h.send(h.http.delete(h.url(&format!("/v1/tokens/{id}"))).basic_auth("guest", Some("guest"))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = h.send(bearer(reqwest::Method::POST, "/v1/vhosts/%2F/queues/orders/receive").json(&json!({}))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (_, t) = h.post("/v1/tokens", json!({"name": "short", "vhost": "/", "scopes": ["consume:.*"], "expires_in": 1})).await;
    let short = t["token"].as_str().unwrap().to_string();
    let (s, _) = h.send(h.http.get(h.url("/v1/vhosts/%2F/queues/orders")).bearer_auth(&short)).await;
    assert_eq!(s, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let (s, v) = h.send(h.http.get(h.url("/v1/vhosts/%2F/queues/orders")).bearer_auth(&short)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"]["message"], "token expired");
    let (s, _) = h.send(h.http.get(h.url("/v1/vhosts/%2F/queues/orders")).bearer_auth("roll_0123456789abcdef_nope")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, v) = h.post("/v1/tokens", json!({"name": "bad", "vhost": "/", "scopes": ["write:.*"]})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}

#[tokio::test]
async fn queue_and_exchange_management() {
    let h = H::start().await;
    let del = |path: &str| h.http.delete(h.url(path)).basic_auth("guest", Some("guest"));
    for q in ["orders", "orders.dlq", "other"] {
        h.queue(q, json!({})).await;
    }

    // List, with a prefix filter.
    let (s, v) = h.get("/v1/vhosts/%2F/queues").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let names = |v: &Value| v.as_array().unwrap().iter().map(|q| q["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(names(&v), ["orders", "orders.dlq", "other"]);
    assert_eq!(v[0]["messages_ready"], 0);
    let (_, v) = h.get("/v1/vhosts/%2F/queues?prefix=orders").await;
    assert_eq!(names(&v), ["orders", "orders.dlq"]);
    let (s, _) = h.get("/v1/vhosts/nope/queues").await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Exchanges: declare (idempotent), validation, reserved names.
    let (s, v) = h.put("/v1/vhosts/%2F/exchanges/orders.x", json!({"type": "topic"})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!((v["type"].as_str(), v["durable"].as_bool()), (Some("topic"), Some(true)));
    let (s, _) = h.put("/v1/vhosts/%2F/exchanges/orders.x", json!({"type": "topic"})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = h.put("/v1/vhosts/%2F/exchanges/orders.x", json!({"type": "fanout"})).await;
    assert_eq!(s, StatusCode::CONFLICT, "inequivalent redeclare: {v}");
    let (s, _) = h.put("/v1/vhosts/%2F/exchanges/x", json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "type is required");
    let (s, _) = h.put("/v1/vhosts/%2F/exchanges/amq.mine", json!({"type": "direct"})).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = h.put("/v1/vhosts/%2F/exchanges/later", json!({"type": "x-delayed-message", "arguments": {"x-delayed-type": "direct"}})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");

    // Bind, publish through the exchange, unbind.
    let binding = json!({"source": "orders.x", "destination": "orders", "routing_key": "order.#"});
    let (s, v) = h.post("/v1/vhosts/%2F/bindings", binding.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["destination_type"], "queue");
    let (_, v) = h.post("/v1/vhosts/%2F/exchanges/orders.x/publish", json!({"routing_key": "order.created", "body": "a"})).await;
    assert_eq!(v["routed"], true, "{v}");
    h.wait_depth("orders", (1, 0)).await;
    let (s, v) = h.send(del("/v1/vhosts/%2F/bindings").json(&binding)).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    let (_, v) = h.post("/v1/vhosts/%2F/exchanges/orders.x/publish", json!({"routing_key": "order.created", "body": "b"})).await;
    assert_eq!(v["routed"], false, "{v}");
    let (s, _) = h.post("/v1/vhosts/%2F/bindings", json!({"source": "missing", "destination": "orders"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h.post("/v1/vhosts/%2F/bindings", json!({"source": "orders.x", "destination": "orders", "destination_type": "topic"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // Exchange-to-exchange.
    h.put("/v1/vhosts/%2F/exchanges/all", json!({"type": "fanout"})).await;
    let (s, v) = h.post("/v1/vhosts/%2F/bindings", json!({"source": "all", "destination": "orders.x", "destination_type": "exchange"})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");

    // Purge, then delete with if_empty / if_unused.
    let (s, v) = h.post("/v1/vhosts/%2F/queues/orders/purge", json!({})).await;
    assert_eq!((s, v), (StatusCode::OK, json!({"purged": 1})));
    h.wait_depth("orders", (0, 0)).await;
    h.post("/v1/vhosts/%2F/queues/orders/messages", json!({"body": "c"})).await;
    let (s, v) = h.send(del("/v1/vhosts/%2F/queues/orders?if_empty=true")).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, v) = h.send(del("/v1/vhosts/%2F/queues/orders")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({"deleted": "orders", "messages": 1}));
    let (s, _) = h.get("/v1/vhosts/%2F/queues/orders").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h.send(del("/v1/vhosts/%2F/queues/orders")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h.send(del("/v1/vhosts/%2F/exchanges/all?if_unused=true")).await;
    assert_eq!(s, StatusCode::CONFLICT, "all is bound to orders.x");
    let (s, _) = h.send(del("/v1/vhosts/%2F/exchanges/all")).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = h.send(del("/v1/vhosts/%2F/exchanges/all")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h.send(del("/v1/vhosts/%2F/exchanges/amq.direct")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn management_permissions_and_scopes() {
    let h = H::start().await;
    for q in ["orders", "other"] {
        h.queue(q, json!({})).await;
    }
    h.put("/v1/vhosts/%2F/exchanges/orders.x", json!({"type": "topic"})).await;

    // A consume-only token sees only its queue and can't change anything.
    let (_, t) = h.post("/v1/tokens", json!({"name": "reader", "vhost": "/", "scopes": ["consume:^orders$"]})).await;
    let reader = t["token"].as_str().unwrap().to_string();
    let (s, v) = h.send(h.http.get(h.url("/v1/vhosts/%2F/queues")).bearer_auth(&reader)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().iter().map(|q| q["name"].clone()).collect::<Vec<_>>(), [json!("orders")]);
    let (s, _) = h.send(h.http.post(h.url("/v1/vhosts/%2F/queues/orders/purge")).bearer_auth(&reader)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "purge needs a manage scope");
    let (s, _) = h.send(h.http.delete(h.url("/v1/vhosts/%2F/queues/orders")).bearer_auth(&reader)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = h.send(h.http.put(h.url("/v1/vhosts/%2F/exchanges/r.x")).bearer_auth(&reader).json(&json!({"type": "direct"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = h.send(h.http.post(h.url("/v1/vhosts/%2F/bindings")).bearer_auth(&reader).json(&json!({"source": "orders.x", "destination": "orders"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // A manage token works on what its scopes match.
    let (_, t) = h.post("/v1/tokens", json!({"name": "admin", "vhost": "/", "scopes": ["manage:^orders"]})).await;
    let manager = t["token"].as_str().unwrap().to_string();
    let (s, v) = h.send(h.http.post(h.url("/v1/vhosts/%2F/bindings")).bearer_auth(&manager).json(&json!({"source": "orders.x", "destination": "orders", "routing_key": "#"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = h.send(h.http.post(h.url("/v1/vhosts/%2F/queues/orders/purge")).bearer_auth(&manager)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _) = h.send(h.http.delete(h.url("/v1/vhosts/%2F/queues/other")).bearer_auth(&manager)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "scope doesn't match 'other'");
    let (s, v) = h.send(h.http.put(h.url("/v1/vhosts/%2F/exchanges/orders.dlx")).bearer_auth(&manager).json(&json!({"type": "fanout"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, _) = h.send(h.http.delete(h.url("/v1/vhosts/%2F/exchanges/orders.dlx")).bearer_auth(&manager)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Users get AMQP's checks: configure to declare/delete, write+read to bind, read to purge.
    h.broker.put_user(mq_core::auth::User::new("bob", "pw", vec![])).await.unwrap();
    h.broker.set_permission("/", "bob", mq_core::auth::Permission { configure: "^bob\\.".into(), write: "^orders$".into(), read: "^orders".into() }).await.unwrap();
    let bob = |m: reqwest::Method, path: &str| h.http.request(m, h.url(path)).basic_auth("bob", Some("pw"));
    let (s, _) = h.send(bob(reqwest::Method::PUT, "/v1/vhosts/%2F/exchanges/x").json(&json!({"type": "direct"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = h.send(bob(reqwest::Method::PUT, "/v1/vhosts/%2F/exchanges/bob.x").json(&json!({"type": "direct"}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = h.send(bob(reqwest::Method::PUT, "/v1/vhosts/%2F/exchanges/bob.y").json(&json!({"type": "direct", "arguments": {"alternate-exchange": "other.ae"}}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "no read on the alternate exchange");
    let (s, _) = h.send(bob(reqwest::Method::POST, "/v1/vhosts/%2F/bindings").json(&json!({"source": "orders.x", "destination": "orders", "routing_key": "b"}))).await;
    assert_eq!(s, StatusCode::CREATED, "write on orders, read on orders.x");
    let (s, _) = h.send(bob(reqwest::Method::POST, "/v1/vhosts/%2F/bindings").json(&json!({"source": "bob.x", "destination": "orders"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "no read on bob.x");
    let (s, _) = h.send(bob(reqwest::Method::POST, "/v1/vhosts/%2F/queues/orders/purge")).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = h.send(bob(reqwest::Method::DELETE, "/v1/vhosts/%2F/queues/orders")).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "no configure on orders");
    let (s, v) = h.send(bob(reqwest::Method::GET, "/v1/vhosts/%2F/queues")).await;
    assert_eq!(v.as_array().map(|a| a.len()), Some(1), "{s} {v}");
}

/// Reads SSE events from a response body.
struct Events {
    stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: String,
}

impl Events {
    fn new(r: reqwest::Response) -> Self {
        Events { stream: Box::pin(r.bytes_stream()), buf: String::new() }
    }

    /// Next event as (event, data); comments (heartbeats) are skipped.
    async fn next(&mut self, timeout: Duration) -> Option<(String, Value)> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(i) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..i + 2).collect();
                let mut event = "message".to_string();
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        event = v.trim().into();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim_start());
                    }
                }
                if data.is_empty() {
                    continue;
                }
                return Some((event, serde_json::from_str(&data).unwrap()));
            }
            match tokio::time::timeout_at(deadline, self.stream.next()).await {
                Ok(Some(Ok(b))) => self.buf.push_str(&String::from_utf8_lossy(&b)),
                _ => return None,
            }
        }
    }
}

#[tokio::test]
async fn sse_stream_manual_ack_and_requeue() {
    let h = H::start().await;
    h.queue("sse", json!({})).await;
    let r = h.http.get(h.url("/v1/vhosts/%2F/queues/sse/events?ack=manual&prefetch=2")).basic_auth("guest", Some("guest")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    let mut ev = Events::new(r);
    let (kind, open) = ev.next(Duration::from_secs(5)).await.unwrap();
    assert_eq!(kind, "open");
    assert_eq!(open["prefetch"], 2);
    for i in 0..3 {
        h.post("/v1/vhosts/%2F/queues/sse/messages", json!({"body": format!("s{i}")})).await;
    }
    let (_, a) = ev.next(Duration::from_secs(5)).await.unwrap();
    let (_, b) = ev.next(Duration::from_secs(5)).await.unwrap();
    assert_eq!((a["body"].as_str(), b["body"].as_str()), (Some("s0"), Some("s1")));
    assert!(ev.next(Duration::from_millis(300)).await.is_none(), "prefetch 2 holds back the third");
    let (s, v) = h.post("/v1/vhosts/%2F/queues/sse/ack", json!({"receipts": [a["receipt"]]})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, c) = ev.next(Duration::from_secs(5)).await.unwrap();
    assert_eq!(c["body"], "s2");
    let (s, _) = h.post("/v1/vhosts/%2F/queues/sse/ack", json!({"receipts": [a["receipt"]]})).await;
    assert_eq!(s, StatusCode::CONFLICT, "acked twice");
    let (s, _) = h.post("/v1/vhosts/%2F/queues/sse/visibility", json!({"receipts": [b["receipt"]], "visibility_ms": 10})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.wait_depth("sse", (0, 2)).await;
    // Disconnecting returns what the stream held.
    drop(ev);
    h.wait_depth("sse", (2, 0)).await;
    let (s, v) = h.post("/v1/vhosts/%2F/queues/sse/ack", json!({"receipts": [b["receipt"]]})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::GONE, Some("session_closed")));

    // Auto ack drains the queue; the connection shows up while streaming.
    let r = h.http.get(h.url("/v1/vhosts/%2F/queues/sse/events")).basic_auth("guest", Some("guest")).send().await.unwrap();
    let mut ev = Events::new(r);
    ev.next(Duration::from_secs(5)).await.unwrap();
    let (_, x) = ev.next(Duration::from_secs(5)).await.unwrap();
    assert!(x.get("receipt").is_none());
    assert_eq!(x["redelivered"], true);
    ev.next(Duration::from_secs(5)).await.unwrap();
    h.wait_depth("sse", (0, 0)).await;
    assert!(h.broker.connections().iter().any(|c| c.protocol == "HTTP SSE"));

    // Force-close from management ends the stream with a close event.
    let c = h.broker.connections().into_iter().find(|c| c.protocol == "HTTP SSE").unwrap();
    c.request_close("closed by test");
    let (kind, v) = ev.next(Duration::from_secs(5)).await.unwrap();
    assert_eq!((kind.as_str(), v["reason"].as_str()), ("close", Some("closed by test")));
    assert!(ev.next(Duration::from_secs(2)).await.is_none());
}

#[tokio::test]
async fn sse_stream_resume_with_last_event_id() {
    let h = H::start().await;
    h.queue("events", json!({"type": "stream"})).await;
    for i in 0..5 {
        h.post("/v1/vhosts/%2F/queues/events/messages", json!({"body": format!("e{i}")})).await;
    }
    let r = h.http.get(h.url("/v1/vhosts/%2F/queues/events/events?offset=first")).basic_auth("guest", Some("guest")).send().await.unwrap();
    let mut ev = Events::new(r);
    ev.next(Duration::from_secs(5)).await.unwrap();
    let (_, e0) = ev.next(Duration::from_secs(5)).await.unwrap();
    let (_, e1) = ev.next(Duration::from_secs(5)).await.unwrap();
    assert_eq!((e0["body"].as_str(), e1["body"].as_str()), (Some("e0"), Some("e1")));
    let last = e1["offset"].as_u64().unwrap();
    drop(ev);
    let r = h.http.get(h.url("/v1/vhosts/%2F/queues/events/events")).basic_auth("guest", Some("guest")).header("Last-Event-ID", last.to_string()).send().await.unwrap();
    let mut ev = Events::new(r);
    ev.next(Duration::from_secs(5)).await.unwrap();
    let (_, e2) = ev.next(Duration::from_secs(5)).await.unwrap();
    assert_eq!(e2["body"], "e2");
    assert_eq!(e2["offset"], last + 1);
    let (s, _) = h.post("/v1/vhosts/%2F/queues/events/receive", json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "streams aren't leased");
}

#[tokio::test]
async fn websocket_deliver_ack_publish_credit() {
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let h = H::start().await;
    h.queue("wsq", json!({})).await;
    for i in 0..3 {
        h.post("/v1/vhosts/%2F/queues/wsq/messages", json!({"body": format!("w{i}")})).await;
    }
    let url = format!("{}/v1/vhosts/%2F/queues/wsq/ws?ack=manual&prefetch=1", h.base.replace("http://", "ws://"));
    let mut req = url.into_client_request().unwrap();
    let basic = base64::engine::general_purpose::STANDARD.encode("guest:guest");
    req.headers_mut().insert("authorization", format!("Basic {basic}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let mut next = async || -> Value {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("frame").unwrap().unwrap() {
                Message::Text(t) => return serde_json::from_str(&t).unwrap(),
                _ => continue,
            }
        }
    };
    assert_eq!(next().await["type"], "open");
    let d = next().await;
    assert_eq!((d["type"].as_str(), d["message"]["body"].as_str()), (Some("deliver"), Some("w0")));
    let tag = d["delivery_tag"].as_u64().unwrap();
    drop(next);
    ws.send(Message::text(json!({"type": "ack", "delivery_tag": tag}).to_string())).await.unwrap();
    ws.send(Message::text(json!({"type": "credit", "prefetch": 10}).to_string())).await.unwrap();
    ws.send(Message::text(json!({"type": "publish", "ref": "p1", "body": "from-ws"}).to_string())).await.unwrap();
    ws.send(Message::text(json!({"type": "ack", "delivery_tag": 999}).to_string())).await.unwrap();
    let mut seen = Vec::new();
    let mut confirm = None;
    let mut error = None;
    while seen.len() < 3 || confirm.is_none() || error.is_none() {
        let f = match tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("frame").unwrap().unwrap() {
            Message::Text(t) => serde_json::from_str::<Value>(&t).unwrap(),
            _ => continue,
        };
        match f["type"].as_str().unwrap() {
            "deliver" => seen.push((f["delivery_tag"].as_u64().unwrap(), f["message"]["body"].as_str().unwrap().to_string())),
            "confirm" => confirm = Some(f),
            "error" => error = Some(f),
            t => panic!("unexpected frame {t}"),
        }
    }
    let confirm = confirm.unwrap();
    assert_eq!((confirm["ref"].as_str(), confirm["routed"].as_bool(), confirm["confirmed"].as_bool()), (Some("p1"), Some(true), Some(true)));
    assert!(error.unwrap()["error"]["message"].as_str().unwrap().contains("unknown delivery tag 999"));
    let bodies: Vec<&str> = seen.iter().map(|(_, b)| b.as_str()).collect();
    assert_eq!(bodies, ["w1", "w2", "from-ws"]);
    let last = seen.last().unwrap().0;
    ws.send(Message::text(json!({"type": "nack", "delivery_tag": last, "multiple": true, "requeue": false}).to_string())).await.unwrap();
    h.wait_depth("wsq", (0, 0)).await;
    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn websocket_disconnect_requeues() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let h = H::start().await;
    h.queue("wsr", json!({})).await;
    h.post("/v1/vhosts/%2F/queues/wsr/messages", json!({"body": "keep"})).await;
    let mut req = format!("{}/v1/vhosts/%2F/queues/wsr/ws?ack=manual", h.base.replace("http://", "ws://")).into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("guest:guest")).parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    h.wait_depth("wsr", (0, 1)).await;
    drop(ws);
    h.wait_depth("wsr", (1, 0)).await;
    // Not a WebSocket request: a JSON error, not a panic.
    let (s, v) = h.get("/v1/vhosts/%2F/queues/wsr/ws").await;
    assert!(s.is_client_error(), "{s} {v}");
}

/// What the webhook receiver saw.
#[derive(Debug, Clone)]
struct Hit {
    headers: axum::http::HeaderMap,
    body: String,
}

async fn receiver(fail_first: usize, always_fail: bool) -> (String, tokio::sync::mpsc::UnboundedReceiver<Hit>) {
    use axum::http::StatusCode as S;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
            let tx = tx.clone();
            let count = count.clone();
            async move {
                let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = tx.send(Hit { headers, body });
                if always_fail || n < fail_first { S::INTERNAL_SERVER_ERROR } else { S::OK }
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/hook"), rx)
}

fn verify(secret: &str, h: &Hit) -> bool {
    use hmac::{Hmac, Mac};
    let sig = h.headers["roll-signature"].to_str().unwrap();
    let (t, v1) = sig.split_once(',').unwrap();
    let t = t.strip_prefix("t=").unwrap();
    let v1 = v1.strip_prefix("v1=").unwrap();
    let mut m = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    m.update(format!("{t}.{}", h.body).as_bytes());
    hex::encode(m.finalize().into_bytes()) == v1
}

#[tokio::test]
async fn webhook_signed_delivery_and_retry() {
    let h = H::start().await;
    h.queue("hooked", json!({})).await;
    let (url, mut hits) = receiver(1, false).await;
    let spec = json!({"queue": "hooked", "url": url, "retry": {"max_attempts": 3, "backoff_ms": 50, "max_backoff_ms": 100}, "headers": {"x-team": "a"}});
    let (s, v) = h.put("/v1/vhosts/%2F/webhooks/orders-hook", spec).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let secret = v["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("whsec_"));
    h.post("/v1/vhosts/%2F/queues/hooked/messages", json!({"id": "w-1", "body": "ping"})).await;

    let first = tokio::time::timeout(Duration::from_secs(5), hits.recv()).await.unwrap().unwrap();
    let second = tokio::time::timeout(Duration::from_secs(5), hits.recv()).await.unwrap().unwrap();
    for hit in [&first, &second] {
        assert!(verify(&secret, hit), "signature");
        assert_eq!(hit.headers["roll-webhook-id"], "orders-hook");
        assert_eq!(hit.headers["roll-message-id"], "w-1");
        assert_eq!(hit.headers["x-team"], "a");
        assert_eq!(hit.headers["content-type"], "application/json");
    }
    assert_eq!(first.headers["roll-delivery-attempt"], "1");
    assert_eq!(second.headers["roll-delivery-attempt"], "2");
    let body: Value = serde_json::from_str(&second.body).unwrap();
    assert_eq!((body["body"].as_str(), body["queue"].as_str(), body["webhook"].as_str()), (Some("ping"), Some("hooked"), Some("orders-hook")));
    assert!(!verify("whsec_wrong", &second));

    h.wait_depth("hooked", (0, 0)).await;
    let mut status = Value::Null;
    for _ in 0..100 {
        status = h.get("/v1/vhosts/%2F/webhooks/orders-hook").await.1;
        if status["status"]["delivered"] == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status["status"]["delivered"], 1, "{status}");
    assert_eq!(status["status"]["retries"], 1);
    assert_eq!(status["status"]["state"], "running");
    assert_eq!(status["status"]["last_error"], "HTTP 500 Internal Server Error");
    assert!(status.get("secret").is_none(), "GET hides the secret");

    // Updating keeps the secret; the list shows it; delete stops it.
    let (s, v) = h.put("/v1/vhosts/%2F/webhooks/orders-hook", json!({"queue": "hooked", "url": url})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["secret"], secret.as_str());
    let (_, l) = h.get("/v1/vhosts/%2F/webhooks").await;
    assert_eq!(l.as_array().unwrap().len(), 1);
    let (s, _) = h.send(h.http.delete(h.url("/v1/vhosts/%2F/webhooks/orders-hook")).basic_auth("guest", Some("guest"))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    h.post("/v1/vhosts/%2F/queues/hooked/messages", json!({"body": "after delete"})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.depth("hooked").await, (1, 0));
    let (s, _) = h.get("/v1/vhosts/%2F/webhooks/orders-hook").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h.put("/v1/vhosts/%2F/webhooks/bad", json!({"queue": "hooked", "url": "ftp://x"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// Another node's writes reach this node through the cluster change feed
/// (`Broker::apply_change`); simulate them by writing the store directly.
#[tokio::test]
async fn webhook_changes_made_on_other_nodes_apply() {
    use mq_core::meta::{self, kind};
    let h = H::start().await;
    h.queue("remote-hooked", json!({})).await;
    let (url_a, mut hits_a) = receiver(0, false).await;
    let (url_b, mut hits_b) = receiver(0, false).await;
    let (s, _) = h.put("/v1/vhosts/%2F/webhooks/local", json!({"queue": "remote-hooked", "url": url_a})).await;
    assert_eq!(s, StatusCode::CREATED);
    h.post("/v1/vhosts/%2F/queues/remote-hooked/messages", json!({"body": "one"})).await;
    tokio::time::timeout(Duration::from_secs(5), hits_a.recv()).await.unwrap().unwrap();
    let delivered = async |name: &str| h.get(&format!("/v1/vhosts/%2F/webhooks/{name}")).await.1["status"]["delivered"].as_u64();
    for _ in 0..100 {
        if delivered("local").await == Some(1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Our own write coming back through the feed doesn't restart the runner.
    let k = meta::key(&["/", "local"]);
    h.broker.apply_change(kind::WEBHOOK, &k).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(delivered("local").await, Some(1), "stats survive a no-op change");

    // Changed elsewhere: the new URL takes over.
    let mut row = h.broker.meta.get(kind::WEBHOOK, &k).await.unwrap().unwrap();
    row["spec"]["url"] = url_b.clone().into();
    row["updated_at"] = (row["updated_at"].as_u64().unwrap() + 1).into();
    h.broker.meta.put(kind::WEBHOOK, &k, &row).await.unwrap();
    h.broker.apply_change(kind::WEBHOOK, &k).await.unwrap();
    let mut seen = Value::Null;
    for _ in 0..100 {
        seen = h.get("/v1/vhosts/%2F/webhooks/local").await.1;
        if seen["url"] == url_b.as_str() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(seen["url"], url_b.as_str(), "{seen}");
    h.post("/v1/vhosts/%2F/queues/remote-hooked/messages", json!({"body": "two"})).await;
    let hit = tokio::time::timeout(Duration::from_secs(5), hits_b.recv()).await.unwrap().unwrap();
    assert!(hit.body.contains("two"));

    // Created elsewhere.
    let k2 = meta::key(&["/", "remote"]);
    row["name"] = "remote".into();
    h.broker.meta.put(kind::WEBHOOK, &k2, &row).await.unwrap();
    h.broker.apply_change(kind::WEBHOOK, &k2).await.unwrap();
    let mut s = StatusCode::NOT_FOUND;
    for _ in 0..100 {
        s = h.get("/v1/vhosts/%2F/webhooks/remote").await.0;
        if s == StatusCode::OK {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(s, StatusCode::OK, "a webhook created on another node starts here");

    // Deleted elsewhere: both stop, so messages stay in the queue.
    for k in [&k, &k2] {
        h.broker.meta.delete(kind::WEBHOOK, k).await.unwrap();
        h.broker.apply_change(kind::WEBHOOK, k).await.unwrap();
    }
    for _ in 0..100 {
        if h.get("/v1/vhosts/%2F/webhooks").await.1.as_array().is_some_and(|l| l.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h.get("/v1/vhosts/%2F/webhooks").await.1, json!([]));
    h.post("/v1/vhosts/%2F/queues/remote-hooked/messages", json!({"body": "three"})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.depth("remote-hooked").await, (1, 0));
    assert!(hits_a.try_recv().is_err() && hits_b.try_recv().is_err());
}

#[tokio::test]
async fn webhook_dead_letters_after_retries_and_survives_restart() {
    let h = H::start().await;
    h.queue("hook-dlq", json!({})).await;
    h.queue("hook-src", json!({"arguments": {"x-dead-letter-exchange": "", "x-dead-letter-routing-key": "hook-dlq"}})).await;
    let (url, mut hits) = receiver(0, true).await;
    let (s, v) = h.put("/v1/vhosts/%2F/webhooks/failing", json!({"queue": "hook-src", "url": url, "secret": "s3cret", "retry": {"max_attempts": 3, "backoff_ms": 20, "max_backoff_ms": 40}})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    h.post("/v1/vhosts/%2F/queues/hook-src/messages", json!({"body": "doomed"})).await;
    for attempt in 1..=3 {
        let hit = tokio::time::timeout(Duration::from_secs(5), hits.recv()).await.unwrap().unwrap();
        assert_eq!(hit.headers["roll-delivery-attempt"], attempt.to_string());
        assert!(verify("s3cret", &hit));
    }
    h.wait_depth("hook-dlq", (1, 0)).await;
    h.wait_depth("hook-src", (0, 0)).await;
    let (_, st) = h.get("/v1/vhosts/%2F/webhooks/failing").await;
    assert_eq!(st["status"]["failed"], 1, "{st}");
    let dead = h.receive("hook-dlq", json!({})).await;
    assert_eq!(dead[0]["headers"]["x-death"][0]["reason"], "rejected");

    // Webhooks are persisted: a fresh API on the same broker restarts them.
    h.api.shutdown();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let api2 = HttpApi::start(h.broker.clone(), Config::default()).await.unwrap();
    tokio::spawn(mq_http::serve_api(api2, l));
    let mut st = Value::Null;
    for _ in 0..100 {
        st = h.send(h.http.get(format!("{base}/v1/vhosts/%2F/webhooks/failing")).basic_auth("guest", Some("guest"))).await.1;
        if st["status"]["state"] == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(st["status"]["state"], "running", "{st}");
    assert_eq!(st["retry"]["max_attempts"], 3);
}

#[tokio::test]
async fn interop_with_amqp_consumer() {
    use lapin::options::*;
    use lapin::types::{AMQPValue, FieldTable};
    let h = H::start().await;
    h.queue("mixed", json!({"type": "quorum"})).await;
    // Published over AMQP, received over HTTP.
    let conn = lapin::Connection::connect(&h.amqp, lapin::ConnectionProperties::default()).await.unwrap();
    let ch = conn.create_channel().await.unwrap();
    ch.basic_publish("".into(), "mixed".into(), BasicPublishOptions::default(), b"from-amqp", lapin::BasicProperties::default().with_delivery_mode(2))
        .await
        .unwrap()
        .await
        .unwrap();
    let m = h.receive("mixed", json!({"wait_ms": 2000, "visibility_ms": 300})).await;
    assert_eq!(m[0]["body"], "from-amqp");
    // The lease expires and an AMQP consumer gets it, counted as a redelivery.
    ch.basic_qos(10, BasicQosOptions::default()).await.unwrap();
    let mut consumer = ch.basic_consume("mixed".into(), "c".into(), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();
    let d = tokio::time::timeout(Duration::from_secs(5), consumer.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(d.data, b"from-amqp");
    assert!(d.redelivered);
    let count = d.properties.headers().as_ref().and_then(|h| h.inner().get("x-delivery-count").cloned());
    assert!(matches!(count, Some(AMQPValue::LongLongInt(1)) | Some(AMQPValue::LongInt(1))), "{count:?}");
    d.ack(BasicAckOptions::default()).await.unwrap();
    // The stale HTTP receipt can no longer settle it.
    let (s, _) = h.post("/v1/vhosts/%2F/queues/mixed/ack", json!({"receipts": [m[0]["receipt"]]})).await;
    assert_eq!(s, StatusCode::GONE);
    // Published over HTTP, consumed over AMQP.
    h.post("/v1/vhosts/%2F/queues/mixed/messages", json!({"body": "from-http", "headers": {"h": "v"}})).await;
    let d = tokio::time::timeout(Duration::from_secs(5), consumer.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(d.data, b"from-http");
    assert!(d.properties.message_id().is_some());
    d.ack(BasicAckOptions::default()).await.unwrap();
    h.wait_depth("mixed", (0, 0)).await;
    conn.close(200, "bye".into()).await.unwrap();
}

#[tokio::test]
async fn draining_ends_streams_and_long_polls() {
    let h = H::start().await;
    h.queue("drain", json!({})).await;
    let r = h.http.get(h.url("/v1/vhosts/%2F/queues/drain/events")).basic_auth("guest", Some("guest")).send().await.unwrap();
    let mut ev = Events::new(r);
    ev.next(Duration::from_secs(5)).await.unwrap();
    let poll = {
        let http = h.http.clone();
        let url = h.url("/v1/vhosts/%2F/queues/drain/receive");
        tokio::spawn(async move {
            let t = Instant::now();
            let r = http.post(url).basic_auth("guest", Some("guest")).json(&json!({"wait_ms": 15_000})).send().await.unwrap();
            (r.status(), t.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let b = h.broker.clone();
    tokio::spawn(async move { b.drain(Duration::from_millis(100)).await });
    let (kind, _) = ev.next(Duration::from_secs(5)).await.unwrap();
    assert_eq!(kind, "close");
    let (status, took) = poll.await.unwrap();
    assert_eq!(status, 200);
    assert!(took < Duration::from_secs(5), "{took:?}");
    let r = h.http.get(h.url("/v1/health")).send().await;
    if let Ok(r) = r {
        assert_eq!(r.status(), 503);
    }
}
