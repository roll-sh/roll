//! The roll API end to end: an in-process broker with an AMQP listener, the API on a random
//! port, and an unmodified AMQP client (lapin) alongside.

use mq_control::{ControlConfig, Endpoints, QueuesControl};
use mq_core::log::MemStore;
use mq_core::meta::MemMeta;
use mq_core::{Broker, BrokerConfig};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

struct Api {
    control: Arc<QueuesControl>,
    base: String,
    amqp: String,
    http: reqwest::Client,
}

impl Api {
    async fn start() -> Self {
        let broker = Broker::start(BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let b = broker.clone();
        tokio::spawn(async move {
            let _ = mq_amqp::serve_listener(b, listener, mq_amqp::ServerConfig { listen: addr, amqp10: None }).await;
        });
        let endpoints = Endpoints { amqp: Some(format!("amqp://{addr}")), ..Default::default() };
        let control = QueuesControl::with_config(broker, endpoints, ControlConfig::default()).unwrap();
        let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_addr = api.local_addr().unwrap();
        let router = mq_control::api::router(control.clone());
        tokio::spawn(async move {
            let _ = axum::serve(api, router).await;
        });
        Api { control, base: format!("http://{api_addr}"), amqp: format!("amqp://guest:guest@{addr}/%2f"), http: reqwest::Client::new() }
    }

    async fn get(&self, path: &str) -> Value {
        let r = self.http.get(format!("{}{path}", self.base)).send().await.unwrap();
        assert!(r.status().is_success(), "GET {path}: {}", r.status());
        r.json().await.unwrap()
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Value) -> (u16, Value) {
        let r = self.http.request(method, format!("{}{path}", self.base)).json(&body).send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.send(reqwest::Method::POST, path, body).await;
        assert!((200..300).contains(&s), "POST {path}: {s} {v}");
        v
    }
}

#[tokio::test]
async fn overview_queues_browse_and_replay() {
    let api = Api::start().await;
    let ov = api.get("/v1/queues/overview").await;
    assert_eq!(ov["totals"]["vhosts"], 1);
    assert!(ov["endpoints"]["amqp"].as_str().unwrap().starts_with("amqp://"));

    // A work queue whose messages expire into a DLQ.
    api.post("/v1/queues/vhosts/%2F/queues", json!({ "name": "dlq" })).await;
    api.post(
        "/v1/queues/vhosts/%2F/queues",
        json!({ "name": "work", "type": "quorum", "arguments": { "x-message-ttl": 1, "x-dead-letter-exchange": "", "x-dead-letter-routing-key": "dlq" } }),
    )
    .await;
    api.post("/v1/queues/vhosts/%2F/queues", json!({ "name": "done" })).await;
    for i in 0..3 {
        let r = api.post("/v1/queues/vhosts/%2F/publish", json!({ "exchange": "", "routing_key": "work", "body": format!("job {i}"), "properties": { "content_type": "text/plain" } })).await;
        assert_eq!(r["routed"], true);
    }
    let r = api.post("/v1/queues/vhosts/%2F/publish", json!({ "exchange": "", "routing_key": "nowhere", "body": "x" })).await;
    assert_eq!(r["routed"], false);

    let mut dlq = Value::Null;
    for _ in 0..100 {
        dlq = api.get("/v1/queues/vhosts/%2F/queues/dlq").await;
        if dlq["messages_ready"] == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(dlq["messages_ready"], 3, "{dlq}");
    assert_eq!(dlq["is_dlq"], true);
    let queues = api.get("/v1/queues/vhosts/%2F/queues").await;
    let work = queues.as_array().unwrap().iter().find(|q| q["name"] == "work").unwrap();
    assert_eq!(work["type"], "quorum");
    assert_eq!(work["dead_letter_exchange"], "");
    assert_eq!(work["is_dlq"], false);

    let msgs = api.get("/v1/queues/vhosts/%2F/queues/dlq/messages?count=10").await;
    let msgs = msgs.as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["payload"], "job 0");
    assert_eq!(msgs[0]["payload_encoding"], "string");
    assert_eq!(msgs[0]["properties"]["content_type"], "text/plain");
    assert_eq!(msgs[0]["x_death"][0]["queue"], "work");
    assert_eq!(msgs[0]["x_death"][0]["reason"], "expired");
    // Browsing doesn't consume.
    assert_eq!(api.get("/v1/queues/vhosts/%2F/queues/dlq").await["messages_ready"], 3);

    let r = api.post("/v1/queues/vhosts/%2F/queues/dlq/replay", json!({ "count": 2, "target_routing_key": "done" })).await;
    assert_eq!(r["replayed"], 2);
    assert_eq!(api.get("/v1/queues/vhosts/%2F/queues/done").await["messages_ready"], 2);
    assert_eq!(api.get("/v1/queues/vhosts/%2F/queues/dlq").await["messages_ready"], 1);

    let r = api.post("/v1/queues/vhosts/%2F/queues/done/purge", json!({})).await;
    assert_eq!(r["purged"], 2);

    // Rates and usage after a sample.
    api.control.sampler.sample(&api.control.broker);
    let rates = api.get("/v1/queues/rates?seconds=60").await;
    assert_eq!(rates["step_s"], 5);
    assert!(!rates["points"].as_array().unwrap().is_empty());
    let usage = api.get("/v1/queues/usage?days=7").await;
    assert!(usage["compare"].as_array().unwrap().len() >= 2);
    assert!(usage["pricing"]["usd_per_million_messages"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn topology_policies_users_connections() {
    let api = Api::start().await;
    let v = api.post("/v1/queues/vhosts", json!({ "name": "shop", "default_queue_type": "quorum" })).await;
    assert_eq!(v["state"], "running");
    assert!(v["amqp_url"].as_str().unwrap().ends_with("/shop"));
    let (s, e) = api.send(reqwest::Method::POST, "/v1/queues/vhosts", json!({ "name": "shop" })).await;
    assert_eq!(s, 409, "{e}");
    assert!(e["error"].is_string());

    api.post("/v1/queues/vhosts/shop/exchanges", json!({ "name": "orders", "type": "topic" })).await;
    api.post("/v1/queues/vhosts/shop/queues", json!({ "name": "eu" })).await;
    api.post("/v1/queues/vhosts/shop/bindings", json!({ "source": "orders", "destination": "eu", "routing_key": "eu.#" })).await;
    let q = api.get("/v1/queues/vhosts/shop/queues/eu").await;
    assert_eq!(q["type"], "quorum", "vhost default queue type applies");
    assert!(q["bindings"].as_array().unwrap().iter().any(|b| b["source"] == "orders" && b["routing_key"] == "eu.#"));
    let r = api.post("/v1/queues/vhosts/shop/publish", json!({ "exchange": "orders", "routing_key": "eu.de", "body": "aGk=", "body_encoding": "base64" })).await;
    assert_eq!(r["routed"], true);
    let xs = api.get("/v1/queues/vhosts/shop/exchanges").await;
    assert!(xs.as_array().unwrap().iter().any(|x| x["name"] == "orders" && x["bindings"] == 1));
    let (s, _) = api.send(reqwest::Method::DELETE, "/v1/queues/vhosts/shop/bindings", json!({ "source": "orders", "destination": "eu", "routing_key": "eu.#" })).await;
    assert_eq!(s, 204);
    let (s, e) = api.send(reqwest::Method::GET, "/v1/queues/vhosts/shop/queues/missing", json!({})).await;
    assert_eq!(s, 404, "{e}");

    let (s, p) = api.send(reqwest::Method::PUT, "/v1/queues/policies/shop/limits", json!({ "pattern": "^eu$", "apply_to": "queues", "definition": { "max-length": 10 }, "priority": 1 })).await;
    assert_eq!(s, 200, "{p}");
    assert_eq!(api.get("/v1/queues/vhosts/shop/queues/eu").await["policy"], "limits");
    let (s, _) = api.send(reqwest::Method::PUT, "/v1/queues/policies/shop/bad", json!({ "pattern": ".*", "definition": { "nonsense": 1 } })).await;
    assert_eq!(s, 400);

    let (s, _) = api.send(reqwest::Method::PUT, "/v1/queues/users/alice", json!({ "password": "s3cret", "tags": ["monitoring"] })).await;
    assert_eq!(s, 200);
    let (s, _) = api.send(reqwest::Method::PUT, "/v1/queues/permissions/shop/alice", json!({ "configure": "", "write": "^orders$", "read": ".*" })).await;
    assert_eq!(s, 200);
    let users = api.get("/v1/queues/users").await;
    let alice = users.as_array().unwrap().iter().find(|u| u["name"] == "alice").unwrap();
    assert_eq!(alice["permissions"][0]["write"], "^orders$");

    // A live AMQP connection shows up and can be closed.
    let conn = lapin::Connection::connect(&api.amqp, lapin::ConnectionProperties::default()).await.unwrap();
    let conns = api.get("/v1/queues/connections").await;
    let c = &conns.as_array().unwrap()[0];
    assert_eq!(c["protocol"], "AMQP 0-9-1");
    let id = c["id"].as_u64().unwrap();
    let (s, _) = api.send(reqwest::Method::DELETE, &format!("/v1/queues/connections/{id}"), json!({})).await;
    assert_eq!(s, 204);
    for _ in 0..100 {
        if !conn.status().connected() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!conn.status().connected());

    let defs = api.get("/v1/queues/definitions").await;
    assert!(defs["queues"].as_array().unwrap().iter().any(|q| q["name"] == "eu"));

    // Scale to zero and back.
    let r = api.post("/v1/queues/vhosts/shop/suspend", json!({})).await;
    assert_eq!(r["state"], "sleeping");
    let vs = api.get("/v1/queues/vhosts").await;
    let shop = vs.as_array().unwrap().iter().find(|v| v["name"] == "shop").unwrap().clone();
    assert_eq!((shop["state"].as_str(), shop["sleeping"].as_bool()), (Some("sleeping"), Some(true)));
    assert_eq!((shop["queues"].as_u64(), shop["messages_ready"].as_u64()), (Some(1), Some(1)), "last-known totals while asleep: {shop}");
    let ov = api.get("/v1/queues/overview").await;
    assert_eq!(ov["totals"]["sleeping_vhosts"], 1);
    assert_eq!((ov["totals"]["queues"].as_u64(), ov["totals"]["messages_ready"].as_u64()), (Some(1), Some(1)), "{ov}");
    let q = api.get("/v1/queues/vhosts/shop/queues/eu").await;
    assert_eq!(q["messages_ready"], 1, "state survives sleep");

    let (s, _) = api.send(reqwest::Method::DELETE, "/v1/queues/vhosts/shop", json!({})).await;
    assert_eq!(s, 204);
    let (s, _) = api.send(reqwest::Method::POST, "/v1/queues/migrate", json!({ "source": { "management_url": "http://x" } })).await;
    assert_eq!(s, 501);
    let (s, _) = api.send(reqwest::Method::DELETE, "/v1/queues/migrate", json!({})).await;
    assert_eq!(s, 501);
}

#[tokio::test]
async fn migration_hooks() {
    let api = Api::start().await;
    let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n = stops.clone();
    api.control.set_migrator(mq_control::Migrator {
        start: Arc::new(|body| Box::pin(async move { Ok(json!({ "imported": {}, "shovels": [], "skipped": [], "echo": body })) })),
        status: Arc::new(|| json!({ "shovels": [{ "name": "roll-migrate-q", "state": "running", "remaining": 0 }] })),
        stop: Arc::new(move || {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(json!({ "stopped": ["roll-migrate-q"] })) })
        }),
    });
    let v = api.post("/v1/queues/migrate", json!({ "source": { "management_url": "http://x", "amqp_uri": "amqp://x:1" }, "include_shovels": true })).await;
    assert_eq!(v["echo"]["source"]["amqp_uri"], "amqp://x:1");
    assert_eq!(api.get("/v1/queues/migrate").await["shovels"][0]["name"], "roll-migrate-q");
    let (s, v) = api.send(reqwest::Method::DELETE, "/v1/queues/migrate", json!({})).await;
    assert_eq!((s, v), (200, json!({ "stopped": ["roll-migrate-q"] })));
    assert_eq!(stops.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Definitions imports report what they skipped.
    let defs = json!({ "policies": [{ "vhost": "/", "name": "ha", "pattern": ".*", "definition": { "ha-mode": "all" } }] });
    let v = api.post("/v1/queues/definitions", defs).await;
    assert_eq!(v["skipped"][0]["name"], "ha", "{v}");
}

/// Whether `url` gets an open connection with a working channel.
async fn usable(url: &str) -> bool {
    match lapin::Connection::connect(url, lapin::ConnectionProperties::default()).await {
        Ok(c) => c.create_channel().await.is_ok(),
        Err(_) => false,
    }
}

#[tokio::test]
async fn provisioned_vhosts_come_with_their_own_connection_url() {
    let api = Api::start().await;
    let (s, v) = api.send(reqwest::Method::POST, "/v1/queues/vhosts", json!({ "name": "tenant-a", "credentials": true })).await;
    assert_eq!(s, 201, "{v}");
    let cr = &v["credentials"];
    assert_eq!(cr["user"], "tenant-a");
    assert_eq!(cr["password"].as_str().unwrap().len(), 32);
    assert!(cr["urls"]["mqtt"].is_null(), "only enabled protocols get a URL: {cr}");
    let url = cr["urls"]["amqp"].as_str().unwrap().to_string();
    assert!(url.starts_with("amqp://tenant-a:") && url.ends_with("/tenant-a"), "{url}");

    // An unmodified client needs nothing but the URL.
    let conn = lapin::Connection::connect(&url, lapin::ConnectionProperties::default()).await.unwrap();
    let ch = conn.create_channel().await.unwrap();
    ch.queue_declare("jobs".into(), Default::default(), Default::default()).await.unwrap();
    conn.close(200, "bye".into()).await.unwrap();
    let q = api.get("/v1/queues/vhosts/tenant-a/queues/jobs").await;
    assert_eq!(q["name"], "jobs");

    // The user reaches only its own vhost.
    let other = format!("{}/%2f", url.rsplit_once('/').unwrap().0);
    assert!(!usable(&other).await, "no access to /");

    // Rotating issues a new password and the old one stops working.
    let rotated = api.post("/v1/queues/vhosts/tenant-a/credentials", json!({})).await;
    assert_ne!(rotated["password"], cr["password"]);
    assert!(!usable(&url).await, "old password");
    let fresh = rotated["urls"]["amqp"].as_str().unwrap();
    lapin::Connection::connect(fresh, lapin::ConnectionProperties::default()).await.unwrap().close(200, "bye".into()).await.unwrap();

    // Taken names conflict, and an operator's password is never reissued.
    let (s, _) = api.send(reqwest::Method::POST, "/v1/queues/vhosts", json!({ "name": "tenant-b", "user": "tenant-a" })).await;
    assert_eq!(s, 409);
    assert!(api.get("/v1/queues/vhosts").await.as_array().unwrap().iter().all(|v| v["name"] != "tenant-b"), "nothing half-created");
    let (s, _) = api.send(reqwest::Method::POST, "/v1/queues/vhosts/tenant-a/credentials", json!({ "user": "guest" })).await;
    assert_eq!(s, 409);
    let (s, _) = api.send(reqwest::Method::POST, "/v1/queues/vhosts/missing/credentials", json!({})).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn quiet_brokers_still_put_idle_vhosts_to_sleep_on_time() {
    let config = BrokerConfig { vhost_idle_suspend_ms: 300, ..BrokerConfig::default() };
    let broker = Broker::start(config, MemMeta::new(), MemStore::new()).await.unwrap();
    broker.open_vhost("/").await.unwrap();
    let control = QueuesControl::with_config(broker.clone(), Endpoints::default(), ControlConfig { idle_suspend_ms: 300, ..Default::default() }).unwrap();
    let bg = control.spawn_background();
    assert!(broker.is_quiet());
    // The loop parks while quiet, but wakes for the vhost's deadline instead of ticking.
    let t = std::time::Instant::now();
    while !broker.is_asleep("/") {
        assert!(t.elapsed() < Duration::from_secs(10), "the idle vhost was never put to sleep");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bg.abort();
}
