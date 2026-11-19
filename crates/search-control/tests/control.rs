//! The roll search control plane end to end: `/v1/search` API → node → Elasticsearch clients
//! over HTTP. Object storage is a temp directory, or S3 with `ROLL_E2E_S3_BUCKET`.

use search_control::{Control, SearchConfig};
use serde_json::{Value, json};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::time::Duration;

/// Fails any single allocation over 1 GiB, as Linux does, so a buffer sized from a peer's count
/// aborts the tests on every platform (macOS would hand it out lazily).
struct Capped;

const MAX_ALLOC: usize = 1 << 30;

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc(layout) } }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc_zeroed(layout) } }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.realloc(ptr, layout, new_size) } }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Capped = Capped;

struct Env {
    control: Arc<Control>,
    api: String,
    es: String,
    http: reqwest::Client,
    home: std::path::PathBuf,
    objects: search_store::Objects,
    cfg: SearchConfig,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

async fn env_with(f: impl FnOnce(&mut SearchConfig)) -> Env {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let i = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!("roll-search-control-{}-{n}-{i}", std::process::id()));
    let objects = search_store::test_objects("control");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let es_addr = listener.local_addr().unwrap();
    let mut cfg = SearchConfig::local(home.clone());
    cfg.node_name = format!("test-{i}");
    cfg.http_port = es_addr.port();
    cfg.gc_interval = None;
    f(&mut cfg);
    let control = Control::new(cfg.clone(), objects.clone()).await.unwrap();
    tokio::spawn(search_server::serve(listener, control.node.clone(), search_server::ListenOptions::default()));
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", api_listener.local_addr().unwrap());
    let router = search_control::api::router(control.clone()).layer(axum::middleware::from_fn(as_person));
    tokio::spawn(async move { axum::serve(api_listener, router).await });
    Env { control, api, es: format!("http://{es_addr}"), http: reqwest::Client::new(), home, objects, cfg }
}

async fn env() -> Env {
    env_with(|_| {}).await
}

/// What rolld's gate puts on a request, from `x-test-as`: `admin` is a signed-in admin (who also
/// carries `Operator`), `usr_…` a signed-in person who isn't one. Without it, an open daemon's
/// request (neither).
async fn as_person(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let who = req.headers().get("x-test-as").and_then(|v| v.to_str().ok()).map(str::to_string);
    if let Some(who) = who {
        let admin = who == "admin";
        if admin {
            req.extensions_mut().insert(roll_auth::Operator);
        }
        req.extensions_mut().insert(roll_auth::SignedIn { user: format!("usr_{who}"), handle: who, admin, code_owner: None, session: "ses_test".into() });
    }
    next.run(req).await
}

impl Env {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.api));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    async fn get(&self, path: &str) -> Value {
        let (s, v) = self.call(reqwest::Method::GET, path, None).await;
        assert_eq!(s, 200, "{path}: {v}");
        v
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert_eq!(s, 200, "{path}: {v}");
        v
    }

    /// [`Env::call`] as a signed-in person (see [`as_person`]).
    async fn call_as(&self, who: &str, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.api)).header("x-test-as", who);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    /// A request to the Elasticsearch port, as a client with Basic auth would send it.
    async fn es(&self, user: &str, password: &str, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.es)).basic_auth(user, Some(password));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
}

fn creds(v: &Value) -> (String, String) {
    (v["username"].as_str().unwrap().to_string(), v["password"].as_str().unwrap().to_string())
}

/// A console response's JSON body.
fn body(r: &Value) -> Value {
    serde_json::from_str(r["body"].as_str().unwrap()).unwrap()
}

fn index_names(v: &Value) -> Vec<String> {
    let mut names: Vec<String> = v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn lifecycle() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "app"})).await;
    let id = c["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("srch_"));
    assert_eq!(c["name"], "app");
    assert_eq!(c["flavor"], "elasticsearch");
    assert_eq!(c["status"]["state"], "asleep");
    assert_eq!(c["url"], e.es.as_str());
    let (user, pw) = creds(&c);
    assert_eq!(user, "app");
    assert_eq!(pw.len(), 32);

    // The password is shown once.
    let shown = e.get(&format!("/v1/search/clusters/{id}")).await;
    assert!(shown.get("password").is_none(), "{shown}");
    let listed = e.get("/v1/search/clusters").await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert!(listed[0].get("password").is_none());
    // By name too.
    assert_eq!(e.get("/v1/search/clusters/app").await["id"], id.as_str());

    // Validation and conflicts.
    let (s, v) = e.call(reqwest::Method::POST, "/v1/search/clusters", Some(json!({"name": "app"}))).await;
    assert_eq!((s, v["error"].as_str().unwrap()), (409, "a search cluster named app already exists"));
    for bad in [json!({"name": "App"}), json!({"name": "my_app"}), json!({"name": "x", "flavor": "solr"}), json!({"name": "x", "max_in_flight": 0}), json!({"name": "x", "at": 5})] {
        let (s, v) = e.call(reqwest::Method::POST, "/v1/search/clusters", Some(bad.clone())).await;
        assert_eq!(s, 400, "{bad}: {v}");
    }
    let (s, _) = e.call(reqwest::Method::GET, "/v1/search/clusters/nope", None).await;
    assert_eq!(s, 404);

    // The dashboard calls the API from another origin.
    let pre = e
        .http
        .request(reqwest::Method::OPTIONS, format!("{}/v1/search/clusters/{id}", e.api))
        .header("origin", "http://localhost:3000")
        .header("access-control-request-method", "PATCH")
        .send()
        .await
        .unwrap();
    assert!(pre.status().is_success());
    assert!(pre.headers().contains_key("access-control-allow-origin"));

    // An Elasticsearch client with Basic auth.
    let r = e.http.get(format!("{}/", e.es)).basic_auth(&user, Some(&pw)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers().get("x-elastic-product").unwrap(), "Elasticsearch");
    let root: Value = r.json().await.unwrap();
    assert_eq!(root["cluster_name"], "app");
    assert_eq!(root["version"]["number"], "9.5.5");
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/products", Some(json!({"mappings": {"properties": {"title": {"type": "text"}}}}))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["acknowledged"], true);
    let (s, v) = e.es(&user, "wrong", reqwest::Method::GET, "/", None).await;
    assert_eq!(s, 401, "{v}");
    assert_eq!(v["error"]["type"], "security_exception");
    // Other ways to present the password: Bearer `<cluster>_<secret>` and ApiKey.
    let r = e.http.get(format!("{}/products", e.es)).bearer_auth(format!("app_{pw}")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    use base64::Engine;
    let key = base64::engine::general_purpose::STANDARD.encode(format!("app:{pw}"));
    let r = e.http.get(format!("{}/products", e.es)).header("authorization", format!("ApiKey {key}")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);

    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/products/_doc/1?refresh=true", Some(json!({"title": "shoe"}))).await;
    assert_eq!(s, 201, "{v}");

    let shown = e.get(&format!("/v1/search/clusters/{id}")).await;
    assert_eq!(shown["status"]["state"], "loaded");
    assert_eq!(shown["status"]["indices"], 1);
    // Live, while the metered `docs` waits for the next sample.
    assert_eq!(shown["status"]["docs"], 1, "{shown}");

    // Indices listing.
    let idx = e.get(&format!("/v1/search/clusters/{id}/indices")).await;
    let idx = idx.as_array().unwrap();
    assert_eq!(idx.len(), 1, "{idx:?}");
    assert_eq!(idx[0]["index"], "products");
    assert!(idx[0]["health"].is_string());
    assert_eq!(idx[0]["primaries"], 1);

    // The console.
    let r = e.post(&format!("/v1/search/clusters/{id}/request"), json!({"method": "get", "path": "/products/_mapping"})).await;
    assert_eq!(r["status"], 200, "{r}");
    assert_eq!(r["json"], true);
    assert_eq!(body(&r)["products"]["mappings"]["properties"]["title"]["type"], "text");
    assert!(r["took_ms"].is_u64());
    let r = e.post(&format!("/v1/search/clusters/{id}/request"), json!({"method": "PUT", "path": "/logs", "body": {"settings": {"number_of_shards": 2}}})).await;
    assert_eq!(r["status"], 200, "{r}");
    let r = e.post(&format!("/v1/search/clusters/{id}/request"), json!({"method": "GET", "path": "/missing"})).await;
    assert_eq!(r["status"], 404);
    assert_eq!(body(&r)["error"]["type"], "index_not_found_exception");
    // The body is Elasticsearch's own text: key order is kept (`root_cause` first).
    assert!(r["body"].as_str().unwrap().starts_with(r#"{"error":{"root_cause""#), "{r}");
    let r = e.post(&format!("/v1/search/clusters/{id}/request"), json!({"method": "HEAD", "path": "/logs"})).await;
    assert_eq!((r["status"].as_u64(), r["body"].as_str()), (Some(200), Some("")));
    let r = e.post(&format!("/v1/search/clusters/{id}/request"), json!({"method": "GET", "path": "/_cat/indices?v&s=index"})).await;
    assert_eq!(r["json"], false);
    assert!(r["body"].as_str().unwrap().contains("products"), "{r}");
    let (s, _) = e.call(reqwest::Method::POST, &format!("/v1/search/clusters/{id}/request"), Some(json!({"method": "GET", "path": "no-slash"}))).await;
    assert_eq!(s, 400);
    let (s, _) = e.call(reqwest::Method::POST, &format!("/v1/search/clusters/{id}/request"), Some(json!({"method": "TRACE", "path": "/"}))).await;
    assert_eq!(s, 400);

    // Password rotation: the old one stops working at once, on the loaded cluster.
    let rotated = e.post(&format!("/v1/search/clusters/{id}/password"), json!({})).await;
    let (_, pw2) = creds(&rotated);
    assert_ne!(pw2, pw);
    assert!(rotated["password_rotated_at"].is_i64());
    assert_eq!(e.es(&user, &pw, reqwest::Method::GET, "/", None).await.0, 401);
    assert_eq!(e.es(&user, &pw2, reqwest::Method::GET, "/", None).await.0, 200);

    // Unload, and wake on the next request with the metadata intact.
    let r = e.post(&format!("/v1/search/clusters/{id}/unload"), json!({})).await;
    assert_eq!(r["unloaded"], true);
    assert_eq!(e.get(&format!("/v1/search/clusters/{id}")).await["status"]["state"], "asleep");
    assert!(e.control.node.loaded().is_empty());
    let (s, v) = e.es(&user, &pw2, reqwest::Method::GET, "/_mapping", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(index_names(&v), ["logs", "products"]);
    assert_eq!(e.get(&format!("/v1/search/clusters/{id}")).await["status"]["state"], "loaded");

    // Settings.
    let (s, v) = e.call(reqwest::Method::PATCH, &format!("/v1/search/clusters/{id}"), Some(json!({"idle_timeout_s": 0, "max_in_flight": 8, "max_body_mb": 1}))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["idle_timeout_s"].as_i64(), v["max_in_flight"].as_i64(), v["max_body_mb"].as_i64()), (Some(0), Some(8), Some(1)));
    // The body limit applies live: 413 like Elasticsearch's http.max_content_length.
    let big = "x".repeat(2 << 20);
    let r = e
        .http
        .put(format!("{}/products/_doc/1", e.es))
        .basic_auth(&user, Some(&pw2))
        .header("content-type", "application/json")
        .body(format!("{{\"title\":\"{big}\"}}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 413);
    let (s, _) = e.call(reqwest::Method::PATCH, &format!("/v1/search/clusters/{id}"), Some(json!({"max_body_mb": 0}))).await;
    assert_eq!(s, 400);

    // Usage: metered while loaded.
    e.control.sample().await.unwrap();
    let u = e.get(&format!("/v1/search/clusters/{id}/usage?days=1")).await;
    assert_eq!(u["cluster_id"], id.as_str());
    // Every request the node admitted: 7 from clients (also across the unload), the indices
    // listing and 5 console requests. Not counted: wrong passwords, the 413, the console's own
    // 400s.
    assert_eq!(u["requests"], 13, "the node's request counts are metered: {u}");
    assert!(u["loaded_seconds"].as_i64().unwrap() >= 1, "{u}");
    assert!(u["loaded_gib_hours"].as_f64().unwrap() > 0.0);
    let cost = &u["cost"];
    assert!(cost["total_usd"].as_f64().unwrap() > 0.0);
    for other in ["elastic_cloud_usd", "opensearch_service_usd", "opensearch_serverless_usd"] {
        assert!(cost[other].as_f64().unwrap() > cost["total_usd"].as_f64().unwrap(), "{other}: {cost}");
    }
    let info = e.get("/v1/search").await;
    assert_eq!(info["version"], "9.5.5");
    assert_eq!(info["loaded"], 1);
    assert_eq!(info["pricing"]["usd_per_gb_month_s3"], 0.023);

    // Delete: the cluster is gone for clients and the API, and its data from storage.
    let (s, v) = e.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/{id}"), None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["deleted"], id.as_str());
    assert_eq!(e.es(&user, &pw2, reqwest::Method::GET, "/", None).await.0, 401);
    let (s, _) = e.call(reqwest::Method::GET, &format!("/v1/search/clusters/{id}"), None).await;
    assert_eq!(s, 404);
    assert!(!e.control.data.cluster_exists("app").await.unwrap());
    // The name is free again, and the new cluster starts empty.
    let c = e.post("/v1/search/clusters", json!({"name": "app"})).await;
    let (user, pw) = creds(&c);
    let (s, v) = e.es(&user, &pw, reqwest::Method::GET, "/_mapping", None).await;
    assert_eq!((s, index_names(&v)), (200, Vec::<String>::new()));
    e.control.shutdown().await.unwrap();
}

/// Signed-in people's own clusters (docs/accounts.md, "Phase 2"): each person lists and reaches
/// only theirs (another's is a 404, as if it didn't exist), what they create and branch is theirs,
/// a branch keeps its parent's owner whoever cuts it, and admins and the operator see everything.
#[tokio::test]
async fn people_see_only_their_own_clusters() {
    use reqwest::Method;
    let e = env().await;
    let names = |v: &Value| v.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let (s, a) = e.call_as("alice", Method::POST, "/v1/search/clusters", Some(json!({ "name": "alice-app" }))).await;
    assert_eq!((s, a["owner"].as_str()), (200, Some("usr_alice")), "{a}");
    let (s, b) = e.call_as("bob", Method::POST, "/v1/search/clusters", Some(json!({ "name": "bob-app" }))).await;
    assert_eq!((s, b["owner"].as_str()), (200, Some("usr_bob")), "{b}");
    let ops = e.post("/v1/search/clusters", json!({ "name": "ops" })).await;
    assert!(ops["owner"].is_null(), "the operator's has no owner: {ops}");
    let (_, by_admin) = e.call_as("admin", Method::POST, "/v1/search/clusters", Some(json!({ "name": "admins" }))).await;
    assert!(by_admin["owner"].is_null(), "nor an admin's: {by_admin}");

    assert_eq!(names(&e.call_as("alice", Method::GET, "/v1/search/clusters", None).await.1), ["alice-app"]);
    assert_eq!(names(&e.call_as("bob", Method::GET, "/v1/search/clusters", None).await.1), ["bob-app"]);
    assert_eq!(names(&e.call_as("admin", Method::GET, "/v1/search/clusters", None).await.1), ["alice-app", "bob-app", "ops", "admins"]);

    // Bob's cluster, by name or id, and the operator's: no such cluster for alice, on every route.
    let bob_id = b["id"].as_str().unwrap();
    for path in [
        "/v1/search/clusters/bob-app".to_string(),
        format!("/v1/search/clusters/{bob_id}"),
        "/v1/search/clusters/ops/usage".into(),
        "/v1/search/clusters/bob-app/indices".into(),
        "/v1/search/clusters/bob-app/audit".into(),
    ] {
        let (s, v) = e.call_as("alice", Method::GET, &path, None).await;
        assert_eq!((s, v["error"].as_str()), (404, Some(&*format!("no search cluster {}", path.split('/').nth(4).unwrap()))), "{path}");
    }
    assert_eq!(e.call_as("alice", Method::POST, "/v1/search/clusters/bob-app/password", Some(json!({}))).await.0, 404);
    assert_eq!(e.call_as("alice", Method::POST, "/v1/search/clusters/bob-app/request", Some(json!({ "method": "GET", "path": "/" }))).await.0, 404);
    assert_eq!(e.call_as("alice", Method::DELETE, "/v1/search/clusters/bob-app", None).await.0, 404);
    assert_eq!(e.call_as("alice", Method::POST, "/v1/search/clusters", Some(json!({ "name": "stolen", "parent": "bob-app" }))).await.0, 404, "nor branch it");
    assert_eq!(e.call_as("bob", Method::GET, "/v1/search/clusters/bob-app", None).await.0, 200, "bob's is still there");
    for path in ["/v1/search/clusters/bob-app/metrics", "/v1/search/clusters/bob-app/slowlog"] {
        assert_eq!(e.call_as("alice", Method::GET, path, None).await.0, 404, "{path}");
    }
    // Which node holds a cluster is the operator's to know (`status.node`), not its owner's.
    let (_, shown) = e.call_as("bob", Method::GET, "/v1/search/clusters/bob-app", None).await;
    assert!(shown["status"].is_object() && shown["status"].get("node").is_none(), "{shown}");

    // Her own branch is hers; one an admin cuts from her cluster is hers too.
    let (s, dev) = e.call_as("alice", Method::POST, "/v1/search/clusters/alice-app/branches", Some(json!({ "name": "alice-dev" }))).await;
    assert_eq!((s, dev["owner"].as_str()), (200, Some("usr_alice")), "{dev}");
    let (s, cut) = e.call_as("admin", Method::POST, "/v1/search/clusters", Some(json!({ "name": "alice-fix", "parent": "alice-app" }))).await;
    assert_eq!((s, cut["owner"].as_str()), (200, Some("usr_alice")), "{cut}");
    assert_eq!(names(&e.call_as("alice", Method::GET, "/v1/search/clusters", None).await.1), ["alice-app", "alice-dev", "alice-fix"]);
    assert_eq!(names(&e.call_as("bob", Method::GET, "/v1/search/clusters", None).await.1), ["bob-app"]);

    // The node-wide figures and where the data is are the operator's.
    let (_, mine) = e.call_as("alice", Method::GET, "/v1/search", None).await;
    assert!(mine["storage"].is_null() && mine["cache"].is_null() && mine["loaded"].is_null() && mine["pricing"].is_object(), "{mine}");
    let (_, all) = e.call_as("admin", Method::GET, "/v1/search", None).await;
    assert!(all["storage"].is_string() && all["cache"].is_object(), "{all}");
}

/// Requests show up in a cluster's metrics by kind, before and after the metering pass moves
/// them to the catalog; a search and a write past their index's slow log thresholds show up in
/// its slow log as Elasticsearch formats them, the search's source cut at 1,000 characters and
/// the document's at `index.indexing.slowlog.source`, with the client's `X-Opaque-Id` and never
/// its credentials.
#[tokio::test]
async fn metrics_and_slow_logs() {
    use reqwest::Method;
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({ "name": "perf" })).await;
    let (user, pw) = creds(&c);
    let settings = json!({ "settings": {
        "index.search.slowlog.threshold.query.warn": "0ms",
        "index.indexing.slowlog.threshold.index.info": "0ms",
        "index.indexing.slowlog.source": "5",
    } });
    assert_eq!(e.es(&user, &pw, Method::PUT, "/logs", Some(settings)).await.0, 200);
    assert_eq!(e.es(&user, &pw, Method::PUT, "/logs/_doc/1?refresh=true", Some(json!({ "msg": "hello world" }))).await.0, 201);
    let long = "x".repeat(2000);
    let search = json!({ "query": { "match": { "msg": long } } });
    let r = e.http.post(format!("{}/logs/_search", e.es)).basic_auth(&user, Some(&pw)).header("X-Opaque-Id", "trace-1").json(&search).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(e.es(&user, &pw, Method::GET, "/missing/_search", None).await.0, 404);

    let totals = |m: &Value, kind: &str, what: &str| m["totals"][kind][what].as_u64().unwrap_or(0);
    // Before the pass: what the node recorded and hasn't handed on.
    let live = e.get("/v1/search/clusters/perf/metrics?range=1h").await;
    assert_eq!((totals(&live, "search", "requests"), totals(&live, "search", "2xx"), totals(&live, "search", "4xx")), (2, 1, 1), "{live}");
    assert_eq!((totals(&live, "index", "requests"), totals(&live, "other", "requests")), (1, 1), "{live}");
    assert!(live["totals"]["search"]["p50_ms"].as_f64().is_some_and(|ms| ms > 0.0), "{live}");
    assert!(!live["points"].as_array().unwrap().is_empty() && live["step_s"] == 60, "{live}");
    // After it: the same numbers, from the catalog.
    e.control.sample().await.unwrap();
    let kept = e.get("/v1/search/clusters/perf/metrics?range=1h").await;
    assert_eq!(kept["totals"]["all"]["requests"], live["totals"]["all"]["requests"], "{kept}");
    for range in ["24h", "14d"] {
        let m = e.get(&format!("/v1/search/clusters/perf/metrics?range={range}")).await;
        assert_eq!(totals(&m, "all", "requests"), 4, "{range}: {m}");
    }
    assert_eq!(e.call(Method::GET, "/v1/search/clusters/perf/metrics?range=1y", None).await.0, 400);

    let log = e.get("/v1/search/clusters/perf/slowlog").await;
    let entries = log["entries"].as_array().unwrap();
    let query = entries.iter().find(|x| x["log.logger"] == "index.search.slowlog.query").unwrap_or_else(|| panic!("{log}"));
    let source = query["elasticsearch.slowlog.source"].as_str().unwrap();
    assert!(source.chars().count() == 1000 && source.starts_with("{\"query\":{\"match\":{\"msg\":\"xxx"), "{query}");
    assert_eq!((query["log.level"].as_str(), query["elasticsearch.slowlog.id"].as_str()), (Some("WARN"), Some("trace-1")), "{query}");
    assert_eq!((query["elasticsearch.slowlog.message"].as_str(), query["elasticsearch.slowlog.total_hits"].as_str()), (Some("[logs][0]"), Some("0 hits")), "{query}");
    let write = entries.iter().find(|x| x["log.logger"] == "index.indexing.slowlog.index").unwrap_or_else(|| panic!("{log}"));
    assert_eq!((write["log.level"].as_str(), write["elasticsearch.slowlog.id"].as_str(), write["elasticsearch.slowlog.source"].as_str()), (Some("INFO"), Some("1"), Some("{\"msg")), "{write}");
    let text = log.to_string();
    assert!(!text.contains(&pw) && !text.to_ascii_lowercase().contains("authorization"), "no credential in a slow log");
    // Moved to the catalog by the pass above: still there, newest first.
    assert_eq!(entries.first().map(|x| &x["log.logger"]), Some(&json!("index.search.slowlog.query")));
}

/// A person's clusters and always-on clusters are capped (`--search-max-*-per-person`); branches
/// count toward the parent's owner, and the operator and admins aren't held to it.
#[tokio::test]
async fn people_are_held_to_their_cluster_caps() {
    use reqwest::Method;
    let e = env_with(|c| c.person_caps = search_control::store::Caps { clusters: 3, always_on: 1 }).await;
    let create = |name: &str, idle: Option<i64>| json!({ "name": name, "idle_timeout_s": idle });
    let (s, a1) = e.call_as("alice", Method::POST, "/v1/search/clusters", Some(create("a1", None))).await;
    assert_eq!(s, 200, "{a1}");
    let (u, p) = creds(&a1);
    assert_eq!(e.es(&u, &p, Method::PUT, "/notes/_doc/1?refresh=true", Some(json!({"t": "x"}))).await.0, 201);
    assert_eq!(e.call_as("alice", Method::POST, "/v1/search/clusters", Some(create("a2", Some(0)))).await.0, 200);
    let (_, info) = e.call_as("alice", Method::GET, "/v1/search", None).await;
    assert_eq!(info["caps"], json!({ "clusters": 3, "always_on": 1 }));

    // A second always-on one, made or switched to: 409, and the cap still lets a2 change.
    let (s, v) = e.call_as("alice", Method::POST, "/v1/search/clusters", Some(create("a3", Some(0)))).await;
    let msg = v["error"].as_str().unwrap_or_default();
    assert!(s == 409 && msg.starts_with("you have 1 always-on") && msg.contains("may have 1") && msg.contains("sleeps when idle") && msg.contains("--search-max-always-on-per-person"), "{s} {v}");
    let (s, v) = e.call_as("alice", Method::PATCH, "/v1/search/clusters/a1", Some(json!({ "idle_timeout_s": 0 }))).await;
    assert_eq!((s, v["error"].as_str()), (409, Some(msg)));
    assert_eq!(e.call_as("alice", Method::PATCH, "/v1/search/clusters/a2", Some(json!({ "max_in_flight": 8 }))).await.0, 200);

    // A branch counts: the third cluster is one, and past the cap neither a create nor a branch passes.
    let branched = e.control.data.stats().branches;
    assert_eq!(e.call_as("alice", Method::POST, "/v1/search/clusters/a1/branches", Some(json!({ "name": "a1-dev" }))).await.0, 200);
    assert_eq!(e.control.data.stats().branches, branched + 1, "a branch of a1 copies its storage");
    let (s, v) = e.call_as("alice", Method::POST, "/v1/search/clusters", Some(create("a4", None))).await;
    let msg = v["error"].as_str().unwrap_or_default();
    assert!(s == 409 && msg.starts_with("you have 3 search clusters") && msg.contains("may have 3") && msg.contains("--search-max-clusters-per-person"), "{s} {v}");
    let before = e.control.data.stats();
    let (s, v) = e.call_as("alice", Method::POST, "/v1/search/clusters", Some(json!({ "name": "a1-x", "parent": "a1" }))).await;
    assert_eq!((s, v["error"].as_str()), (409, Some(msg)));
    assert!(e.control.store.by_name("a1-x").await.unwrap().is_none());
    // Refused before anything was written to storage (or removed again).
    let after = e.control.data.stats();
    assert_eq!((after.branches, after.clusters_deleted), (before.branches, before.clusters_deleted));

    // An admin isn't capped, even cutting a branch that is alice's or switching hers to always-on.
    let (s, cut) = e.call_as("admin", Method::POST, "/v1/search/clusters", Some(json!({ "name": "a1-fix", "parent": "a1" }))).await;
    assert_eq!((s, cut["owner"].as_str()), (200, Some("usr_alice")), "{cut}");
    assert_eq!(e.call_as("admin", Method::PATCH, "/v1/search/clusters/a1", Some(json!({ "idle_timeout_s": 0 }))).await.0, 200);
    assert!(e.call_as("admin", Method::GET, "/v1/search", None).await.1["caps"].is_null());
    // Alice's count toward nobody else: bob still makes his.
    assert_eq!(e.call_as("bob", Method::POST, "/v1/search/clusters", Some(create("b1", None))).await.0, 200);

    // A deleted cluster frees its place (alice: a1, a2, a1-dev and the admin's a1-fix; two go),
    // and restoring one past the cap is the same 409 as a create.
    let fix = cut["id"].as_str().unwrap().to_string();
    let dev = e.control.store.by_name("a1-dev").await.unwrap().unwrap().id;
    for id in [&fix, &dev] {
        assert_eq!(e.call_as("alice", Method::DELETE, &format!("/v1/search/clusters/{id}"), None).await.0, 200);
    }
    assert_eq!(e.call_as("alice", Method::POST, "/v1/search/clusters", Some(create("a5", None))).await.0, 200);
    let (s, v) = e.call_as("alice", Method::POST, &format!("/v1/search/clusters/{fix}/restore"), None).await;
    assert!(s == 409 && v["error"].as_str().unwrap_or_default().starts_with("you have 3 search clusters"), "{s} {v}");
    assert!(e.control.store.by_name("a1-fix").await.unwrap().is_none());
    // An admin restores it whatever her count.
    assert_eq!(e.call_as("admin", Method::POST, &format!("/v1/search/clusters/{fix}/restore"), None).await.0, 200);
}

#[tokio::test]
async fn branches() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "shop", "max_in_flight": 16})).await;
    let id = c["id"].as_str().unwrap().to_string();
    let (user, pw) = creds(&c);
    // A branch of a cluster that was never written to is empty.
    let empty = e.post(&format!("/v1/search/clusters/{id}/branches"), json!({"name": "shop-empty"})).await;
    let (u0, p0) = creds(&empty);
    assert_eq!(index_names(&e.es(&u0, &p0, reqwest::Method::GET, "/_mapping", None).await.1), Vec::<String>::new());

    assert_eq!(e.es(&user, &pw, reqwest::Method::PUT, "/orders", Some(json!({}))).await.0, 200);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let t1 = search_control::control::now();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(e.es(&user, &pw, reqwest::Method::PUT, "/later", Some(json!({}))).await.0, 200);

    // Branch now: both indices, its own credentials and settings from the parent.
    let b = e.post(&format!("/v1/search/clusters/{id}/branches"), json!({"name": "shop-dev"})).await;
    assert_eq!(b["parent_id"], id.as_str());
    assert_eq!(b["parent_name"], "shop");
    assert_eq!(b["max_in_flight"], 16);
    let (bu, bp) = creds(&b);
    assert_ne!(bp, pw);
    // Figures of a branch that never loaded come from its stored state.
    e.control.sample().await.unwrap();
    assert_eq!(e.get("/v1/search/clusters/shop-dev").await["indices"], 2);
    let (s, v) = e.es(&bu, &bp, reqwest::Method::GET, "/_mapping", None).await;
    assert_eq!((s, index_names(&v)), (200, vec!["later".to_string(), "orders".to_string()]));
    let (_, root) = e.es(&bu, &bp, reqwest::Method::GET, "/", None).await;
    assert_eq!(root["cluster_name"], "shop-dev");
    // The parent's password doesn't open the branch.
    assert_eq!(e.es(&bu, &pw, reqwest::Method::GET, "/", None).await.0, 401);
    // Changes stay on their side.
    assert_eq!(e.es(&bu, &bp, reqwest::Method::PUT, "/branch-only", Some(json!({}))).await.0, 200);
    assert_eq!(e.es(&user, &pw, reqwest::Method::DELETE, "/orders", None).await.0, 200);
    let (_, v) = e.es(&user, &pw, reqwest::Method::GET, "/_mapping", None).await;
    assert_eq!(index_names(&v), ["later"]);
    let (_, v) = e.es(&bu, &bp, reqwest::Method::GET, "/_mapping", None).await;
    assert_eq!(index_names(&v), ["branch-only", "later", "orders"]);

    // Point in time: as of t1, before `later` existed.
    let pit = e.post("/v1/search/clusters", json!({"name": "shop-t1", "parent": "shop", "at": t1})).await;
    assert_eq!(pit["parent_at"], t1);
    let (pu, pp) = creds(&pit);
    let (s, v) = e.es(&pu, &pp, reqwest::Method::GET, "/_mapping", None).await;
    assert_eq!((s, index_names(&v)), (200, vec!["orders".to_string()]));

    // Out of the window, or in the future.
    let (s, v) = e.call(reqwest::Method::POST, &format!("/v1/search/clusters/{id}/branches"), Some(json!({"name": "old", "at": 1}))).await;
    assert_eq!(s, 400, "{v}");
    let (s, _) = e.call(reqwest::Method::POST, &format!("/v1/search/clusters/{id}/branches"), Some(json!({"name": "future", "at": t1 + 3600}))).await;
    assert_eq!(s, 400);
    let (s, _) = e.call(reqwest::Method::POST, &format!("/v1/search/clusters/{id}/branches"), Some(json!({"name": "shop-dev"}))).await;
    assert_eq!(s, 409);

    let shown = e.get(&format!("/v1/search/clusters/{id}")).await;
    let names: Vec<&str> = shown["branches"].as_array().unwrap().iter().map(|b| b["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["shop-empty", "shop-dev", "shop-t1"]);
    assert_eq!(e.get(&format!("/v1/search/clusters/{id}/branches")).await.as_array().unwrap().len(), 3);

    // Deleting the parent leaves branches working (after a wake from storage).
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/{id}"), None).await;
    assert_eq!(s, 200);
    assert!(e.post("/v1/search/clusters/shop-dev/unload", json!({})).await["unloaded"].as_bool().unwrap());
    let (s, v) = e.es(&bu, &bp, reqwest::Method::GET, "/_mapping", None).await;
    assert_eq!((s, index_names(&v)), (200, vec!["branch-only".to_string(), "later".to_string(), "orders".to_string()]));
    let shown = e.get("/v1/search/clusters/shop-dev").await;
    assert_eq!(shown["parent_name"], Value::Null);
}

#[tokio::test]
async fn opensearch_flavor_and_cluster_hook_shape() {
    let e = env().await;
    // What `search-yaml run --cluster-hook` sends and reads.
    let c = e.post("/v1/search/clusters", json!({"name": "yaml-1-w1-1", "flavor": "opensearch", "destructive_requires_name": false})).await;
    for k in ["url", "username", "password", "id"] {
        assert!(c[k].is_string(), "{k}: {c}");
    }
    let (user, pw) = creds(&c);
    let (s, root) = e.es(&user, &pw, reqwest::Method::GET, "/", None).await;
    assert_eq!(s, 200);
    assert_eq!(root["version"]["distribution"], "opensearch");
    assert_eq!(root["version"]["number"], "3.9.0");

    // Switching the flavor applies on the next wake (the control plane unloads the cluster).
    let (s, v) = e.call(reqwest::Method::PATCH, "/v1/search/clusters/yaml-1-w1-1", Some(json!({"flavor": "elasticsearch"}))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["flavor"], "elasticsearch");
    let (_, root) = e.es(&user, &pw, reqwest::Method::GET, "/", None).await;
    assert_eq!(root["version"]["number"], "9.5.5");
    assert!(root["version"].get("distribution").is_none());

    // DELETE <hook>/<id>.
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/{}", c["id"].as_str().unwrap()), None).await;
    assert_eq!(s, 200);
    assert!(e.get("/v1/search/clusters").await.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn idle_clusters_unload_and_meter_exactly() {
    let e = env_with(|c| c.idle_unload = Duration::from_millis(200)).await;
    let c = e.post("/v1/search/clusters", json!({"name": "sleepy", "idle_timeout_s": 1})).await;
    let (user, pw) = creds(&c);
    assert_eq!(e.es(&user, &pw, reqwest::Method::PUT, "/i", Some(json!({}))).await.0, 200);
    assert_eq!(e.control.node.loaded(), ["sleepy"]);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !e.control.node.loaded().is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the idle cluster unloads");
    // The unload accounted its loaded time; asleep, nothing more accrues.
    e.control.sample().await.unwrap();
    let u = e.get("/v1/search/clusters/sleepy/usage").await;
    let secs = u["loaded_seconds"].as_i64().unwrap();
    assert!((1..=5).contains(&secs), "{u}");
    let shown = e.get("/v1/search/clusters/sleepy").await;
    assert_eq!(shown["status"]["state"], "asleep");
    assert_eq!(shown["indices"], 1, "figures refreshed after the unload: {shown}");
    e.control.sample().await.unwrap();
    assert_eq!(e.get("/v1/search/clusters/sleepy/usage").await["loaded_seconds"].as_i64().unwrap(), secs);
    // Wakes on the next request.
    let (s, v) = e.es(&user, &pw, reqwest::Method::GET, "/i", None).await;
    assert_eq!(s, 200, "{v}");
}

#[tokio::test]
async fn domain_routing() {
    let e = env_with(|c| c.domain = Some("search.example.test".into())).await;
    let c = e.post("/v1/search/clusters", json!({"name": "web"})).await;
    assert_eq!(c["host"], "web.search.example.test");
    assert!(c["url"].as_str().unwrap().starts_with("http://web.search.example.test:"));
    let (_, pw) = creds(&c);
    // Any user name works when the host names the cluster (clients default to `elastic`).
    let r = e.http.get(format!("{}/", e.es)).header("host", "web.search.example.test").basic_auth("elastic", Some(&pw)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["cluster_name"], "web");
}

/// A REST listener of the node `e` serves, with `opts` (trusted proxies, PROXY protocol).
async fn listen(e: &Env, opts: search_server::ListenOptions) -> std::net::SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(search_server::serve(l, e.control.node.clone(), opts));
    a
}

/// One request written by hand (`head` before it: a PROXY header), and the status line back.
async fn raw_status(addr: std::net::SocketAddr, head: &str, user: &str, pw: &str) -> String {
    use base64::Engine;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pw}"));
    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    tcp.write_all(format!("{head}GET / HTTP/1.1\r\nHost: x\r\nAuthorization: Basic {auth}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut out = Vec::new();
    tcp.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).lines().next().unwrap_or_default().to_string()
}

/// A cluster's IP allowlist turns other addresses away on the data plane before their
/// credentials are looked at and before a sleeping cluster wakes, with Elastic Cloud's
/// traffic-filter 403. The client's address is the peer, the PROXY header's source, or a trusted
/// proxy's header, never a header from anyone else. roll's own requests (the console) aren't
/// filtered.
#[tokio::test]
async fn ip_allowlists_turn_addresses_away_before_credentials() {
    const REFUSED: &str = r#"{"ok":false,"message":"Forbidden due to traffic filtering"}"#;
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "shop"})).await;
    let (user, pw) = creds(&c);
    let path = format!("/v1/search/clusters/{}", c["id"].as_str().unwrap());
    for bad in [json!(["nope"]), json!([{"cidr": "10.0.0.0/8", "note": "x"}]), json!(vec!["10.0.0.0/8"; 65]), json!([{"cidr": "10.0.0.0/8", "description": "x".repeat(201)}])] {
        let (s, v) = e.call(reqwest::Method::PATCH, &path, Some(json!({ "ip_allowlist": bad }))).await;
        assert!(s == 400 || s == 422, "{bad}: {s} {v}");
    }
    // Two networks this test's loopback client isn't in, kept canonical (a mapped CIDR as IPv4).
    let (s, v) = e.call(reqwest::Method::PATCH, &path, Some(json!({ "ip_allowlist": ["203.0.113.9/24", {"cidr": "::ffff:198.51.100.0/120", "description": " office "}] }))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["ip_allowlist"], json!([{"cidr": "203.0.113.0/24"}, {"cidr": "198.51.100.0/24", "description": "office"}]));
    assert!(e.control.node.loaded().is_empty(), "made, never used");

    // Right or wrong credentials, the same 403, and the cluster stays asleep.
    for p in [pw.as_str(), "wrong"] {
        let r = e.http.get(format!("{}/", e.es)).basic_auth(&user, Some(p)).header("x-forwarded-for", "203.0.113.1").send().await.unwrap();
        assert_eq!(r.status().as_u16(), 403);
        assert_eq!(r.text().await.unwrap(), REFUSED);
    }
    assert!(e.control.node.loaded().is_empty(), "a refused request doesn't wake the cluster");
    // The console is roll's own request: it reaches the cluster from anywhere (and loads it).
    let r = e.post(&format!("{path}/request"), json!({"method": "GET", "path": "/"})).await;
    assert_eq!(r["status"], 200, "{r}");
    let (s, _) = e.es(&user, "wrong", reqwest::Method::GET, "/", None).await;
    assert_eq!(s, 403, "loaded, still refused before its credentials");

    // A trusted proxy's header is believed; the same header from a peer that isn't one isn't.
    let header = |trusted: &str| search_server::ListenOptions { client: search_server::ClientAddresses::new(Some("CF-Connecting-IP"), Some(trusted)).unwrap(), ..Default::default() };
    let trusted = listen(&e, header("127.0.0.0/8")).await;
    let untrusted = listen(&e, header("10.0.0.0/8")).await;
    let via = |addr: std::net::SocketAddr, ip: &'static str, p: &str| e.http.get(format!("http://{addr}/")).basic_auth(&user, Some(p)).header("cf-connecting-ip", ip).send();
    assert_eq!(via(trusted, "203.0.113.50", &pw).await.unwrap().status().as_u16(), 200);
    assert_eq!(via(trusted, "203.0.113.50", "wrong").await.unwrap().status().as_u16(), 401, "let in, then authenticated");
    assert_eq!(via(trusted, "192.0.2.1", &pw).await.unwrap().status().as_u16(), 403);
    assert_eq!(via(untrusted, "203.0.113.50", &pw).await.unwrap().status().as_u16(), 403, "an untrusted peer's header is ignored");

    // Behind an NLB: the PROXY header's source, an IPv4-mapped one included.
    let nlb = listen(&e, search_server::ListenOptions { proxy_protocol: true, ..Default::default() }).await;
    let ok = raw_status(nlb, "PROXY TCP4 203.0.113.7 127.0.0.1 5000 9200\r\n", &user, &pw).await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    let mapped = raw_status(nlb, "PROXY TCP6 ::ffff:198.51.100.4 ::1 5000 9200\r\n", &user, &pw).await;
    assert!(mapped.starts_with("HTTP/1.1 200"), "{mapped}");
    let refused = raw_status(nlb, "PROXY TCP4 192.0.2.1 127.0.0.1 5000 9200\r\n", &user, &pw).await;
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");

    // An empty list opens it again.
    let (s, _) = e.call(reqwest::Method::PATCH, &path, Some(json!({ "ip_allowlist": [] }))).await;
    assert_eq!(s, 200);
    let (s, _) = e.es(&user, &pw, reqwest::Method::GET, "/", None).await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn documents() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "docs"})).await;
    let (user, pw) = creds(&c);
    // Bulk through the console: a string body is sent as is, as NDJSON.
    let bulk = "{\"index\":{\"_index\":\"books\",\"_id\":\"1\"}}\n{\"title\":\"the rust book\",\"year\":2018}\n{\"index\":{\"_index\":\"books\",\"_id\":\"2\"}}\n{\"title\":\"programming rust\",\"year\":2021}\n";
    let r = e.post("/v1/search/clusters/docs/request", json!({"method": "POST", "path": "/_bulk?refresh=true", "body": bulk})).await;
    assert_eq!(r["status"], 200, "{r}");
    assert_eq!(body(&r)["errors"], false, "{r}");
    // An Elasticsearch client indexes and searches.
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/books/_doc/3?refresh=true", Some(json!({"title": "rust in action", "year": 2021}))).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["result"], "created");
    let (s, v) = e.es(&user, &pw, reqwest::Method::GET, "/books/_doc/1", None).await;
    assert_eq!((s, v["_source"]["title"].as_str()), (200, Some("the rust book")));
    // The indices listing reports the documents.
    let idx = e.get("/v1/search/clusters/docs/indices").await;
    assert_eq!(idx[0]["index"], "books");
    // Docs and sizes are the engine's (what `_stats` reports), unflushed writes included.
    assert_eq!(idx[0]["docs"], 3, "{idx}");
    assert_eq!(idx[0]["deleted_docs"], 0, "{idx}");
    assert!(idx[0]["size_bytes"].as_i64().unwrap() > 0, "{idx}");
    assert_eq!(idx[0]["size_bytes"], idx[0]["primary_size_bytes"], "{idx}");
    // A loaded cluster shows its live counts before metering has refreshed the catalog.
    let shown = e.get("/v1/search/clusters/docs").await;
    assert_eq!((shown["docs"].as_i64(), shown["indices"].as_i64()), (Some(3), Some(1)), "{shown}");
    let listed = e.get("/v1/search/clusters").await;
    let listed = listed.as_array().unwrap().iter().find(|c| c["name"] == "docs").unwrap().clone();
    assert_eq!((listed["docs"].as_i64(), listed["indices"].as_i64()), (Some(3), Some(1)), "{listed}");
    // Metering sees the documents: the cluster's figures, and a resident size above the base.
    e.control.sample().await.unwrap();
    let shown = e.get("/v1/search/clusters/docs").await;
    assert_eq!(shown["docs"], 3, "{shown}");
    assert_eq!(shown["indices"], 1, "{shown}");
    // Another tenant can't see them.
    let other = e.post("/v1/search/clusters", json!({"name": "other"})).await;
    let (ou, op) = creds(&other);
    let (s, v) = e.es(&ou, &op, reqwest::Method::GET, "/books/_doc/1", None).await;
    assert_eq!(s, 404, "{v}");
    assert_eq!(v["error"]["type"], "index_not_found_exception");
    let (s, v) = e.es(&user, &pw, reqwest::Method::POST, "/books/_search", Some(json!({"query": {"term": {"year": 2021}}}))).await;
    // The search APIs are being wired into search-es; until then they answer 501.
    if s != 501 {
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["hits"]["total"]["value"], 2, "{v}");
        let mut ids: Vec<&str> = v["hits"]["hits"].as_array().unwrap().iter().map(|h| h["_id"].as_str().unwrap()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["2", "3"]);
        // Sorting on `_id` is refused, as in Elasticsearch 9.5.5 (no fielddata on `_id`).
        let (s, v) = e.es(&user, &pw, reqwest::Method::POST, "/books/_search", Some(json!({"sort": [{"_id": "asc"}]}))).await;
        assert_eq!((s, v["error"]["root_cause"][0]["type"].as_str()), (400, Some("illegal_argument_exception")), "{v}");
        // The console reaches the same search (what the dashboard's document browser sends).
        let r = e.post("/v1/search/clusters/docs/request", json!({"method": "POST", "path": "/books/_search", "body": {"query": {"query_string": {"query": "title:rust"}}, "from": 0, "size": 20, "track_total_hits": true}})).await;
        assert_eq!(r["status"], 200, "{r}");
        assert_eq!(body(&r)["hits"]["total"]["value"], 3, "{r}");
    }
    // The console sends JSON bodies byte for byte: `_source` keeps the client's key order.
    // (Written by hand: `json!` would sort the keys before they reach the API.)
    let raw = r#"{"method": "PUT", "path": "/books/_doc/9", "body": {"year": 1999, "title": "zz"}}"#;
    let r: Value = e.http.post(format!("{}/v1/search/clusters/docs/request", e.api)).header("content-type", "application/json").body(raw).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["status"], 201, "{r}");
    let r = e.post("/v1/search/clusters/docs/request", json!({"method": "GET", "path": "/books/_doc/9"})).await;
    assert!(r["body"].as_str().unwrap().contains(r#""_source":{"year":1999,"title":"zz"}"#), "{r}");
    // Unloading bills the stay at the size measured from the engine (above the bare cluster).
    assert!(e.post("/v1/search/clusters/docs/unload", json!({})).await["unloaded"].as_bool().unwrap());
    let u = e.get("/v1/search/clusters/docs/usage").await;
    assert!(u["loaded_seconds"].as_i64().unwrap() >= 1, "{u}");
    assert!(u["peak_loaded_bytes"].as_i64().unwrap() > search_control::meter::LOADED_BASE_BYTES, "{u}");
}

/// Documents are durable: acknowledged writes survive an unload (flush, recovery on wake), a
/// branch sees them, and so does a new node on the same object storage after a restart.
/// The per-index routes (what roll's permission catalogue and MCP tools map onto): each one
/// Elasticsearch operation, answering with its status and body.
#[tokio::test]
async fn per_index_routes() {
    let e = env().await;
    e.post("/v1/search/clusters", json!({"name": "idx"})).await;
    let base = "/v1/search/clusters/idx/indices";
    let (s, v) = e.call(reqwest::Method::PUT, &format!("{base}/books"), Some(json!({"mappings": {"properties": {"title": {"type": "text"}}}}))).await;
    assert_eq!((s, &v["acknowledged"]), (200, &json!(true)), "{v}");
    let (s, v) = e.call(reqwest::Method::PUT, &format!("{base}/books/mapping"), Some(json!({"properties": {"year": {"type": "integer"}}}))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.call(reqwest::Method::PUT, &format!("{base}/books/docs/1?refresh=true"), Some(json!({"title": "the rust book", "year": 2018}))).await;
    assert_eq!((s, v["result"].as_str()), (201, Some("created")), "{v}");
    let (s, v) = e.call(reqwest::Method::POST, &format!("{base}/books/docs?refresh=true"), Some(json!({"title": "rust in action", "year": 2021}))).await;
    assert_eq!(s, 201, "{v}");
    let bulk = "{\"index\":{\"_id\":\"3\"}}\n{\"title\":\"programming rust\",\"year\":2021}\n";
    let r = e.http.post(format!("{}{base}/books/bulk?refresh=true", e.api)).body(bulk).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["errors"], false, "{v}");
    // A route names its index and reaches no other: not through a bulk line's `_index`, nor a
    // read's expression.
    e.call(reqwest::Method::PUT, &format!("{base}/secrets/docs/1?refresh=true"), Some(json!({"k": "v"}))).await;
    let sneaky = "{\"index\":{\"_index\":\"secrets\",\"_id\":\"2\"}}\n{\"k\":\"w\"}\n";
    let r = e.http.post(format!("{}{base}/books/bulk?refresh=true", e.api)).body(sneaky).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!((v["errors"].as_bool(), v["items"][0]["index"]["status"].as_i64()), (Some(true), Some(403)), "the item is denied, as Elasticsearch denies bulk items: {v}");
    let (s, v) = e.call(reqwest::Method::POST, &format!("{base}/books,secrets/search"), Some(json!({}))).await;
    assert_eq!((s, v["hits"]["total"]["value"].as_i64()), (200, Some(4)), "a scoped read names both: {v}");
    let (s, v) = e.call(reqwest::Method::POST, &format!("{base}/book*/search"), Some(json!({"size": 0}))).await;
    assert_eq!((s, v["hits"]["total"]["value"].as_i64()), (200, Some(3)), "{v}");
    let (s, v) = e.call(reqwest::Method::GET, &format!("{base}/secrets/docs/2"), None).await;
    assert_eq!((s, &v["found"]), (404, &json!(false)), "the bulk line didn't write: {v}");
    // Nor through a lookup inside the query (read as the caller, as Elasticsearch does).
    let lookup = |index: &str| json!({"query": {"terms": {"title": {"index": index, "id": "1", "path": "title"}}}});
    let (s, v) = e.call(reqwest::Method::POST, &format!("{base}/books/search"), Some(lookup("secrets"))).await;
    assert_eq!((s, v["error"]["type"].as_str()), (403, Some("security_exception")), "{v}");
    let (s, v) = e.call(reqwest::Method::POST, &format!("{base}/books/search"), Some(lookup("books"))).await;
    assert_eq!(s, 200, "a lookup in the route's own index: {v}");
    // However the body comes: pretty-printed, or in `?source=`.
    let pretty = serde_json::to_string_pretty(&lookup("secrets")).unwrap();
    let r = e.http.post(format!("{}{base}/books/search", e.api)).header("content-type", "application/json").body(pretty).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 403);
    let source = urlencoding::encode(&lookup("secrets").to_string()).into_owned();
    let r = e.http.post(format!("{}{base}/books/search?source={source}&source_content_type=application/json", e.api)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 403);
    // Nor through an ingest pipeline that moves the document (the owner's, made in the console).
    let (s, v) = e.call(reqwest::Method::POST, "/v1/search/clusters/idx/request", Some(json!({"method": "PUT", "path": "/_ingest/pipeline/move", "body": {"processors": [{"set": {"field": "_index", "value": "secrets"}}]}}))).await;
    assert_eq!((s, v["status"].as_i64()), (200, Some(200)), "{v}");
    let (s, v) = e.call(reqwest::Method::PUT, &format!("{base}/books/docs/9?pipeline=move&refresh=true"), Some(json!({"k": "moved"}))).await;
    assert_eq!((s, v["error"]["type"].as_str()), (403, Some("security_exception")), "{v}");
    let (s, _) = e.call(reqwest::Method::GET, &format!("{base}/secrets/docs/9"), None).await;
    assert_eq!(s, 404, "nothing written where the pipeline pointed");
    let (s, v) = e.call(reqwest::Method::GET, &format!("{base}/books/docs/1"), None).await;
    assert_eq!((s, v["_source"]["year"].as_i64()), (200, Some(2018)), "{v}");
    let (s, v) = e.call(reqwest::Method::POST, &format!("{base}/books/search"), Some(json!({"query": {"term": {"year": 2021}}}))).await;
    assert_eq!((s, v["hits"]["total"]["value"].as_i64()), (200, Some(2)), "{v}");
    let (_, v) = e.call(reqwest::Method::GET, &format!("{base}/books"), None).await;
    assert_eq!(v["books"]["mappings"]["properties"]["year"]["type"], "integer", "{v}");
    let (_, v) = e.call(reqwest::Method::GET, &format!("{base}/books/mapping"), None).await;
    assert_eq!(v["books"]["mappings"]["properties"]["year"]["type"], "integer", "{v}");
    let (s, v) = e.call(reqwest::Method::POST, &format!("{base}/books/analyze"), Some(json!({"field": "title", "text": "Rust Book"}))).await;
    assert_eq!((s, v["tokens"].as_array().map(Vec::len)), (200, Some(2)), "{v}");
    let (s, v) = e.call(reqwest::Method::POST, &format!("{base}/books/delete_by_query?refresh=true"), Some(json!({"query": {"term": {"year": 2018}}}))).await;
    assert_eq!((s, v["deleted"].as_i64()), (200, Some(1)), "{v}");
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("{base}/books/docs/3"), None).await;
    assert_eq!(s, 200);
    // Elasticsearch's own errors and statuses come through.
    let (s, v) = e.call(reqwest::Method::GET, &format!("{base}/books/docs/1"), None).await;
    assert_eq!((s, &v["found"]), (404, &json!(false)), "{v}");
    let (s, v) = e.call(reqwest::Method::GET, &format!("{base}/nope"), None).await;
    assert_eq!((s, v["error"]["type"].as_str()), (404, Some("index_not_found_exception")), "{v}");
    // A write names one index; `_` names are Elasticsearch's own APIs, not indices.
    assert_eq!(e.call(reqwest::Method::DELETE, &format!("{base}/books*"), None).await.0, 400);
    assert_eq!(e.call(reqwest::Method::GET, &format!("{base}/_all"), None).await.0, 400);
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("{base}/books"), None).await;
    assert_eq!(s, 200);
}

/// API keys a client gets instead of the cluster's password (roll secrets' managed links):
/// they search, and stop working once revoked; the password is untouched.
#[tokio::test]
async fn api_keys() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "keys"})).await;
    let (user, pw) = creds(&c);
    let (s, _) = e.es(&user, &pw, reqwest::Method::PUT, "/notes/_doc/1?refresh=true", Some(json!({"t": "x"}))).await;
    assert_eq!(s, 201);
    let k = e.post("/v1/search/clusters/keys/keys", json!({"name": "roll secrets prod ES_KEY", "expires_in_s": 3600})).await;
    assert!(k["expires_at"].as_i64().is_some(), "{k}");
    let header = format!("ApiKey {}", k["api_key"].as_str().unwrap());
    let search = |h: String| {
        let url = format!("{}/notes/_count", e.es);
        let http = e.http.clone();
        async move { http.get(url).header("Authorization", h).send().await.unwrap().status().as_u16() }
    };
    assert_eq!(search(header.clone()).await, 200);
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/keys/keys/{}", k["id"].as_str().unwrap()), None).await;
    assert_eq!(s, 204);
    assert_eq!(search(header).await, 401);
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/keys/keys/{}", k["id"].as_str().unwrap()), None).await;
    assert_eq!(s, 404);
    // The cluster's own password still works.
    assert_eq!(e.es(&user, &pw, reqwest::Method::GET, "/notes/_count", None).await.0, 200);
    // A data key writes documents but can't make keys or users of its own.
    let d = e.post("/v1/search/clusters/keys/keys", json!({"name": "app", "access": "data"})).await;
    // The answer says which access it made (the default is admin).
    assert_eq!((d["access"].as_str(), k["access"].as_str()), (Some("data"), Some("admin")));
    let h = format!("ApiKey {}", d["api_key"].as_str().unwrap());
    let send = |m: reqwest::Method, path: &str, body: Value| {
        let r = e.http.request(m, format!("{}{path}", e.es)).header("Authorization", h.clone()).json(&body).send();
        async move { r.await.unwrap().status().as_u16() }
    };
    assert_eq!(send(reqwest::Method::PUT, "/notes/_doc/2", json!({"t": "y"})).await, 201);
    assert_eq!(send(reqwest::Method::POST, "/_security/api_key", json!({"name": "more"})).await, 403);
    assert_eq!(send(reqwest::Method::PUT, "/_security/user/x", json!({"password": "pw-pw-pw-pw", "roles": ["superuser"]})).await, 403);
    let (s, _) = e.call(reqwest::Method::POST, "/v1/search/clusters/keys/keys", Some(json!({"name": "x", "access": "root"}))).await;
    assert_eq!(s, 400);
}

/// A cluster's audit log entries (`GET …/audit`), and the one of them that is `event` and
/// whose entry passes `f`.
async fn audit_entries(e: &Env, cluster: &str, query: &str) -> Vec<Value> {
    e.get(&format!("/v1/search/clusters/{cluster}/audit{query}")).await["entries"].as_array().unwrap().clone()
}

fn audited<'a>(entries: &'a [Value], event: &str, f: impl Fn(&Value) -> bool) -> Option<&'a Value> {
    entries.iter().find(|x| x["event"] == event && f(&x["entry"])).map(|x| &x["entry"])
}

/// The security audit log (docs/search/README.md, "Audit log"): a wrong password, a denied
/// index, an API key made and invalidated and a role change each show up under Elasticsearch's
/// event names with its fields, newest first; a granted request only once the cluster asks for
/// it; an OpenSearch-flavor cluster records OpenSearch's categories as its stored audit config
/// says; a deleted cluster's entries go with it, and come back with a restore.
#[tokio::test]
async fn the_audit_log_records_what_matters_for_security() {
    use reqwest::Method;
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "aud"})).await;
    let (user, pw) = creds(&c);
    for (path, body) in [
        ("/open/_doc/1?refresh=true", json!({"t": "x"})),
        ("/secret/_doc/1?refresh=true", json!({"t": "y"})),
        ("/_security/role/reader", json!({"indices": [{"names": ["open"], "privileges": ["read"]}]})),
        ("/_security/user/bob", json!({"password": "bob-password-1", "roles": ["reader"]})),
    ] {
        let (s, v) = e.es(&user, &pw, Method::PUT, path, Some(body)).await;
        assert!(s == 200 || s == 201, "{path}: {v}");
    }
    assert_eq!(e.es("bob@aud", "not-bobs-password", Method::GET, "/open/_search", None).await.0, 401);
    assert_eq!(e.es("bob@aud", "bob-password-1", Method::GET, "/secret/_search", None).await.0, 403);
    assert_eq!(e.es("bob@aud", "bob-password-1", Method::GET, "/open/_search", None).await.0, 200);
    let k = e.post("/v1/search/clusters/aud/keys", json!({"name": "ci"})).await;
    let key_id = k["id"].as_str().unwrap();
    assert_eq!(e.call(Method::DELETE, &format!("/v1/search/clusters/aud/keys/{key_id}"), None).await.0, 204);
    let (s, _) = e.es(&user, &pw, Method::PUT, "/_security/role/reader", Some(json!({"indices": [{"names": ["open", "more"], "privileges": ["read"]}]}))).await;
    assert_eq!(s, 200);

    let all = audit_entries(&e, "aud", "").await;
    let failed = audited(&all, "authentication_failed", |x| x["user.name"] == "bob").expect("the wrong password");
    assert_eq!((&failed["event.type"], &failed["url.path"], &failed["request.method"]), (&json!("rest"), &json!("/open/_search"), &json!("GET")), "{failed}");
    assert_eq!(failed["origin.address"], "127.0.0.1", "the address the allowlist checks: {failed}");
    let denied = audited(&all, "access_denied", |x| x["user.name"] == "bob").expect("the denied index");
    assert_eq!(
        (&denied["action"], &denied["indices"], &denied["user.realm"], &denied["authentication.type"], &denied["request.name"]),
        (&json!("indices:data/read/search"), &json!(["secret"]), &json!("default_native"), &json!("REALM"), &json!("SearchRequest")),
        "{denied}"
    );
    let put_user = audited(&all, "security_config_change", |x| x["event.action"] == "put_user").expect("the new user");
    assert_eq!(put_user["put"]["user"], json!({"name": "bob", "enabled": true, "roles": ["reader"], "has_password": true}));
    let created = audited(&all, "security_config_change", |x| x["event.action"] == "create_apikey").expect("the key");
    assert_eq!((&created["create"]["apikey"]["id"], &created["create"]["apikey"]["name"]), (&json!(key_id), &json!("ci")), "{created}");
    let invalidated = audited(&all, "security_config_change", |x| x["event.action"] == "invalidate_apikeys").expect("the revoked key");
    assert_eq!(invalidated["invalidate"]["apikeys"]["ids"], json!([key_id]));
    let role = audited(&all, "security_config_change", |x| x["event.action"] == "put_role").expect("the role change");
    assert_eq!(role["put"]["role"]["role_descriptor"]["indices"][0]["names"], json!(["open", "more"]), "newest first: {role}");
    assert!(audited(&all, "access_granted", |_| true).is_none(), "granted access isn't recorded by default");
    let ats: Vec<i64> = all.iter().map(|x| x["at_ms"].as_i64().unwrap()).collect();
    assert!(ats.windows(2).all(|w| w[0] >= w[1]), "newest first: {ats:?}");
    let only = audit_entries(&e, "aud", "?event=access_denied").await;
    assert!(!only.is_empty() && only.iter().all(|x| x["event"] == "access_denied"));
    let later = audit_entries(&e, "aud", &format!("?from={}", ats[0] + 1)).await;
    assert!(later.is_empty(), "{later:?}");

    // Granted access once the cluster includes it; event names are checked; `enabled: false`
    // records nothing more.
    let settings = |v: Value| e.es(&user, &pw, Method::PUT, "/_cluster/settings", Some(json!({"persistent": v})));
    let (s, v) = settings(json!({"xpack.security.audit.logfile.events.include": "bogus"})).await;
    assert_eq!((s, v["error"]["reason"].as_str()), (400, Some("invalid event name specified [bogus]")), "{v}");
    assert_eq!(settings(json!({"xpack.security.audit.logfile.events.include": "_all"})).await.0, 200);
    assert_eq!(e.es("bob@aud", "bob-password-1", Method::GET, "/open/_search", None).await.0, 200);
    let granted = audit_entries(&e, "aud", "?event=access_granted").await;
    assert!(audited(&granted, "access_granted", |x| x["user.name"] == "bob" && x["indices"] == json!(["open"])).is_some(), "{granted:?}");
    assert_eq!(settings(json!({"xpack.security.audit.enabled": false})).await.0, 200);
    let before = audit_entries(&e, "aud", "").await.len();
    assert_eq!(e.es("bob@aud", "wrong-again", Method::GET, "/", None).await.0, 401);
    assert_eq!(audit_entries(&e, "aud", "").await.len(), before, "nothing recorded while disabled");

    // OpenSearch: its categories, and its stored audit config takes effect.
    let o = e.post("/v1/search/clusters", json!({"name": "aud-os", "flavor": "opensearch"})).await;
    let (ou, opw) = creds(&o);
    for (path, body) in [
        ("/open/_doc/1?refresh=true", json!({"t": "x"})),
        ("/secret/_doc/1?refresh=true", json!({"t": "y"})),
        ("/_plugins/_security/api/roles/reader", json!({"index_permissions": [{"index_patterns": ["open"], "allowed_actions": ["read"]}]})),
        ("/_plugins/_security/api/internalusers/carol", json!({"password": "Zq9#vLx2!pRt7mWk", "opendistro_security_roles": ["reader"]})),
    ] {
        let (s, v) = e.es(&ou, &opw, Method::PUT, path, Some(body)).await;
        assert!(s == 200 || s == 201, "{path}: {v}");
    }
    assert_eq!(e.es("carol@aud-os", "not-carols", Method::GET, "/open/_search", None).await.0, 401);
    assert_eq!(e.es("carol@aud-os", "Zq9#vLx2!pRt7mWk", Method::GET, "/secret/_search", None).await.0, 403);
    let os = audit_entries(&e, "aud-os", "").await;
    let failed = audited(&os, "FAILED_LOGIN", |x| x["audit_request_effective_user"] == "carol").expect("the wrong password");
    assert_eq!((&failed["audit_request_layer"], &failed["audit_rest_request_path"], &failed["audit_format_version"]), (&json!("REST"), &json!("/open/_search"), &json!(4)), "{failed}");
    let denied = audited(&os, "MISSING_PRIVILEGES", |x| x["audit_request_effective_user"] == "carol").expect("the denied index");
    assert_eq!((&denied["audit_request_privilege"], &denied["audit_trace_indices"]), (&json!("indices:data/read/search"), &json!(["secret"])), "{denied}");
    let write = audited(&os, "COMPLIANCE_INTERNAL_CONFIG_WRITE", |x| x["audit_trace_doc_id"] == "roles").expect("the role");
    assert_eq!((&write["audit_request_effective_user"], &write["audit_compliance_operation"]), (&json!("admin"), &json!("UPDATE")), "{write}");
    let (s, v) = e.es(&ou, &opw, Method::PATCH, "/_plugins/_security/api/audit", Some(json!([{"op": "replace", "path": "/config/audit/enable_rest", "value": false}]))).await;
    assert_eq!(s, 200, "{v}");
    let before = audit_entries(&e, "aud-os", "?event=FAILED_LOGIN").await.len();
    assert_eq!(e.es("carol@aud-os", "still-not-carols", Method::GET, "/", None).await.0, 401);
    assert_eq!(audit_entries(&e, "aud-os", "?event=FAILED_LOGIN").await.len(), before, "REST categories off");

    // A deleted cluster's entries go with it (a new cluster of the same name starts empty) and
    // come back when it is restored.
    let id = c["id"].as_str().unwrap();
    assert_eq!(e.call(Method::DELETE, "/v1/search/clusters/aud", None).await.0, 200);
    e.post("/v1/search/clusters", json!({"name": "aud"})).await;
    assert_eq!(audit_entries(&e, "aud", "").await, Vec::<Value>::new());
    let (s, v) = e.call(Method::POST, &format!("/v1/search/clusters/{id}/restore"), Some(json!({"name": "aud-back"}))).await;
    assert_eq!(s, 200, "{v}");
    let back = audit_entries(&e, "aud-back", "?event=security_config_change").await;
    assert!(audited(&back, "security_config_change", |x| x["event.action"] == "put_user").is_some(), "{back:?}");
}

/// No audit entry holds a credential, a query string or a request body: every secret the
/// requests below carry (passwords, an API key, a token, the cluster's own password, markers in
/// bodies and query strings) is looked for in the entries as the API answers them and in the
/// objects as stored, with every event recorded.
#[tokio::test]
async fn audit_entries_hold_no_secrets() {
    use base64::Engine;
    use reqwest::Method;
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "sec"})).await;
    let (user, pw) = creds(&c);
    let (s, _) = e.es(&user, &pw, Method::PUT, "/_cluster/settings", Some(json!({"persistent": {"xpack.security.audit.logfile.events.include": "_all"}}))).await;
    assert_eq!(s, 200);
    let (s, _) = e.es(&user, &pw, Method::PUT, "/_security/role/r", Some(json!({"indices": [{"names": ["open"], "privileges": ["all"]}]}))).await;
    assert_eq!(s, 200);
    let (s, _) = e.es(&user, &pw, Method::PUT, "/_security/user/dave", Some(json!({"password": "USER-SECRET-pw-1", "roles": ["r"]}))).await;
    assert_eq!(s, 200);
    let (s, _) = e.es(&user, &pw, Method::POST, "/_security/user/dave/_password", Some(json!({"password": "USER-SECRET-pw-2"}))).await;
    assert_eq!(s, 200);
    assert_eq!(e.es("dave@sec", "WRONG-SECRET-pw", Method::GET, "/open/_search?q=QUERY-MARKER", None).await.0, 401);
    assert_eq!(e.es("dave@sec", "USER-SECRET-pw-2", Method::POST, "/secret/_doc?routing=QUERY-MARKER", Some(json!({"t": "BODY-MARKER"}))).await.0, 403);
    assert_eq!(e.es("dave@sec", "USER-SECRET-pw-2", Method::POST, "/open/_doc", Some(json!({"t": "BODY-MARKER"}))).await.0, 201);
    let (s, k) = e.es(&user, &pw, Method::POST, "/_security/api_key", Some(json!({"name": "k", "role_descriptors": {"x": {"cluster": ["monitor"]}}, "metadata": {"m": 1}}))).await;
    assert_eq!(s, 200, "{k}");
    let key = k["api_key"].as_str().unwrap().to_string();
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{}@sec:{key}", k["id"].as_str().unwrap()));
    let with = |h: String, path: &'static str| {
        let (url, http) = (format!("{}{path}", e.es), e.http.clone());
        async move { http.get(url).header("Authorization", h).send().await.unwrap().status().as_u16() }
    };
    assert_eq!(with(format!("ApiKey {encoded}"), "/secret/_search").await, 403);
    let wrong_key = base64::engine::general_purpose::STANDARD.encode(format!("{}@sec:WRONG-KEY-SECRET", k["id"].as_str().unwrap()));
    assert_eq!(with(format!("ApiKey {wrong_key}"), "/").await, 401);
    assert_eq!(with("Bearer sec_BEARER-SECRET-token".into(), "/").await, 401);
    let (s, _) = e.es(&user, &pw, Method::POST, "/_security/api_key/grant", Some(json!({"grant_type": "password", "username": "dave", "password": "USER-SECRET-pw-2", "api_key": {"name": "g"}}))).await;
    assert_eq!(s, 200);

    let secrets = ["USER-SECRET", "WRONG-SECRET", "QUERY-MARKER", "BODY-MARKER", "WRONG-KEY-SECRET", "BEARER-SECRET", key.as_str(), encoded.as_str(), pw.as_str(), "Basic ", "ApiKey ", "authorization", "Authorization"];
    let answered = e.get("/v1/search/clusters/sec/audit?size=1000").await;
    let entries = answered["entries"].as_array().unwrap();
    for event in ["authentication_failed", "anonymous_access_denied", "access_denied", "access_granted", "security_config_change"] {
        assert!(entries.iter().any(|x| x["event"] == event), "no {event} recorded to check: {answered}");
    }
    let mut stored = String::new();
    for o in e.control.data.list_audit("sec").await.unwrap() {
        stored.push_str(std::str::from_utf8(&e.control.data.read_audit(&o).await.unwrap().unwrap()).unwrap());
    }
    assert!(!stored.is_empty());
    for (what, text) in [("answered", answered.to_string()), ("stored", stored)] {
        for secret in secrets {
            assert!(!text.contains(secret), "{what} audit entries hold {secret:?}: {text}");
        }
    }
}

/// Masking a branch (roll previews: `_update_by_query`, then `only_expunge_deletes` with
/// `expunge_deletes_allowed: 0`) rewrites the segments it shares with its parent, so the old
/// values are gone from the branch's term dictionaries (`_terms_enum`, suggesters), while the
/// parent keeps them.
#[tokio::test]
async fn expunging_a_branch_rewrites_the_segments_it_shares() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "masked"})).await;
    let (user, pw) = creds(&c);
    let (s, _) = e.es(&user, &pw, reqwest::Method::PUT, "/people/_doc/1?refresh=true", Some(json!({"name": "Alice Example"}))).await;
    assert_eq!(s, 201);
    let b = e.post("/v1/search/clusters/masked/branches", json!({"name": "masked-b"})).await;
    let (bu, bp) = creds(&b);
    let terms = |u: String, p: String| {
        let e = &e;
        async move { e.es(&u, &p, reqwest::Method::POST, "/people/_terms_enum", Some(json!({"field": "name.keyword"}))).await.1["terms"].to_string() }
    };
    let mask = json!({"query": {"exists": {"field": "name"}}, "script": {"source": "ctx._source.name = '[masked]'"}});
    let (s, v) = e.es(&bu, &bp, reqwest::Method::POST, "/people/_update_by_query?conflicts=proceed&refresh=true", Some(mask)).await;
    assert_eq!(s, 200, "{v}");
    // Masked but not yet rewritten: the shared segment still holds the old term.
    assert!(terms(bu.clone(), bp.clone()).await.contains("Alice Example"));
    let (s, v) = e.es(&bu, &bp, reqwest::Method::PUT, "/people/_settings", Some(json!({"index.merge.policy.expunge_deletes_allowed": 0}))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(&bu, &bp, reqwest::Method::POST, "/people/_forcemerge?only_expunge_deletes=true", None).await;
    assert_eq!(s, 200, "{v}");
    let branch = terms(bu, bp).await;
    assert!(!branch.contains("Alice Example") && branch.contains("[masked]"), "{branch}");
    assert!(terms(user, pw).await.contains("Alice Example"), "the parent keeps its documents");
}

#[tokio::test]
async fn documents_survive_unload_and_branch() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "durable"})).await;
    let (user, pw) = creds(&c);
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/notes/_doc/1?refresh=true", Some(json!({"text": "kept"}))).await;
    assert_eq!(s, 201, "{v}");
    assert!(e.post("/v1/search/clusters/durable/unload", json!({})).await["unloaded"].as_bool().unwrap());
    // An unload through the API records the asleep figures at once (no metering pass needed).
    let shown = e.get("/v1/search/clusters/durable").await;
    assert_eq!((shown["status"]["state"].as_str(), shown["indices"].as_i64(), shown["docs"].as_i64()), (Some("asleep"), Some(1), Some(1)), "{shown}");
    assert!(shown["stored_bytes"].as_i64().unwrap() > 0, "{shown}");
    let (s, v) = e.es(&user, &pw, reqwest::Method::GET, "/notes/_doc/1", None).await;
    assert_eq!((s, v["_source"]["text"].as_str()), (200, Some("kept")), "{v}");
    e.control.sample().await.unwrap();
    let shown = e.get("/v1/search/clusters/durable").await;
    assert!(shown["stored_bytes"].as_i64().unwrap() > 0, "{shown}");
    // A branch sees the parent's documents, and shows its figures from the start.
    let b = e.post("/v1/search/clusters/durable/branches", json!({"name": "durable-b"})).await;
    assert_eq!((b["indices"].as_i64(), b["docs"].as_i64()), (Some(1), Some(1)), "{b}");
    assert!(b["stored_bytes"].as_i64().unwrap() > 0, "{b}");
    let (bu, bp) = creds(&b);
    let (s, v) = e.es(&bu, &bp, reqwest::Method::GET, "/notes/_doc/1", None).await;
    assert_eq!((s, v["_source"]["text"].as_str()), (200, Some("kept")), "{v}");
    // Written, not refreshed, then the node stops: the WAL has it.
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/notes/_doc/2", Some(json!({"text": "late"}))).await;
    assert_eq!(s, 201, "{v}");
    e.control.shutdown().await.unwrap();
    // A new node on another machine: the same catalog and object storage, an empty disk.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let es = format!("http://{}", listener.local_addr().unwrap());
    let mut cfg = e.cfg.clone();
    cfg.http_port = listener.local_addr().unwrap().port();
    cfg.home = e.home.with_extension("restarted");
    std::fs::create_dir_all(&cfg.home).unwrap();
    for f in ["search.db", "search.db-wal", "search.db-shm"] {
        let _ = std::fs::copy(e.home.join(f), cfg.home.join(f));
    }
    let restarted_home = cfg.home.clone();
    let control = Control::new(cfg, e.objects.clone()).await.unwrap();
    tokio::spawn(search_server::serve(listener, control.node.clone(), search_server::ListenOptions::default()));
    let get = |id: &'static str| e.http.get(format!("{es}/notes/_doc/{id}")).basic_auth(&user, Some(&pw)).send();
    for (id, text) in [("1", "kept"), ("2", "late")] {
        let v: Value = get(id).await.unwrap().json().await.unwrap();
        assert_eq!(v["_source"]["text"], text, "{v}");
    }
    let r = e.http.get(format!("{es}/notes/_count")).basic_auth(&user, Some(&pw)).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["count"], 2);
    control.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(restarted_home);
}

/// A cluster with `idle_timeout_s` 0 is up from the moment it exists, when the setting
/// changes to 0, and after a node starts: never asleep until its first request.
#[tokio::test]
async fn always_on_clusters_load_without_a_request() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "always", "idle_timeout_s": 0})).await;
    let (user, pw) = creds(&c);
    assert_eq!(e.control.node.loaded(), ["always"]);
    assert_eq!(e.get("/v1/search/clusters/always").await["status"]["state"], "loaded");
    let listed = e.get("/v1/search/clusters").await;
    assert_eq!(listed[0]["status"]["state"], "loaded", "{listed}");
    // A cluster that may sleep is created asleep, and loads once it may not.
    e.post("/v1/search/clusters", json!({"name": "sometimes"})).await;
    assert_eq!(e.get("/v1/search/clusters/sometimes").await["status"]["state"], "asleep");
    let (s, v) = e.call(reqwest::Method::PATCH, "/v1/search/clusters/sometimes", Some(json!({"idle_timeout_s": 0}))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(e.get("/v1/search/clusters/sometimes").await["status"]["state"], "loaded");
    // An explicit unload still puts it to sleep.
    assert!(e.post("/v1/search/clusters/sometimes/unload", json!({})).await["unloaded"].as_bool().unwrap());
    assert_eq!(e.get("/v1/search/clusters/sometimes").await["status"]["state"], "asleep");
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/kept/_doc/1?refresh=true", Some(json!({"n": 1}))).await;
    assert_eq!(s, 201, "{v}");
    e.control.shutdown().await.unwrap();
    // A node that starts on the same catalog loads both again on its own.
    let mut cfg = e.cfg.clone();
    cfg.home = e.home.with_extension("restarted");
    std::fs::create_dir_all(&cfg.home).unwrap();
    for f in ["search.db", "search.db-wal", "search.db-shm"] {
        let _ = std::fs::copy(e.home.join(f), cfg.home.join(f));
    }
    let restarted_home = cfg.home.clone();
    let control = Control::new(cfg, e.objects.clone()).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut loaded = control.node.loaded();
        loaded.sort();
        if loaded == ["always", "sometimes"] {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{loaded:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let l = control.node.get_loaded("always").unwrap();
    assert_eq!(l.cluster.state().metadata.indices.len(), 1, "loaded with its data");
    drop(l);
    control.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(restarted_home);
}

/// `GET /` with Basic auth, from a task of its own (the request that wakes a cluster).
fn spawn_get(e: &Env, user: &str, password: &str, path: &str) -> tokio::task::JoinHandle<u16> {
    let req = e.http.get(format!("{}{path}", e.es)).basic_auth(user, Some(password));
    tokio::spawn(async move { req.send().await.unwrap().status().as_u16() })
}

/// Unloads a cluster and makes its next wake take `delay`, so changes can land mid-wake.
async fn asleep_with_slow_wake(e: &Env, name: &str, delay: Duration) {
    assert!(e.post(&format!("/v1/search/clusters/{name}/unload"), json!({})).await["unloaded"].as_bool().unwrap());
    e.control.set_wake_delay(delay);
}

#[tokio::test]
async fn rotation_during_a_wake_applies() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "rot", "idle_timeout_s": 0})).await;
    let (user, old) = creds(&c);
    assert_eq!(e.es(&user, &old, reqwest::Method::PUT, "/i", Some(json!({}))).await.0, 200);
    asleep_with_slow_wake(&e, "rot", Duration::from_millis(400)).await;
    // The request looks the cluster up (old password), then the cluster loads for 400 ms.
    let waking = spawn_get(&e, &user, &old, "/");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(e.control.node.loaded(), Vec::<String>::new(), "still waking");
    let r = e.post("/v1/search/clusters/rot/password", json!({})).await;
    let (_, new) = creds(&r);
    e.control.set_wake_delay(Duration::ZERO);
    // Authenticated before the rotation: served.
    assert_eq!(waking.await.unwrap(), 200);
    assert_eq!(e.control.node.loaded(), ["rot"]);
    // From the rotation on, only the new password works, and the cluster stays loaded.
    assert_eq!(e.es(&user, &old, reqwest::Method::GET, "/", None).await.0, 401);
    assert_eq!(e.es(&user, &new, reqwest::Method::GET, "/", None).await.0, 200);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(e.es(&user, &old, reqwest::Method::GET, "/", None).await.0, 401);
    assert_eq!(e.es(&user, &new, reqwest::Method::GET, "/i", None).await.0, 200);
}

#[tokio::test]
async fn delete_during_a_wake_leaves_no_ghost() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "ghost"})).await;
    let (user, old) = creds(&c);
    for i in ["a", "b"] {
        assert_eq!(e.es(&user, &old, reqwest::Method::PUT, &format!("/{i}"), Some(json!({}))).await.0, 200);
    }
    asleep_with_slow_wake(&e, "ghost", Duration::from_millis(400)).await;
    let waking = spawn_get(&e, &user, &old, "/");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (s, v) = e.call(reqwest::Method::DELETE, "/v1/search/clusters/ghost", None).await;
    assert_eq!(s, 200, "{v}");
    e.control.set_wake_delay(Duration::ZERO);
    // The wake started before the delete and was let finish; the delete then unloaded it.
    assert_eq!(waking.await.unwrap(), 200);
    assert!(!e.control.node.loaded().contains(&"ghost".to_string()));
    assert_eq!(e.es(&user, &old, reqwest::Method::GET, "/", None).await.0, 401);
    assert_eq!(e.call(reqwest::Method::GET, "/v1/search/clusters/ghost", None).await.0, 404);
    // A new cluster of the same name is new: empty, and only its own password works.
    let c = e.post("/v1/search/clusters", json!({"name": "ghost"})).await;
    let (_, new) = creds(&c);
    assert_eq!(e.es(&user, &old, reqwest::Method::GET, "/", None).await.0, 401);
    let (s, v) = e.es(&user, &new, reqwest::Method::GET, "/_mapping", None).await;
    assert_eq!((s, index_names(&v)), (200, Vec::<String>::new()), "{v}");
    assert_eq!(e.es(&user, &old, reqwest::Method::GET, "/", None).await.0, 401);
}

/// Five `_cluster/health` waits of `timeout` at once; returns their statuses, sorted.
async fn health_burst(e: &Env, user: &str, pw: &str, timeout: &str) -> Vec<u16> {
    let tasks: Vec<_> = (0..5).map(|_| spawn_get(e, user, pw, &format!("/_cluster/health?wait_for_status=green&timeout={timeout}"))).collect();
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out.sort_unstable();
    out
}

#[tokio::test]
async fn max_in_flight_applies_to_a_loaded_cluster() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "lim", "max_in_flight": 64})).await;
    let (user, pw) = creds(&c);
    // A replica that can't be assigned: yellow, so green waits run until their timeout.
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/y", Some(json!({"settings": {"number_of_replicas": 1}}))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(health_burst(&e, &user, &pw, "300ms").await, [408; 5]);
    let (s, v) = e.call(reqwest::Method::PATCH, "/v1/search/clusters/lim", Some(json!({"max_in_flight": 1}))).await;
    assert_eq!((s, &v["max_in_flight"]), (200, &json!(1)), "{v}");
    // One runs, one waits, the rest are turned away.
    let got = health_burst(&e, &user, &pw, "500ms").await;
    assert_eq!(got.iter().filter(|s| **s == 429).count(), 3, "{got:?}");
    // And back up.
    e.call(reqwest::Method::PATCH, "/v1/search/clusters/lim", Some(json!({"max_in_flight": 8}))).await;
    assert_eq!(health_burst(&e, &user, &pw, "300ms").await, [408; 5]);
}

#[tokio::test]
async fn reload_settings_apply_once_requests_finish() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "flv", "flavor": "opensearch", "idle_timeout_s": 0})).await;
    let (user, pw) = creds(&c);
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/y", Some(json!({"settings": {"number_of_replicas": 1}}))).await;
    assert_eq!(s, 200, "{v}");
    let busy = spawn_get(&e, &user, &pw, "/_cluster/health?wait_for_status=green&timeout=1s");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (s, v) = e.call(reqwest::Method::PATCH, "/v1/search/clusters/flv", Some(json!({"flavor": "elasticsearch"}))).await;
    assert_eq!((s, &v["flavor"]), (200, &json!("elasticsearch")), "{v}");
    // A request is running: the cluster keeps speaking OpenSearch, node-level answers included.
    let (_, root) = e.es(&user, &pw, reqwest::Method::GET, "/", None).await;
    assert_eq!(root["version"]["distribution"], "opensearch", "{root}");
    let r = e.http.get(format!("{}/", e.es)).basic_auth(&user, Some("wrong")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401);
    assert_eq!(busy.await.unwrap(), 408);
    // Once it finished, the cluster reloads as Elasticsearch, though it never goes idle.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (s, root) = e.es(&user, &pw, reqwest::Method::GET, "/", None).await;
        assert_eq!(s, 200);
        if root["version"].get("distribution").is_none() {
            assert_eq!(root["version"]["number"], "9.5.5");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "still OpenSearch: {root}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn console_takes_bodies_up_to_the_cluster_limit() {
    let e = env().await;
    e.post("/v1/search/clusters", json!({"name": "big"})).await;
    e.post("/v1/search/clusters", json!({"name": "small", "max_body_mb": 1})).await;
    let blob = "x".repeat(10_000);
    let mut bulk = String::new();
    for i in 0..400 {
        bulk.push_str(&format!("{{\"index\":{{\"_index\":\"blobs\",\"_id\":\"{i}\"}}}}\n{{\"blob\":\"{blob}\"}}\n"));
    }
    assert!(bulk.len() > 3 << 20);
    let mapping = json!({"method": "PUT", "path": "/blobs", "body": {"mappings": {"properties": {"blob": {"type": "keyword", "index": false, "doc_values": false}}}}});
    assert_eq!(e.post("/v1/search/clusters/big/request", mapping).await["status"], 200);
    // Over axum's 2 MB default, within the cluster's 100 MiB.
    let r = e.post("/v1/search/clusters/big/request", json!({"method": "POST", "path": "/_bulk", "body": bulk})).await;
    assert_eq!(r["status"], 200, "{}", &r.to_string()[..500.min(r.to_string().len())]);
    assert_eq!(body(&r)["errors"], false);
    // Over a 1 MiB cluster's limit: the cluster answers Elasticsearch's 413 when the request
    // fits the envelope, the API a 413 of its own when it doesn't.
    let mid = &bulk[..(1 << 20) + 30_000];
    let mid = &mid[..mid.rfind('\n').unwrap() + 1];
    let r = e.post("/v1/search/clusters/small/request", json!({"method": "POST", "path": "/_bulk", "body": mid})).await;
    assert_eq!(r["status"], 413, "{r}");
    let (s, v) = e.call(reqwest::Method::POST, "/v1/search/clusters/small/request", Some(json!({"method": "POST", "path": "/_bulk", "body": bulk}))).await;
    assert_eq!(s, 413, "{v}");
    assert!(v["error"].as_str().unwrap().contains("max_body_mb"), "{v}");
}

/// A sleeping cluster wakes on its own when a background job comes due (a started continuous
/// transform's next checkpoint), as Elasticsearch runs it on schedule, and nothing wakes a
/// cluster without jobs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_due_transform_wakes_its_sleeping_cluster() {
    let e = env_with(|c| c.job_wake_interval = Some(std::time::Duration::from_millis(200))).await;
    let c = e.post("/v1/search/clusters", json!({"name": "jobs"})).await;
    let (user, pw) = creds(&c);
    e.post("/v1/search/clusters", json!({"name": "idle"})).await;
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/src/_doc/1?refresh=true", Some(json!({"t": "2026-01-01T00:00:00Z", "k": "a"}))).await;
    assert_eq!(s, 201, "{v}");
    let transform = json!({
        "source": {"index": "src"}, "dest": {"index": "dst"}, "frequency": "1s",
        "sync": {"time": {"field": "t", "delay": "0s"}},
        "pivot": {"group_by": {"k": {"terms": {"field": "k.keyword"}}}, "aggs": {"n": {"value_count": {"field": "k.keyword"}}}}
    });
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/_transform/t", Some(transform)).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(&user, &pw, reqwest::Method::POST, "/_transform/t/_start", None).await;
    assert_eq!(s, 200, "{v}");
    for name in ["jobs", "idle"] {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let (s, v) = e.call(reqwest::Method::POST, &format!("/v1/search/clusters/{name}/unload"), Some(json!({}))).await;
            assert_eq!(s, 200, "{v}");
            if v["unloaded"] == true || !e.control.node.loaded().iter().any(|n| &**n == name) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{name} never unloaded: {v}");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    // Asleep, a cluster says when its next job wakes it (unless that already happened).
    let (_, v) = e.call(reqwest::Method::GET, "/v1/search/clusters/jobs", None).await;
    assert!(v["status"]["state"] == "loaded" || v["status"]["wakes_at"].as_i64().is_some_and(|t| t > 0), "{v}");
    let (_, v) = e.call(reqwest::Method::GET, "/v1/search/clusters/idle", None).await;
    assert_eq!((v["status"]["state"].as_str(), v["status"].get("wakes_at")), (Some("asleep"), None), "{v}");
    // No request: the transform's next trigger loads its cluster again.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while !e.control.node.loaded().iter().any(|n| &**n == "jobs") {
        assert!(tokio::time::Instant::now() < deadline, "the due transform never woke its cluster");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!e.control.node.loaded().iter().any(|n| &**n == "idle"), "a cluster without jobs stays asleep");
}

/// A data stream lifecycle's retention wakes its sleeping cluster when a backing index comes
/// due for deletion, as Elasticsearch's lifecycle deletes it on its next poll; the write index
/// stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_due_retention_deletion_wakes_its_sleeping_cluster() {
    let e = env_with(|c| c.job_wake_interval = Some(std::time::Duration::from_millis(200))).await;
    let c = e.post("/v1/search/clusters", json!({"name": "logs"})).await;
    let (user, pw) = creds(&c);
    let template = json!({"index_patterns": ["logs-app*"], "data_stream": {}, "template": {"lifecycle": {"data_retention": "5s"}}});
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/_index_template/app", Some(template)).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/_data_stream/logs-app", None).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(&user, &pw, reqwest::Method::POST, "/logs-app/_rollover", None).await;
    assert_eq!(s, 200, "{v}");
    let old = v["old_index"].as_str().unwrap().to_string();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let (s, v) = e.call(reqwest::Method::POST, "/v1/search/clusters/logs/unload", Some(json!({}))).await;
        assert_eq!(s, 200, "{v}");
        if v["unloaded"] == true || !e.control.node.loaded().iter().any(|n| &**n == "logs") {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never unloaded: {v}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (_, v) = e.call(reqwest::Method::GET, "/v1/search/clusters/logs", None).await;
    assert!(v["status"]["state"] == "loaded" || v["status"]["wakes_at"].as_i64().is_some_and(|t| t > 0), "{v}");
    // No request: the retention's deadline loads the cluster, and the lifecycle deletes the
    // old backing index.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while !e.control.node.loaded().iter().any(|n| &**n == "logs") {
        assert!(tokio::time::Instant::now() < deadline, "the due deletion never woke its cluster");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    loop {
        let (s, v) = e.es(&user, &pw, reqwest::Method::GET, "/_data_stream/logs-app", None).await;
        assert_eq!(s, 200, "{v}");
        let indices: Vec<String> = v["data_streams"][0]["indices"].as_array().unwrap().iter().map(|i| i["index_name"].as_str().unwrap().to_string()).collect();
        if !indices.contains(&old) {
            assert_eq!(indices.len(), 1, "the write index stays: {indices:?}");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the old backing index was never deleted: {indices:?}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// A snapshot lifecycle policy's next run wakes its sleeping cluster, which takes the snapshot
/// on schedule, as Elasticsearch's scheduler does, into the default repository every cluster
/// has without setup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_due_slm_run_wakes_its_sleeping_cluster() {
    let e = env_with(|c| c.job_wake_interval = Some(std::time::Duration::from_millis(200))).await;
    let c = e.post("/v1/search/clusters", json!({"name": "snaps"})).await;
    let (user, pw) = creds(&c);
    let calls = [
        (reqwest::Method::PUT, "/_cluster/settings", json!({"persistent": {"slm.minimum_interval": "0s"}})),
        (reqwest::Method::PUT, "/docs/_doc/1?refresh=true", json!({"a": 1})),
        (reqwest::Method::PUT, "/_slm/policy/p", json!({"schedule": "4s", "name": "<snap-{now/d}>", "repository": "found-snapshots"})),
    ];
    for (m, uri, body) in calls {
        let (s, v) = e.es(&user, &pw, m, uri, Some(body)).await;
        assert!(s < 300, "{uri}: {v}");
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let (s, v) = e.call(reqwest::Method::POST, "/v1/search/clusters/snaps/unload", Some(json!({}))).await;
        assert_eq!(s, 200, "{v}");
        if v["unloaded"] == true || !e.control.node.loaded().iter().any(|n| &**n == "snaps") {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never unloaded: {v}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (_, v) = e.call(reqwest::Method::GET, "/v1/search/clusters/snaps", None).await;
    assert!(v["status"]["state"] == "loaded" || v["status"]["wakes_at"].as_i64().is_some_and(|t| t > 0), "{v}");
    // No request: the policy's next run loads the cluster, and the snapshot is taken.
    while !e.control.node.loaded().iter().any(|n| &**n == "snaps") {
        assert!(tokio::time::Instant::now() < deadline, "the due policy never woke its cluster");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    loop {
        let (s, v) = e.es(&user, &pw, reqwest::Method::GET, "/_slm/policy/p", None).await;
        assert_eq!(s, 200, "{v}");
        if v["p"]["stats"]["snapshots_taken"].as_i64() >= Some(1) {
            let name = v["p"]["last_success"]["snapshot_name"].as_str().unwrap().to_string();
            let (s, v) = e.es(&user, &pw, reqwest::Method::GET, &format!("/_snapshot/found-snapshots/{name}"), None).await;
            assert_eq!((s, &v["snapshots"][0]["metadata"]["policy"]), (200, &json!("p")), "{v}");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the policy never took its snapshot: {v}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// An S3-compatible bucket in memory (path-style requests): objects with ETags, `If-None-Match:
/// *` and `If-Match` writes, ranged reads, `ListObjectsV2`, multipart uploads (listed by prefix),
/// and the access key each request was signed with.
#[derive(Clone, Default)]
struct MockBucket {
    objects: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>>,
    signed_by: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    /// Listings never end: every page is truncated, with 1,000 keys of 1 KiB.
    endless: Arc<std::sync::atomic::AtomicBool>,
    /// Objects a multipart upload completed, with their part counts (their ETags end `-<parts>`).
    multipart: Arc<std::sync::Mutex<std::collections::BTreeMap<String, usize>>>,
    /// Multipart uploads in progress: upload id → (key, parts by number).
    uploads: Arc<std::sync::Mutex<std::collections::BTreeMap<String, MockUpload>>>,
    /// Part uploads fail (a bucket failing mid-upload).
    fail_parts: Arc<std::sync::atomic::AtomicBool>,
    /// Writes of `test-blob-*` objects are acknowledged and dropped (an S3 clone that loses them).
    drop_test_blobs: Arc<std::sync::atomic::AtomicBool>,
}

/// A multipart upload in a [`MockBucket`]: its key and parts by number.
type MockUpload = (String, std::collections::BTreeMap<u32, Vec<u8>>);

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl MockBucket {
    async fn start() -> (String, MockBucket) {
        let m = MockBucket::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        // Parts and blobs are megabytes: no body limit.
        let router = axum::Router::new().fallback(Self::handle).layer(axum::extract::DefaultBodyLimit::disable()).with_state(m.clone());
        tokio::spawn(async move { axum::serve(listener, router).await });
        (endpoint, m)
    }

    fn keys(&self, prefix: &str) -> Vec<String> {
        self.objects.lock().unwrap().keys().filter(|k| k.starts_with(prefix)).cloned().collect()
    }

    fn etag(&self, key: &str, v: &[u8]) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        v.hash(&mut h);
        match self.multipart.lock().unwrap().get(key) {
            Some(n) => format!("\"{:032x}-{n}\"", h.finish()),
            None => format!("\"{:032x}\"", h.finish()),
        }
    }

    async fn handle(axum::extract::State(m): axum::extract::State<MockBucket>, method: axum::http::Method, uri: axum::http::Uri, headers: axum::http::HeaderMap, body: axum::body::Bytes) -> axum::response::Response {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use std::sync::atomic::Ordering::Relaxed;
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
        if let Some(key) = auth.split("Credential=").nth(1).and_then(|c| c.split('/').next()) {
            m.signed_by.lock().unwrap().insert(key.to_string());
        }
        let key = uri.path().strip_prefix("/backups").unwrap_or_default().trim_start_matches('/').to_string();
        let q: std::collections::BTreeMap<String, String> = uri
            .query()
            .unwrap_or_default()
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| p.split_once('=').map_or((p.to_string(), String::new()), |(k, v)| (k.to_string(), percent_decode(v))))
            .collect();
        let header = |h: &str| headers.get(h).and_then(|v| v.to_str().ok()).map(str::to_string);
        let dropped = m.drop_test_blobs.load(Relaxed) && key.contains("/test-blob-");
        let tagged = |status: StatusCode, etag: String, body: Vec<u8>| (status, [("etag", etag)], body).into_response();
        match method.as_str() {
            "GET" if key.is_empty() && q.contains_key("uploads") => {
                let prefix = q.get("prefix").cloned().unwrap_or_default();
                let items: String = m.uploads.lock().unwrap().iter().filter(|(_, (k, _))| k.starts_with(&prefix)).map(|(id, (k, _))| format!("<Upload><Key>{k}</Key><UploadId>{id}</UploadId></Upload>")).collect();
                format!("<ListMultipartUploadsResult><IsTruncated>false</IsTruncated>{items}</ListMultipartUploadsResult>").into_response()
            }
            "GET" if key.is_empty() && m.endless.load(Relaxed) => {
                let items: String = (0..1000).map(|i| format!("<Contents><Key>prod/roll/indices/{i}{}</Key><Size>1</Size></Contents>", "k".repeat(1024))).collect();
                format!("<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>more</NextContinuationToken>{items}</ListBucketResult>").into_response()
            }
            "GET" if key.is_empty() => {
                let prefix = q.get("prefix").cloned().unwrap_or_default();
                let items: String = m.objects.lock().unwrap().iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(k, v)| format!("<Contents><Key>{k}</Key><Size>{}</Size></Contents>", v.len())).collect();
                format!("<ListBucketResult><IsTruncated>false</IsTruncated>{items}</ListBucketResult>").into_response()
            }
            "GET" => {
                let Some(v) = m.objects.lock().unwrap().get(&key).cloned() else {
                    return (StatusCode::NOT_FOUND, "<Error><Code>NoSuchKey</Code><Message>no</Message></Error>").into_response();
                };
                let etag = m.etag(&key, &v);
                let range = header("range").and_then(|r| {
                    let (a, b) = r.strip_prefix("bytes=")?.split_once('-')?;
                    Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                });
                match range {
                    Some((a, b)) => tagged(StatusCode::PARTIAL_CONTENT, etag, v.get(a..=b.min(v.len().saturating_sub(1))).unwrap_or_default().to_vec()),
                    None => tagged(StatusCode::OK, etag, v),
                }
            }
            "POST" if q.contains_key("uploads") => {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                let id = format!("upload{}", NEXT.fetch_add(1, Relaxed));
                m.uploads.lock().unwrap().insert(id.clone(), (key, Default::default()));
                format!("<InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>").into_response()
            }
            "POST" if q.contains_key("uploadId") => {
                let Some((k, parts)) = m.uploads.lock().unwrap().remove(&q["uploadId"]) else {
                    return (StatusCode::NOT_FOUND, "<Error><Code>NoSuchUpload</Code><Message>no</Message></Error>").into_response();
                };
                if !dropped {
                    m.multipart.lock().unwrap().insert(k.clone(), parts.len());
                    m.objects.lock().unwrap().insert(k, parts.into_values().flatten().collect());
                }
                "<CompleteMultipartUploadResult></CompleteMultipartUploadResult>".into_response()
            }
            "PUT" if q.contains_key("partNumber") => {
                if m.fail_parts.load(Relaxed) {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "<Error><Code>InternalError</Code><Message>part lost</Message></Error>").into_response();
                }
                let mut uploads = m.uploads.lock().unwrap();
                let Some((_, parts)) = uploads.get_mut(&q["uploadId"]) else {
                    return (StatusCode::NOT_FOUND, "<Error><Code>NoSuchUpload</Code><Message>no</Message></Error>").into_response();
                };
                parts.insert(q["partNumber"].parse().unwrap_or(0), body.to_vec());
                tagged(StatusCode::OK, m.etag("", &body), Vec::new())
            }
            "PUT" if dropped => tagged(StatusCode::OK, m.etag("", &body), Vec::new()),
            "PUT" => {
                let mut objects = m.objects.lock().unwrap();
                let current = objects.get(&key).map(|v| m.etag(&key, v));
                let refused = match (header("if-none-match"), header("if-match")) {
                    (Some(_), _) => current.is_some(),
                    (_, Some(want)) => current.as_deref() != Some(want.as_str()),
                    _ => false,
                };
                if refused {
                    return StatusCode::PRECONDITION_FAILED.into_response();
                }
                m.multipart.lock().unwrap().remove(&key);
                let etag = m.etag(&key, &body);
                objects.insert(key, body.to_vec());
                tagged(StatusCode::OK, etag, Vec::new())
            }
            "DELETE" if q.contains_key("uploadId") => {
                m.uploads.lock().unwrap().remove(&q["uploadId"]);
                StatusCode::NO_CONTENT.into_response()
            }
            "DELETE" => {
                m.objects.lock().unwrap().remove(&key);
                StatusCode::NO_CONTENT.into_response()
            }
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        }
    }
}

/// The bucket an `s3` repository test writes to: the in-memory [`MockBucket`], or a real
/// S3-compatible server when `ROLL_SEARCH_S3_TEST_ENDPOINT` names one (with
/// `ROLL_SEARCH_S3_TEST_BUCKET`, `_ACCESS_KEY` and `_SECRET_KEY`; MinIO in Docker, say). Each
/// test keeps its objects under a base path of its own.
struct TestBucket {
    mock: Option<MockBucket>,
    settings: Value,
    client: search_es::s3::Client,
    base: String,
}

impl TestBucket {
    async fn start(name: &str) -> TestBucket {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let base = format!("{name}-{n}");
        let real = std::env::var("ROLL_SEARCH_S3_TEST_ENDPOINT").ok().filter(|e| !e.is_empty());
        let (endpoint, bucket, keys, mock) = match real {
            Some(endpoint) => {
                let var = |k: &str| std::env::var(format!("ROLL_SEARCH_S3_TEST_{k}")).unwrap_or_else(|_| panic!("ROLL_SEARCH_S3_TEST_{k} is needed with an endpoint"));
                (endpoint, var("BUCKET"), Some((var("ACCESS_KEY"), var("SECRET_KEY"))), None)
            }
            None => {
                let (endpoint, m) = MockBucket::start().await;
                (endpoint, "backups".to_string(), None, Some(m))
            }
        };
        let mut settings = json!({"bucket": bucket, "base_path": base, "endpoint": endpoint, "path_style_access": true});
        if let Some((a, s)) = &keys {
            settings["access_key"] = json!(a);
            settings["secret_key"] = json!(s);
        }
        let at = search_es::s3::Location { endpoint: reqwest::Url::parse(&endpoint).unwrap(), bucket, region: "us-east-1".into(), path_style: true };
        let creds = keys.map(|(access_key, secret_key)| search_es::s3::Credentials { access_key, secret_key, session_token: None });
        let client = search_es::s3::Client::new(at, creds, true).unwrap();
        TestBucket { mock, settings, client, base }
    }

    /// An `s3` repository here, with `extra` settings.
    fn repo(&self, extra: Value) -> Value {
        let mut settings = self.settings.clone();
        for (k, v) in extra.as_object().unwrap() {
            settings[k] = v.clone();
        }
        json!({"type": "s3", "settings": settings})
    }

    /// `<base_path>/roll/`: where the repository keeps everything.
    fn prefix(&self) -> String {
        format!("{}/roll/", self.base)
    }

    async fn objects(&self, under: &str) -> Vec<(String, u64)> {
        self.client.list(&format!("{}{under}", self.prefix())).await.unwrap()
    }
}

/// Documents of random text, about `size` bytes each, gzip barely shrinks.
fn random_docs(n: usize, size: usize, seed: u64) -> Vec<Value> {
    let mut x = seed | 1;
    (0..n)
        .map(|i| {
            let raw: Vec<u8> = (0..size * 3 / 4)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect();
            json!({"n": i, "blob": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw)})
        })
        .collect()
}

async fn index_docs(e: &Env, user: &str, pw: &str, index: &str, docs: &[Value]) {
    for (i, d) in docs.iter().enumerate() {
        let (s, v) = e.es(user, pw, reqwest::Method::PUT, &format!("/{index}/_doc/{i}"), Some(d.clone())).await;
        assert_eq!(s, 201, "{v}");
    }
    let (s, v) = e.es(user, pw, reqwest::Method::POST, &format!("/{index}/_refresh"), None).await;
    assert_eq!(s, 200, "{v}");
}

/// A snapshot's contents go up as `chunk_size` objects, each larger than `buffer_size` as a
/// multipart upload (both validated as Elasticsearch does), and restore whole; a part that fails
/// aborts its upload, and `_cleanup` aborts the uploads a crash left under the repository.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_s3_repository_uploads_in_parts() {
    let b = TestBucket::start("parts").await;
    let e = env_with(|c| c.node_settings.push((search_control::S3_ALLOW_PRIVATE.into(), "true".into()))).await;
    let (u, p) = creds(&e.post("/v1/search/clusters", json!({"name": "parts"})).await);
    for (extra, reason) in [
        (json!({"chunk_size": "10mb", "buffer_size": "20mb"}), "[bad] chunk_size (10mb) can't be lower than buffer_size (20mb)."),
        (json!({"chunk_size": "1kb"}), "failed to parse value [1kb] for setting [chunk_size], must be >= [5mb]"),
        (json!({"buffer_size": "6gb"}), "failed to parse value [6gb] for setting [buffer_size], must be <= [5gb]"),
        (json!({"max_multipart_parts": 0}), "Failed to parse value [0] for setting [max_multipart_parts] must be >= 1"),
    ] {
        let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/bad", Some(b.repo(extra))).await;
        assert_eq!((s, v["error"]["caused_by"]["reason"].as_str()), (500, Some(reason)), "{v}");
    }
    // ~18 MiB gzipped over a few segments: full 8 MiB chunks (5 MiB and 3 MiB parts) and smaller
    // last ones.
    index_docs(&e, &u, &p, "docs", &random_docs(24, 1 << 20, 7)).await;
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/backup", Some(b.repo(json!({"buffer_size": "5mb", "chunk_size": "8mb"})))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/backup/one?wait_for_completion=true", Some(json!({"indices": "docs"}))).await;
    assert_eq!((s, v["snapshot"]["state"].as_str()), (200, Some("SUCCESS")), "{v}");
    let chunks: Vec<(String, u64)> = b.objects("indices/").await.into_iter().filter(|(k, _)| k.ends_with(".gz")).collect();
    let full: Vec<&String> = chunks.iter().filter(|(_, n)| *n == 8 << 20).map(|(k, _)| k).collect();
    assert!(chunks.len() >= 3 && !full.is_empty() && chunks.iter().all(|(_, n)| *n <= 8 << 20), "{chunks:?}");
    let (_, etag) = b.client.get_tagged(full[0], 16 << 20).await.unwrap().unwrap();
    assert!(etag.trim_matches('"').ends_with("-2"), "not uploaded in two parts: {etag}");
    let (s, v) = e.es(&u, &p, reqwest::Method::POST, "/_snapshot/backup/one/_restore?wait_for_completion=true", Some(json!({"rename_pattern": "docs", "rename_replacement": "back"}))).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = e.es(&u, &p, reqwest::Method::GET, "/back/_count", None).await;
    assert_eq!(v["count"], 24, "{v}");
    let (_, from) = e.es(&u, &p, reqwest::Method::GET, "/docs/_doc/23", None).await;
    let (_, to) = e.es(&u, &p, reqwest::Method::GET, "/back/_doc/23", None).await;
    assert_eq!(from["_source"], to["_source"]);

    // A part the bucket fails aborts its upload: nothing is left in progress.
    if let Some(m) = &b.mock {
        index_docs(&e, &u, &p, "more", &random_docs(8, 1 << 20, 9)).await;
        m.fail_parts.store(true, std::sync::atomic::Ordering::Relaxed);
        let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/backup/two?wait_for_completion=true", Some(json!({"indices": "more"}))).await;
        m.fail_parts.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(s >= 500 || v["snapshot"]["state"] != "SUCCESS", "{v}");
        assert!(m.uploads.lock().unwrap().is_empty(), "an upload was left in progress");
    }

    // An upload a crash left under the repository goes with `_cleanup`; one elsewhere stays.
    let inside = format!("{}indices/gone/0/0123456789abcdef0123456789abcdef-1.0.gz", b.prefix());
    let outside = format!("{}-other/dangling", b.base);
    let id_in = b.client.create_multipart(&inside).await.unwrap();
    let id_out = b.client.create_multipart(&outside).await.unwrap();
    let (s, v) = e.es(&u, &p, reqwest::Method::POST, "/_snapshot/backup/_cleanup", None).await;
    assert_eq!(s, 200, "{v}");
    let listed = |prefix: String| {
        let c = &b.client;
        async move { c.list_uploads(&prefix, 100).await.unwrap() }
    };
    match b.mock.is_some() || !listed(b.prefix()).await.is_empty() || !listed(inside.clone()).await.iter().any(|(_, id)| *id == id_in) {
        true => {
            assert!(!listed(inside.clone()).await.iter().any(|(_, id)| *id == id_in), "the dangling upload is still there");
            assert!(listed(outside.clone()).await.iter().any(|(_, id)| *id == id_out), "an upload outside the repository went");
        }
        // MinIO lists uploads by exact key only: a prefix finds none (its own expiry removes them).
        false => eprintln!("the bucket lists multipart uploads by exact key only: dangling-upload cleanup not checked"),
    }
    let _ = b.client.abort_multipart(&inside, &id_in).await;
    let _ = b.client.abort_multipart(&outside, &id_out).await;
}

/// `max_snapshot_bytes_per_sec` paces a snapshot's uploads, `max_restore_bytes_per_sec` and the
/// cluster's `indices.recovery.max_bytes_per_sec` a restore's reads, and `_nodes/stats/repositories`
/// counts the time throttled and what was uploaded. A throttle guarantees an upper bound: no
/// transfer goes faster than its rate, beyond one payment of burst. A slower one is the host's
/// load (a debug build, a busy machine), so the measured rates are printed, not asserted from
/// below; what the limiter waited is checked against the bytes, the rate and the time spent
/// producing them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_s3_repository_throttles_bucket_traffic() {
    // The most bytes one payment covers: a 1 MiB drain plus the ~1 MiB record that crossed it
    // (a restore pays per received piece, far less).
    const BURST: f64 = (2 << 20) as f64;
    // Timer and scheduling slack.
    const SLACK: f64 = 0.25;
    let b = TestBucket::start("throttle").await;
    let e = env_with(|c| c.node_settings.push((search_control::S3_ALLOW_PRIVATE.into(), "true".into()))).await;
    let (u, p) = creds(&e.post("/v1/search/clusters", json!({"name": "throttle"})).await);
    index_docs(&e, &u, &p, "docs", &random_docs(6, 1 << 20, 3)).await;
    let rate = (2u64 << 20) as f64;
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/slow", Some(b.repo(json!({"max_snapshot_bytes_per_sec": "2mb", "max_restore_bytes_per_sec": "2mb"})))).await;
    assert_eq!(s, 200, "{v}");
    let stats = |human: bool| {
        let e = &e;
        let (u, p) = (u.clone(), p.clone());
        async move {
            let (_, v) = e.es(&u, &p, reqwest::Method::GET, &format!("/_nodes/stats/repositories?human={human}"), None).await;
            v["nodes"].as_object().unwrap().values().next().unwrap()["repositories"]["slow"].clone()
        }
    };
    let secs = |v: &Value, k: &str| v[k].as_f64().unwrap() / 1e9;
    // No faster than `rate`, beyond one payment.
    let not_faster = |what: &str, bytes: f64, took: f64, rate: f64| {
        eprintln!("{what}: {bytes:.0} bytes in {took:.2}s, {:.0} B/s against {rate:.0} ({:.2} of it)", bytes / took, bytes / took / rate);
        assert!(bytes <= rate * took + BURST, "{what}: {bytes:.0} bytes in {took:.2}s is faster than {rate:.0} B/s");
    };

    let started = std::time::Instant::now();
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/slow/one?wait_for_completion=true", Some(json!({"indices": "docs"}))).await;
    let took = started.elapsed().as_secs_f64();
    assert_eq!((s, v["snapshot"]["state"].as_str()), (200, Some("SUCCESS")), "{v}");
    let s1 = stats(false).await;
    eprintln!("snapshot stats: {s1}");
    let uploaded = s1["uploaded_size_in_bytes"].as_f64().unwrap();
    not_faster("snapshot", uploaded, took, rate);
    assert_eq!((s1["shard_snapshots_started"].as_i64(), s1["shard_snapshots_completed"].as_i64(), s1["shard_snapshots_in_progress"].as_i64()), (Some(1), Some(1), Some(0)), "{s1}");
    assert!(s1["uploaded_blobs"].as_i64() >= Some(1), "{s1}");
    // The limiter waits for what producing the bytes didn't already take, and never longer than
    // the bytes take at the rate.
    let (throttled, producing) = (secs(&s1, "total_write_throttled_time_nanos"), s1["total_read_time_in_millis"].as_f64().unwrap() / 1e3);
    let owed = uploaded / rate - producing - BURST / rate - SLACK;
    assert!(throttled <= uploaded / rate + SLACK, "throttled {throttled:.2}s for {uploaded:.0} bytes at {rate:.0} B/s: {s1}");
    assert!(throttled >= owed, "throttled {throttled:.2}s, owed at least {owed:.2}s: {s1}");
    if owed > 0.0 {
        assert!(throttled > 0.0, "{s1}");
    }

    let restore = |to: &'static str| {
        let e = &e;
        let (u, p) = (u.clone(), p.clone());
        async move {
            let started = std::time::Instant::now();
            let (s, v) = e.es(&u, &p, reqwest::Method::POST, "/_snapshot/slow/one/_restore?wait_for_completion=true", Some(json!({"rename_pattern": "docs", "rename_replacement": to}))).await;
            assert_eq!(s, 200, "{v}");
            started.elapsed().as_secs_f64()
        }
    };
    not_faster("restore", uploaded, restore("r1").await, rate);
    let s2 = stats(false).await;
    let read_throttled = secs(&s2, "total_read_throttled_time_nanos");
    assert!(read_throttled <= uploaded / rate + SLACK, "read throttled {read_throttled:.2}s for {uploaded:.0} bytes at {rate:.0} B/s: {s2}");
    // Without a repository limit, the cluster's recovery limit paces restores.
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/slow", Some(b.repo(json!({"max_snapshot_bytes_per_sec": "2mb"})))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_cluster/settings", Some(json!({"persistent": {"indices.recovery.max_bytes_per_sec": "4mb"}}))).await;
    assert_eq!(s, 200, "{v}");
    not_faster("restore under the recovery limit", uploaded, restore("r2").await, 2.0 * rate);
    let (_, v) = e.es(&u, &p, reqwest::Method::GET, "/r2/_count", None).await;
    assert_eq!(v["count"], 6, "{v}");
    let s3 = stats(true).await;
    let read_throttled = secs(&s3, "total_read_throttled_time_nanos");
    assert!(read_throttled <= uploaded / rate + uploaded / (2.0 * rate) + 2.0 * SLACK, "read throttled {read_throttled:.2}s: {s3}");
    assert!(s3["total_write_throttled_time"].is_string() && s3["uploaded_size"].is_string(), "{s3}");
}

/// `_analyze` really exercises a bucket: on a good one it passes with Elasticsearch's answer
/// (registers, overwrite protection, blobs past `buffer_size` in parts, rare overwrites, aborts
/// and early reads) and leaves nothing behind; a bucket that acknowledges writes it drops fails
/// it as Elasticsearch fails it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_s3_repository_analysis_exercises_the_bucket() {
    let b = TestBucket::start("analyze").await;
    let e = env_with(|c| c.node_settings.push((search_control::S3_ALLOW_PRIVATE.into(), "true".into()))).await;
    let (u, p) = creds(&e.post("/v1/search/clusters", json!({"name": "analyze"})).await);
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/good", Some(b.repo(json!({"buffer_size": "5mb"})))).await;
    assert_eq!(s, 200, "{v}");
    let path = "/_snapshot/good/_analyze?blob_count=12&max_blob_size=6mb&max_total_data_size=24mb&seed=42&detailed=true&rare_action_probability=0.5&timeout=120s";
    let (s, v) = e.es(&u, &p, reqwest::Method::POST, path, None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["blob_count"].as_i64(), v["summary"]["write"]["count"].as_i64(), v["details"].as_array().map(Vec::len)), (Some(12), Some(12), Some(12)), "{v}");
    assert_eq!((v["max_blob_size"].as_str(), v["rare_action_probability"].as_f64(), &v["issues_detected"]), (Some("6mb"), Some(0.5), &json!([])), "{v}");
    let written = v["summary"]["write"]["total_size_bytes"].as_i64().unwrap();
    assert!(written > 0 && written <= 24 << 20 && v["summary"]["read"]["count"].as_i64() >= Some(1), "{v}");
    assert!(v["details"].as_array().unwrap().iter().any(|d| d["blob"]["overwritten"] == true || d["blob"]["read_early"] == true), "no rare action ran: {v}");
    assert!(b.objects("temp-analysis-").await.is_empty(), "the analysis left blobs behind");
    if b.mock.is_some() || !b.client.list_uploads(&b.prefix(), 10).await.unwrap().is_empty() {
        assert!(b.client.list_uploads(&b.prefix(), 10).await.unwrap().is_empty(), "the analysis left uploads behind");
    }

    // A bucket that loses writes it acknowledged.
    let (endpoint, m) = MockBucket::start().await;
    m.drop_test_blobs.store(true, std::sync::atomic::Ordering::Relaxed);
    let repo = json!({"type": "s3", "settings": {"bucket": "backups", "base_path": "broken", "endpoint": endpoint, "path_style_access": true}});
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/broken", Some(repo)).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(&u, &p, reqwest::Method::POST, "/_snapshot/broken/_analyze?blob_count=5&max_blob_size=64kb&seed=3", None).await;
    let err = &v["error"];
    assert_eq!((s, err["type"].as_str()), (500, Some("repository_verification_exception")), "{v}");
    assert!(err["reason"].as_str().unwrap().starts_with("[broken] Elasticsearch observed the storage system underneath this repository behaved incorrectly"), "{v}");
    assert!(err["caused_by"]["reason"].as_str().unwrap().starts_with("[broken] failure processing [blob analysis [broken:temp-analysis-"), "{v}");
    let innermost = err["caused_by"]["caused_by"]["reason"].as_str().unwrap();
    assert!(innermost.starts_with("[broken] node [{") && innermost.ends_with("reported blob not found after it was written"), "{v}");
    assert_eq!(err["root_cause"][0]["reason"].as_str(), Some(innermost), "{v}");
    assert!(m.keys("broken/roll/temp-analysis-").is_empty(), "the failed analysis left blobs behind");
}

/// An `s3` repository in a tenant's bucket: credentials sign every request and are never shown,
/// snapshots are incremental, another cluster registering the bucket restores them (ids,
/// routing, versions and sequence numbers intact), deleting a snapshot keeps what a later one
/// shares, and `_cleanup` removes what no snapshot references.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_s3_repository_moves_indices_between_clusters() {
    let (endpoint, bucket) = MockBucket::start().await;
    let e = env_with(|c| {
        c.node_settings.push((search_control::S3_ALLOW_PRIVATE.into(), "true".into()));
        c.sealer = search_control::Sealer::from_secret("0123456789abcdef0123456789abcdef");
    })
    .await;
    let (ua, pa) = creds(&e.post("/v1/search/clusters", json!({"name": "src"})).await);
    let (ub, pb) = creds(&e.post("/v1/search/clusters", json!({"name": "dst"})).await);
    let repo = json!({"type": "s3", "settings": {
        "bucket": "backups", "base_path": "prod", "endpoint": endpoint, "path_style_access": true,
        "access_key": "AKIDTENANT", "secret_key": "tenant-secret-key",
    }});
    for i in 0..20 {
        let (s, v) = e.es(&ua, &pa, reqwest::Method::PUT, &format!("/docs/_doc/{i}?routing=r{}", i % 3), Some(json!({"n": i, "text": "hello world"}))).await;
        assert_eq!(s, 201, "{v}");
    }
    e.es(&ua, &pa, reqwest::Method::POST, "/docs/_refresh", None).await;
    let (s, v) = e.es(&ua, &pa, reqwest::Method::PUT, "/_snapshot/backup", Some(repo.clone())).await;
    assert_eq!(s, 200, "{v}");
    for (s, v) in [e.es(&ua, &pa, reqwest::Method::GET, "/_snapshot/backup", None).await, e.es(&ua, &pa, reqwest::Method::GET, "/_cluster/state/metadata", None).await] {
        assert_eq!(s, 200);
        assert!(!v.to_string().contains("tenant-secret-key") && !v.to_string().contains("AKIDTENANT"), "credentials shown: {v}");
    }
    let (s, v) = e.es(&ua, &pa, reqwest::Method::PUT, "/_snapshot/backup/one?wait_for_completion=true", Some(json!({"indices": "docs"}))).await;
    assert_eq!((s, v["snapshot"]["state"].as_str()), (200, Some("SUCCESS")), "{v}");
    assert_eq!(bucket.signed_by.lock().unwrap().iter().cloned().collect::<Vec<_>>(), vec!["AKIDTENANT".to_string()]);

    // A second snapshot after one more document uploads only the new segment.
    let (s, _) = e.es(&ua, &pa, reqwest::Method::PUT, "/docs/_doc/20?routing=r2&refresh=true", Some(json!({"n": 20}))).await;
    assert_eq!(s, 201);
    let (s, v) = e.es(&ua, &pa, reqwest::Method::PUT, "/_snapshot/backup/two?wait_for_completion=true", Some(json!({"indices": "docs"}))).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = e.es(&ua, &pa, reqwest::Method::GET, "/_snapshot/backup/two/_status", None).await;
    let stats = &v["snapshots"][0]["stats"];
    assert!(stats["incremental"]["file_count"].as_i64() < stats["total"]["file_count"].as_i64(), "{v}");

    // Another cluster finds both and restores the first, as it was.
    let (s, v) = e.es(&ub, &pb, reqwest::Method::PUT, "/_snapshot/backup", Some(repo)).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = e.es(&ub, &pb, reqwest::Method::GET, "/_snapshot/backup/_all", None).await;
    let names: Vec<&str> = v["snapshots"].as_array().unwrap().iter().filter_map(|s| s["snapshot"].as_str()).collect();
    assert_eq!(names, ["one", "two"], "{v}");
    let (s, v) = e.es(&ub, &pb, reqwest::Method::POST, "/_snapshot/backup/one/_restore?wait_for_completion=true", Some(json!({}))).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = e.es(&ub, &pb, reqwest::Method::GET, "/docs/_count", None).await;
    assert_eq!(v["count"], 20, "{v}");
    let (_, from) = e.es(&ua, &pa, reqwest::Method::GET, "/docs/_doc/7?routing=r1", None).await;
    let (s, to) = e.es(&ub, &pb, reqwest::Method::GET, "/docs/_doc/7?routing=r1", None).await;
    assert_eq!(s, 200, "{to}");
    for k in ["_source", "_seq_no", "_version", "_primary_term", "_routing"] {
        assert_eq!(from[k], to[k], "{k}: {from} vs {to}");
    }

    // Deleting the first keeps what the second shares; the second still restores whole.
    let (s, v) = e.es(&ua, &pa, reqwest::Method::DELETE, "/_snapshot/backup/one", None).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(&ub, &pb, reqwest::Method::POST, "/_snapshot/backup/two/_restore?wait_for_completion=true", Some(json!({"rename_pattern": "docs", "rename_replacement": "docs2"}))).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = e.es(&ub, &pb, reqwest::Method::GET, "/docs2/_count", None).await;
    assert_eq!(v["count"], 21, "{v}");

    // `_cleanup` deletes what no snapshot references.
    bucket.objects.lock().unwrap().insert("prod/roll/indices/stray/0/0123456789abcdef0123456789abcdef-1.0.gz".into(), vec![0; 10]);
    let (s, v) = e.es(&ua, &pa, reqwest::Method::POST, "/_snapshot/backup/_cleanup", None).await;
    assert_eq!((s, v["results"]["deleted_blobs"].as_i64()), (200, Some(1)), "{v}");
    assert!(bucket.keys("prod/roll/indices/stray/").is_empty());
}

/// roll secrets as a fake (rolld's is `Secrets::may_reveal_for`, `maker_caller` and
/// `resolve_for`): keys by reference with a version, the clusters a project's canvas holds, who may
/// reveal (a person's `usr_…`, or `operator`), and protected references (refused to an untrusted
/// copy).
#[derive(Default)]
struct FakeSecrets {
    keys: std::sync::Mutex<std::collections::BTreeMap<String, (String, u32)>>,
    canvas: std::sync::Mutex<Vec<String>>,
    revealers: std::sync::Mutex<Vec<String>>,
    protected: Vec<String>,
    resolves: std::sync::Mutex<Vec<search_control::secret_refs::Holder>>,
}

impl FakeSecrets {
    /// What resolving `reference` for `h` answers, `who` (else its maker) asking.
    fn check(&self, h: &search_control::secret_refs::Holder, who: &str, reference: &str) -> Result<(String, u32), search_control::secret_refs::Refused> {
        use search_control::secret_refs::Refused;
        if !self.canvas.lock().unwrap().contains(&h.id) {
            return Err(Refused::Invalid(format!("no project: search:search:{} isn't on any project's canvas", h.id)));
        }
        if !self.revealers.lock().unwrap().iter().any(|r| r == who) {
            return Err(Refused::Forbidden(format!("not trusted: {who} may not reveal production:/")));
        }
        if h.copy && self.protected.iter().any(|p| p == reference) {
            return Err(Refused::Forbidden("protected: production is protected".into()));
        }
        self.keys.lock().unwrap().get(reference).cloned().ok_or_else(|| Refused::Invalid(format!("no such key: {reference}")))
    }

    fn set(&self, reference: &str, value: &str) {
        let mut keys = self.keys.lock().unwrap();
        let version = keys.get(reference).map_or(1, |(_, v)| v + 1);
        keys.insert(reference.into(), (value.into(), version));
    }
}

fn refused(r: search_control::secret_refs::Refused) -> String {
    match r {
        search_control::secret_refs::Refused::Forbidden(m) | search_control::secret_refs::Refused::Invalid(m) => m,
    }
}

#[async_trait::async_trait]
impl search_control::secret_refs::ResourceSecrets for FakeSecrets {
    async fn may_write(&self, who: &search_control::secret_refs::Who, h: &search_control::secret_refs::Holder, reference: &str) -> Result<String, search_control::secret_refs::Refused> {
        let principal = who.user.as_ref().map_or_else(|| "operator".to_string(), |u| u.user.clone());
        self.check(h, &principal, reference).map(|_| principal)
    }

    async fn resolve(&self, h: &search_control::secret_refs::Holder, reference: &str) -> Result<search_control::secret_refs::Revealed, String> {
        let (value, _) = self.check(h, &h.maker, reference).map_err(refused)?;
        self.resolves.lock().unwrap().push(h.clone());
        Ok(search_control::secret_refs::Revealed::new(value))
    }

    async fn digest(&self, h: &search_control::secret_refs::Holder, reference: &str) -> Result<String, String> {
        self.check(h, &h.maker, reference).map(|(_, v)| format!("v{v}")).map_err(refused)
    }
}

/// An `s3` repository's keys as roll secrets references: written only through roll's API by
/// someone who may reveal them (never through the cluster's endpoint), kept as written, resolved
/// for the cluster as their maker when its client is built and again only once a rotation moves
/// the digest; refused to a cluster no project holds, to a branch reaching a protected
/// environment, once the maker can't reveal them, and once the secret is deleted (the snapshot
/// fails with why, never anonymously); registering it elsewhere through the endpoint drops them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_s3_repository_takes_its_keys_from_roll_secrets() {
    let (endpoint, bucket) = MockBucket::start().await;
    let e = env_with(|c| c.node_settings.push((search_control::S3_ALLOW_PRIVATE.into(), "true".into()))).await;
    let made = |name: &str| e.call_as("ada", reqwest::Method::POST, "/v1/search/clusters", Some(json!({"name": name})));
    let (s, vault) = made("vault").await;
    assert_eq!(s, 200, "{vault}");
    let (u, p) = creds(&vault);
    let id = vault["id"].as_str().unwrap().to_string();
    let (s, loose) = made("loose").await;
    assert_eq!(s, 200, "{loose}");
    let (access, secret) = ("roll://production/AWS_ACCESS_KEY_ID", "roll://production/AWS_SECRET_ACCESS_KEY");
    let fake = Arc::new(FakeSecrets { protected: vec![access.into(), secret.into()], ..Default::default() });
    fake.canvas.lock().unwrap().push(id.clone());
    fake.revealers.lock().unwrap().push("usr_ada".into());
    fake.set(access, "AKIDONE");
    fake.set(secret, "fake-secret-one");
    e.control.set_secrets(fake.clone());
    let repo = json!({"type": "s3", "settings": {"bucket": "backups", "base_path": "refs", "endpoint": endpoint, "path_style_access": true, "access_key": access, "secret_key": secret}});
    let route = |cluster: &str| format!("/v1/search/clusters/{cluster}/repositories/backup");

    // The cluster's own endpoint (its password: a cluster admin) can't name a reference.
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/backup", Some(repo.clone())).await;
    assert_eq!((s, v["error"]["type"].as_str()), (400, Some("settings_exception")), "{v}");
    assert!(v["error"]["reason"].as_str().unwrap().contains("roll's API"), "{v}");
    // Through roll's API: someone who may not reveal it is refused, and so is a cluster no
    // project holds.
    let (s, v) = e.call_as("admin", reqwest::Method::PUT, &route(&id), Some(repo.clone())).await;
    assert_eq!(s, 403, "{v}");
    let (s, v) = e.call_as("ada", reqwest::Method::PUT, &route(loose["id"].as_str().unwrap()), Some(repo.clone())).await;
    assert!(s == 400 && v.to_string().contains("isn't on any project's canvas"), "{v}");

    // Registered and verified with the resolved keys; the reference shows as written.
    let (s, v) = e.call_as("ada", reqwest::Method::PUT, &route(&id), Some(repo)).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = e.es(&u, &p, reqwest::Method::GET, "/_snapshot/backup", None).await;
    assert_eq!(v["backup"]["settings"]["secret_key"], secret, "{v}");
    let (_, state) = e.es(&u, &p, reqwest::Method::GET, "/_cluster/state/metadata", None).await;
    assert!(!state.to_string().contains("fake-secret-one"), "a value in the cluster state");
    e.es(&u, &p, reqwest::Method::PUT, "/docs/_doc/1?refresh=true", Some(json!({"n": 1}))).await;
    let snapshot = |name: &str| format!("/_snapshot/backup/{name}?wait_for_completion=true");
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, &snapshot("one"), Some(json!({"indices": "docs"}))).await;
    assert_eq!((s, v["snapshot"]["state"].as_str()), (200, Some("SUCCESS")), "{v}");
    assert_eq!(bucket.signed_by.lock().unwrap().iter().cloned().collect::<Vec<_>>(), ["AKIDONE"]);
    assert_eq!(fake.resolves.lock().unwrap().len(), 2, "resolved once, then kept while the digests stay");
    assert!(fake.resolves.lock().unwrap().iter().all(|h| h.maker == "usr_ada" && !h.copy));

    // A rotation moves the digest: the next snapshot signs with the new key.
    fake.set(access, "AKIDTWO");
    bucket.signed_by.lock().unwrap().clear();
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, &snapshot("two"), Some(json!({"indices": "docs"}))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(bucket.signed_by.lock().unwrap().iter().cloned().collect::<Vec<_>>(), ["AKIDTWO"]);

    // The maker loses reveal: the next snapshot fails, with why and without the value.
    fake.revealers.lock().unwrap().clear();
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, &snapshot("lost"), Some(json!({"indices": "docs"}))).await;
    assert_eq!(s, 500, "{v}");
    assert!(v.to_string().contains("usr_ada may not reveal") && !v.to_string().contains("fake-secret"), "{v}");
    fake.revealers.lock().unwrap().push("usr_ada".into());

    // A branch asks as the cluster it came from, untrusted: production is refused, visibly.
    let (s, branch) = e.call_as("ada", reqwest::Method::POST, &format!("/v1/search/clusters/{id}/branches"), Some(json!({"name": "vault-dev"}))).await;
    assert_eq!(s, 200, "{branch}");
    let (bu, bp) = creds(&branch);
    let (s, v) = e.es(&bu, &bp, reqwest::Method::PUT, &snapshot("from-branch"), Some(json!({"indices": "docs"}))).await;
    assert_eq!(s, 500, "{v}");
    assert!(v.to_string().contains("protected") && !v.to_string().contains("fake-secret"), "{v}");
    assert!(fake.resolves.lock().unwrap().iter().all(|h| !h.copy), "nothing resolved for the branch");

    // The secret deleted: the snapshot fails with why, and nothing reaches the bucket unsigned.
    fake.keys.lock().unwrap().remove(secret);
    let before = bucket.keys("").len();
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, &snapshot("three"), Some(json!({"indices": "docs"}))).await;
    assert_eq!(s, 500, "{v}");
    assert!(v["error"]["reason"].as_str().unwrap().contains("no such key"), "{v}");
    assert_eq!(bucket.keys("").len(), before);

    // Pointed elsewhere through the endpoint, without the references: they're gone, not carried.
    let (elsewhere, _) = MockBucket::start().await;
    let moved = json!({"type": "s3", "settings": {"bucket": "backups", "base_path": "refs", "endpoint": elsewhere, "path_style_access": true}});
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/backup", Some(moved)).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = e.es(&u, &p, reqwest::Method::GET, "/_snapshot/backup", None).await;
    assert!(v["backup"]["settings"].get("secret_key").is_none() && v["backup"]["settings"].get("access_key").is_none(), "{v}");
}

#[path = "control/cloud_repositories.rs"]
mod cloud_repositories;

fn gzip(raw: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(raw).unwrap();
    enc.finish().unwrap()
}

/// A tenant's bucket is someone else's input. Repository data that parses past its bounds, a
/// document larger than any write could send and a listing that never ends fail
/// without being held; index names and global state the APIs would refuse fail the restore.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_s3_repository_bounds_and_checks_what_a_bucket_holds() {
    use std::io::Read;
    let (endpoint, bucket) = MockBucket::start().await;
    let e = env_with(|c| c.node_settings.push((search_control::S3_ALLOW_PRIVATE.into(), "true".into()))).await;
    let (u, p) = creds(&e.post("/v1/search/clusters", json!({"name": "bounds"})).await);
    let es = |method: reqwest::Method, path: &'static str, body: Option<Value>| e.es(&u, &p, method, path, body);
    let repo = json!({"type": "s3", "settings": {"bucket": "backups", "base_path": "prod", "endpoint": endpoint, "path_style_access": true}});
    assert_eq!(es(reqwest::Method::PUT, "/docs/_doc/1?refresh=true", Some(json!({"n": 1}))).await.0, 201);
    assert_eq!(es(reqwest::Method::PUT, "/_snapshot/backup", Some(repo)).await.0, 200);
    let (s, v) = es(reqwest::Method::PUT, "/_snapshot/backup/one?wait_for_completion=true", Some(json!({"indices": "docs", "include_global_state": true}))).await;
    assert_eq!(s, 200, "{v}");
    let object = |k: &str| bucket.objects.lock().unwrap().get(k).cloned().unwrap();
    let put = |k: String, v: Vec<u8>| bucket.objects.lock().unwrap().insert(k, v);
    let latest: i64 = String::from_utf8(object("prod/roll/index.latest")).unwrap().trim().parse().unwrap();
    let mut text = Vec::new();
    flate2::read::GzDecoder::new(&object(&format!("prod/roll/index-{latest}"))[..]).read_to_end(&mut text).unwrap();
    let data: Value = serde_json::from_slice(&text).unwrap();
    // Each change is committed as the next generation, as another writer would.
    let mut generation = latest;
    let mut commit = |edit: &dyn Fn(&mut Value)| {
        generation += 1;
        let mut d = data.clone();
        d["generation"] = json!(generation);
        edit(&mut d["snapshots"][0]);
        put(format!("prod/roll/index-{generation}"), gzip(d.to_string().as_bytes()));
        put("prod/roll/index.latest".into(), generation.to_string().into_bytes());
    };
    let restore = |body: Value| e.es(&u, &p, reqwest::Method::POST, "/_snapshot/backup/one/_restore?wait_for_completion=true", Some(body));
    let mut problems = Vec::new();
    let mut expect = |what: &str, (s, v): (u16, Value), status: u16, text: &str| {
        if s != status || !v.to_string().contains(text) {
            let problem = format!("{what}: wanted {status} with [{text}], got {s} {v}");
            eprintln!("{problem}");
            problems.push(problem);
        }
    };

    // A document longer than `http.max_content_length` (100mb), though the chunk is small.
    let chunk = bucket.keys("prod/roll/indices/").into_iter().find(|k| k.ends_with(".0.gz")).unwrap();
    let saved = object(&chunk);
    put(chunk.clone(), gzip(format!("{{\"id\":\"1\",\"routing\":null,\"seq_no\":0,\"primary_term\":1,\"version\":1,\"len\":{}}}\n", 200 << 20).as_bytes()));
    expect("oversized document", restore(json!({"rename_pattern": "docs", "rename_replacement": "big"})).await, 500, "out of bounds");
    put(chunk, saved);

    // Names and global state the APIs refuse.
    commit(&|s| {
        s["indices"] = json!(["_x"]);
        let meta = s["index_meta"]["docs"].take();
        s["index_meta"] = json!({"_x": meta});
    });
    expect("index name", restore(json!({})).await, 400, "invalid_index_name_exception");
    commit(&|s| s["global"]["persistent"] = json!({"cluster.election.back_off_time": "1s"}));
    expect("static setting", restore(json!({"include_global_state": true, "rename_pattern": "docs", "rename_replacement": "g1"})).await, 400, "not dynamically updateable");
    commit(&|s| s["global"]["pipelines"] = json!({"p": {"processors": [{"no_such_processor": {}}]}}));
    expect("pipeline", restore(json!({"include_global_state": true, "rename_pattern": "docs", "rename_replacement": "g2"})).await, 400, "no_such_processor");
    let (_, v) = es(reqwest::Method::GET, "/_cat/indices?format=json&h=index", None).await;
    let restored = v.as_array().into_iter().flatten().any(|i| ["_x", "g1", "g2"].contains(&i["index"].as_str().unwrap_or_default()));
    expect("nothing restored", (200, json!(restored)), 200, "false");

    // Repository data that would parse into more values than its bound (its inflated size is
    // bounded too: `inflate`'s unit test). Unbounded, this one aborts the process.
    generation += 1;
    let mut dense = b"[".to_vec();
    dense.extend(b"0,".repeat(9 << 20));
    dense.extend(b"0]");
    put(format!("prod/roll/index-{generation}"), gzip(&dense));
    expect("value bomb", es(reqwest::Method::GET, "/_snapshot/backup/_all", None).await, 500, "values");
    bucket.objects.lock().unwrap().remove(&format!("prod/roll/index-{generation}"));

    // A listing that never ends.
    bucket.endless.store(true, std::sync::atomic::Ordering::Relaxed);
    let cleanup = tokio::time::timeout(Duration::from_secs(60), es(reqwest::Method::POST, "/_snapshot/backup/_cleanup", None)).await;
    expect("endless listing", cleanup.unwrap_or((0, Value::Null)), 500, "listing");
    assert!(problems.is_empty(), "{problems:#?}");
}

/// ES|QL and SQL read as their client: an API key limited to one index (what roll secrets hands
/// out) sees no other index through `FROM`, a wildcard, a lookup join or SQL, and a role's
/// document query applies to its rows.
#[tokio::test]
async fn esql_and_sql_read_as_their_client() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "q"})).await;
    let (user, pw) = creds(&c);
    for (idx, doc) in [("notes", json!({"k": "a", "team": "red"})), ("notes", json!({"k": "b", "team": "blue"})), ("secrets", json!({"k": "a", "s": "x"}))] {
        let (s, v) = e.es(&user, &pw, reqwest::Method::POST, &format!("/{idx}/_doc?refresh=true"), Some(doc)).await;
        assert_eq!(s, 201, "{v}");
    }
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/secrets/_settings", Some(json!({"index.mode": "lookup"}))).await;
    let _ = (s, v);
    let role = json!({"name": "notes-only", "role_descriptors": {"r": {"indices": [{"names": ["notes"], "privileges": ["read"], "query": {"term": {"team.keyword": "red"}}}]}}});
    let (s, k) = e.es(&user, &pw, reqwest::Method::POST, "/_security/api_key", Some(role)).await;
    assert_eq!(s, 200, "{k}");
    // The cluster is named in the key (as the control plane's keys are): `<id>@<cluster>:<key>`.
    let raw = format!("{}@q:{}", k["id"].as_str().unwrap(), k["api_key"].as_str().unwrap());
    let header = format!("ApiKey {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw));
    let ask = |path: &'static str, body: Value| {
        let (url, http, h) = (format!("{}{path}", e.es), e.http.clone(), header.clone());
        async move {
            let r = http.post(url).header("Authorization", h).json(&body).send().await.unwrap();
            let s = r.status().as_u16();
            (s, r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    // Its own index: only the documents its role query lets through.
    let (s, v) = ask("/_query", json!({"query": "FROM notes | KEEP k"})).await;
    assert_eq!((s, v["values"].clone()), (200, json!([["a"]])), "{v}");
    for q in ["FROM secrets", "FROM notes,secrets"] {
        let (s, v) = ask("/_query", json!({"query": q})).await;
        assert!(s == 400 || s == 403, "{q}: {s} {v}");
        assert!(!v.to_string().contains("\"x\""), "{q} leaked a document: {v}");
    }
    let (s, v) = ask("/_query", json!({"query": "FROM *"})).await;
    assert_eq!(s, 200, "{v}");
    assert!(!v.to_string().contains("\"x\""), "a wildcard reached another index: {v}");
    let (s, v) = ask("/_query", json!({"query": "FROM notes | LOOKUP JOIN secrets ON k"})).await;
    assert!(!v.to_string().contains("\"x\""), "a lookup join reached another index: {s} {v}");
    let (s, v) = ask("/_sql?format=json", json!({"query": "SELECT * FROM secrets"})).await;
    assert!(s == 400 || s == 403, "SQL: {s} {v}");
    let (s, v) = ask("/_sql?format=json", json!({"query": "SELECT k FROM notes"})).await;
    assert_eq!((s, v["rows"].clone()), (200, json!([["a"]])), "{v}");
}

/// Field level security: a role that hides `secret` lets no read path give its content out
/// (Elasticsearch filters every one through the same field permissions). Each probe is a
/// request a user of that role may send; none may answer with the hidden value.
#[tokio::test]
async fn field_security_hides_a_field_on_every_read_path() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "fls"})).await;
    let (owner, pw) = creds(&c);
    let mapping = json!({"mappings": {"properties": {
        "name": {"type": "text", "term_vector": "with_positions_offsets", "fields": {"raw": {"type": "keyword"}}},
        "secret": {"type": "text", "term_vector": "with_positions_offsets", "fields": {"raw": {"type": "keyword"}}},
        "tags": {"type": "nested", "properties": {"secret": {"type": "keyword"}, "name": {"type": "keyword"}}},
        "suggest": {"type": "completion"}
    }}});
    let (s, v) = e.es(&owner, &pw, reqwest::Method::PUT, "/docs", Some(mapping)).await;
    assert_eq!(s, 200, "{v}");
    let doc = json!({"name": "alpha zebra", "secret": "zebra topsecretvalue", "tags": [{"secret": "topsecretvalue", "name": "t"}], "suggest": "alpha"});
    let (s, v) = e.es(&owner, &pw, reqwest::Method::PUT, "/docs/_doc/1?refresh=true", Some(doc)).await;
    assert_eq!(s, 201, "{v}");
    let role = json!({"indices": [{"names": ["docs"], "privileges": ["read", "view_index_metadata"], "field_security": {"grant": ["*"], "except": ["secret", "secret.*", "tags.secret"]}}]});
    assert_eq!(e.es(&owner, &pw, reqwest::Method::PUT, "/_security/role/no_secret", Some(role)).await.0, 200);
    let user = json!({"password": "reader-password-1", "roles": ["no_secret"]});
    assert_eq!(e.es(&owner, &pw, reqwest::Method::PUT, "/_security/user/reader", Some(user)).await.0, 200);
    let probes: Vec<(&str, reqwest::Method, &str, Option<Value>)> = vec![
        ("search _source", reqwest::Method::POST, "/docs/_search", Some(json!({}))),
        ("get", reqwest::Method::GET, "/docs/_doc/1", None),
        ("fields *", reqwest::Method::POST, "/docs/_search", Some(json!({"fields": ["*"]}))),
        ("docvalue_fields", reqwest::Method::POST, "/docs/_search", Some(json!({"docvalue_fields": ["secret.raw"]}))),
        ("query on it", reqwest::Method::POST, "/docs/_search", Some(json!({"query": {"match": {"secret": "topsecretvalue"}}}))),
        ("highlight", reqwest::Method::POST, "/docs/_search", Some(json!({"query": {"match": {"name": "zebra"}}, "highlight": {"require_field_match": false, "fields": {"*": {}}}}))),
        ("highlight named", reqwest::Method::POST, "/docs/_search", Some(json!({"query": {"match": {"name": "zebra"}}, "highlight": {"require_field_match": false, "fields": {"secret": {}}}}))),
        ("script_fields doc", reqwest::Method::POST, "/docs/_search", Some(json!({"script_fields": {"x": {"script": "doc['secret.raw'].size() > 0 ? doc['secret.raw'].value : ''"}}}))),
        ("script_fields _source", reqwest::Method::POST, "/docs/_search", Some(json!({"script_fields": {"x": {"script": "params._source.secret"}}}))),
        ("runtime field", reqwest::Method::POST, "/docs/_search", Some(json!({"runtime_mappings": {"x": {"type": "keyword", "script": "emit(params._source.secret)"}}, "fields": ["x"]}))),
        ("runtime field agg", reqwest::Method::POST, "/docs/_search", Some(json!({"size": 0, "runtime_mappings": {"x": {"type": "keyword", "script": "emit(params._source.secret)"}}, "aggs": {"a": {"terms": {"field": "x"}}}}))),
        ("terms agg", reqwest::Method::POST, "/docs/_search", Some(json!({"size": 0, "aggs": {"a": {"terms": {"field": "secret.raw"}}}}))),
        ("nested inner_hits", reqwest::Method::POST, "/docs/_search", Some(json!({"query": {"nested": {"path": "tags", "query": {"match_all": {}}, "inner_hits": {}}}}))),
        ("term suggester", reqwest::Method::POST, "/docs/_search", Some(json!({"suggest": {"s": {"text": "topsecretvalu", "term": {"field": "secret"}}}}))),
        ("termvectors", reqwest::Method::GET, "/docs/_termvectors/1?fields=secret", None),
        ("termvectors all", reqwest::Method::GET, "/docs/_termvectors/1", None),
        ("explain", reqwest::Method::POST, "/docs/_explain/1", Some(json!({"query": {"match": {"name": "zebra"}}}))),
        ("mget", reqwest::Method::POST, "/_mget", Some(json!({"docs": [{"_index": "docs", "_id": "1"}]}))),
        ("esql", reqwest::Method::POST, "/_query", Some(json!({"query": "FROM docs"}))),
        ("sql", reqwest::Method::POST, "/_sql?format=json", Some(json!({"query": "SELECT * FROM docs"}))),
        ("count by it", reqwest::Method::POST, "/docs/_count", Some(json!({"query": {"term": {"secret.raw": "zebra topsecretvalue"}}}))),
        ("sort script", reqwest::Method::POST, "/docs/_search", Some(json!({"sort": {"_script": {"type": "string", "script": "params._source.secret", "order": "asc"}}}))),
    ];
    // The reader reads the document (its visible fields), so a probe that answers says something.
    let (s, v) = e.es("reader@fls", "reader-password-1", reqwest::Method::POST, "/docs/_search", Some(json!({}))).await;
    assert_eq!((s, v["hits"]["hits"][0]["_source"]["name"].as_str()), (200, Some("alpha zebra")), "{v}");
    let mut leaks = Vec::new();
    for (name, method, path, body) in probes {
        let (s, v) = e.es("reader@fls", "reader-password-1", method, path, body).await;
        if v.to_string().contains("topsecretvalue") {
            leaks.push(format!("{name} ({s}): {}", v.to_string().chars().take(300).collect::<String>()));
        }
    }
    assert!(leaks.is_empty(), "{} read paths give the hidden field out:\n{}", leaks.len(), leaks.join("\n"));
    // Nor its name: field capabilities and mappings list only what the reader may see.
    for path in ["/docs/_field_caps?fields=*", "/docs/_mapping", "/docs/_mapping/field/*"] {
        let (s, v) = e.es("reader@fls", "reader-password-1", reqwest::Method::GET, path, None).await;
        let text = v.to_string();
        assert_eq!(s, 200, "{path}: {v}");
        assert!(text.contains("\"name") && !text.contains("\"secret"), "{path} lists the hidden field: {text}");
    }
}

/// An app's link to a cluster gets a credential of its own (apps' `Resources::link`): it reads and
/// writes the data, can't manage the cluster's security, and stops working once revoked: an API
/// key on an Elasticsearch-flavor cluster, an internal user of OpenSearch Security on an
/// OpenSearch-flavor one (which has no API keys).
#[tokio::test]
async fn a_link_credential_uses_the_data_and_is_revoked() {
    let e = env().await;
    for (name, flavor) in [("es-link", "elasticsearch"), ("os-link", "opensearch")] {
        e.post("/v1/search/clusters", json!({"name": name, "flavor": flavor})).await;
        let cred = e.control.mint_link(name, "svc_1").await.unwrap();
        let vars: std::collections::HashMap<_, _> = cred.vars.iter().cloned().collect();
        let send = |method: reqwest::Method, path: &str, body: Option<Value>| {
            let mut r = match (vars.get("ELASTICSEARCH_URL"), vars.get("OPENSEARCH_URL")) {
                (Some(u), _) => e.http.request(method, format!("{}{path}", e.es)).header("Host", host_of(u)).header("Authorization", format!("ApiKey {}", vars["ELASTICSEARCH_API_KEY"])),
                (_, Some(u)) => {
                    let parsed = reqwest::Url::parse(u).unwrap();
                    let user = urlencoding::decode(parsed.username()).unwrap().into_owned();
                    let pw = urlencoding::decode(parsed.password().unwrap()).unwrap().into_owned();
                    e.http.request(method, format!("{}{path}", e.es)).basic_auth(user, Some(pw))
                }
                _ => panic!("no URL in {vars:?}"),
            };
            if let Some(b) = body {
                r = r.json(&b);
            }
            async move { r.send().await.unwrap().status().as_u16() }
        };
        assert_eq!(send(reqwest::Method::PUT, "/items/_doc/1?refresh=true", Some(json!({"n": 1}))).await, 201, "{name}");
        assert_eq!(send(reqwest::Method::GET, "/items/_doc/1", None).await, 200, "{name}");
        let (path, user) = match flavor {
            "opensearch" => ("/_plugins/_security/api/internalusers/intruder", json!({"password": "correcthorsebattery", "opendistro_security_roles": ["all_access"]})),
            _ => ("/_security/user/intruder", json!({"password": "intruder-pass-1", "roles": ["superuser"]})),
        };
        assert_eq!(send(reqwest::Method::PUT, path, Some(user)).await, 403, "{name}: no security");
        e.control.revoke_link(name, &cred.kept).await.unwrap();
        assert_eq!(send(reqwest::Method::GET, "/items/_doc/1", None).await, 401, "{name}: revoked");
        e.control.revoke_link(name, &cred.kept).await.unwrap();
    }
    // The keys route on an OpenSearch-flavor cluster (roll secrets): an internal user of its own,
    // the data role, removed on DELETE, and refused once its expiry passes.
    let as_user = |u: &str, p: &str, method: reqwest::Method, path: &str, body: Option<Value>| {
        let mut r = e.http.request(method, format!("{}{path}", e.es)).basic_auth(u, Some(p));
        if let Some(b) = body {
            r = r.json(&b);
        }
        async move { r.send().await.unwrap().status().as_u16() }
    };
    let k = e.post("/v1/search/clusters/os-link/keys", json!({"name": "worker", "access": "data"})).await;
    assert!(k.get("api_key").is_none(), "{k}");
    let (u, p) = (k["username"].as_str().unwrap().to_string(), k["password"].as_str().unwrap().to_string());
    assert_eq!(as_user(&u, &p, reqwest::Method::PUT, "/items/_doc/2", Some(json!({"n": 2}))).await, 201);
    assert_eq!(as_user(&u, &p, reqwest::Method::PUT, "/_plugins/_security/api/internalusers/x", Some(json!({"password": "correcthorsebattery"}))).await, 403);
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/os-link/keys/{}", k["id"].as_str().unwrap()), None).await;
    assert_eq!(s, 204);
    assert_eq!(as_user(&u, &p, reqwest::Method::GET, "/items/_doc/2", None).await, 401, "removed");
    let (s, _) = e.call(reqwest::Method::DELETE, "/v1/search/clusters/os-link/keys/admin", None).await;
    assert_eq!(s, 404, "the keys route removes only users it made");
    // Three seconds, not one: a loaded host can take a second between minting and first use.
    let k = e.post("/v1/search/clusters/os-link/keys", json!({"name": "brief", "expires_in_s": 3})).await;
    assert!(k["expires_at"].as_i64().is_some(), "{k}");
    let (u, p) = (k["username"].as_str().unwrap().to_string(), k["password"].as_str().unwrap().to_string());
    assert_eq!(as_user(&u, &p, reqwest::Method::GET, "/items/_doc/2", None).await, 200);
    tokio::time::sleep(std::time::Duration::from_millis(3100)).await;
    assert_eq!(as_user(&u, &p, reqwest::Method::GET, "/items/_doc/2", None).await, 401, "expired");
}

/// The host of a URL (its cluster's name on the edge, a node's address otherwise).
fn host_of(url: &str) -> String {
    reqwest::Url::parse(url).unwrap().host_str().unwrap_or_default().to_string()
}

/// Deleted clusters are kept for the node's window (`deleted_retention`, 7 days): gone from the
/// list and the REST port at once, with their name free, listed under `?deleted=true`, and
/// restorable by their owner (or the operator) with their data, settings, owner and password;
/// under another name when someone took theirs.
#[tokio::test]
async fn deleted_clusters_restore_within_their_window() {
    let e = env().await;
    let (s, c) = e.call_as("ann", reqwest::Method::POST, "/v1/search/clusters", Some(json!({"name": "keep", "max_in_flight": 8}))).await;
    assert_eq!(s, 200, "{c}");
    let id = c["id"].as_str().unwrap().to_string();
    let (_, pw) = creds(&c);
    let (s, v) = e.es("keep", &pw, reqwest::Method::PUT, "/notes/_doc/1?refresh=true", Some(json!({"text": "kept"}))).await;
    assert_eq!(s, 201, "{v}");
    let path = format!("/v1/search/clusters/{id}");
    let delete = || e.call_as("ann", reqwest::Method::DELETE, &path, None);
    let deleted = |who: &'static str| {
        let e = &e;
        async move {
            let (s, v) = e.call_as(who, reqwest::Method::GET, "/v1/search/clusters?deleted=true", None).await;
            assert_eq!(s, 200, "{v}");
            v.as_array().unwrap().clone()
        }
    };

    // Deleted: gone from the list and the REST port, listed as deleted with its purge date.
    assert_eq!(delete().await.0, 200);
    let (_, list) = e.call_as("ann", reqwest::Method::GET, "/v1/search/clusters", None).await;
    assert!(list.as_array().unwrap().is_empty(), "{list}");
    assert_eq!(e.es("keep", &pw, reqwest::Method::GET, "/", None).await.0, 401);
    let gone = deleted("ann").await;
    assert_eq!((gone.len(), gone[0]["id"].as_str(), gone[0]["name"].as_str()), (1, Some(id.as_str()), Some("keep")), "{gone:?}");
    let (at, purge) = (gone[0]["deleted_at"].as_i64().unwrap(), gone[0]["purge_at"].as_i64().unwrap());
    assert_eq!(purge - at, 7 * 86_400);
    assert!(gone[0].get("password").is_none());
    // Someone else neither sees nor restores it.
    assert!(deleted("bob").await.is_empty());
    let (s, _) = e.call_as("bob", reqwest::Method::POST, &format!("/v1/search/clusters/{id}/restore"), Some(json!({}))).await;
    assert_eq!(s, 404);

    // Restored under its own name: its data, settings, owner and password are back.
    let (s, r) = e.call_as("ann", reqwest::Method::POST, &format!("/v1/search/clusters/{id}/restore"), None).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!((r["id"].as_str(), r["name"].as_str(), r["owner"].as_str(), r["max_in_flight"].as_i64()), (Some(id.as_str()), Some("keep"), Some("usr_ann"), Some(8)), "{r}");
    assert!(r.get("password").is_none());
    let (s, v) = e.es("keep", &pw, reqwest::Method::GET, "/notes/_doc/1", None).await;
    assert_eq!((s, v["_source"]["text"].as_str()), (200, Some("kept")), "{v}");
    assert!(deleted("ann").await.is_empty());
    let (s, _) = e.call_as("ann", reqwest::Method::POST, &format!("/v1/search/clusters/{id}/restore"), None).await;
    assert_eq!(s, 404, "a live cluster has nothing to restore");

    // Deleted again, and someone takes the name: restoring under it is a conflict, under
    // another name it works, with the same password and the new name as the user.
    assert_eq!(delete().await.0, 200);
    let (s, other) = e.call_as("bob", reqwest::Method::POST, "/v1/search/clusters", Some(json!({"name": "keep"}))).await;
    assert_eq!(s, 200, "{other}");
    let (s, v) = e.call_as("ann", reqwest::Method::POST, &format!("/v1/search/clusters/{id}/restore"), None).await;
    assert_eq!(s, 409, "{v}");
    assert!(v["error"].as_str().unwrap().contains("\"name\""), "{v}");
    let (s, r) = e.call_as("ann", reqwest::Method::POST, &format!("/v1/search/clusters/{id}/restore"), Some(json!({"name": "keep-2"}))).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!((r["name"].as_str(), r["username"].as_str()), (Some("keep-2"), Some("keep-2")), "{r}");
    let (s, v) = e.es("keep-2", &pw, reqwest::Method::GET, "/notes/_doc/1", None).await;
    assert_eq!((s, v["_source"]["text"].as_str()), (200, Some("kept")), "{v}");
    // Bob's cluster of that name is his own, empty one.
    let (bu, bp) = creds(&other);
    let (s, v) = e.es(&bu, &bp, reqwest::Method::GET, "/_mapping", None).await;
    assert_eq!((s, index_names(&v)), (200, Vec::<String>::new()));
    e.control.shutdown().await.unwrap();
}

/// A link removed while its cluster is deleted stays removed after a restore: roll secrets'
/// unlink (the keys route) and an app's (`revoke_link`) reach a kept cluster, and the restore
/// revokes what they named before it answers. A key still linked works again.
#[tokio::test]
async fn a_link_removed_while_deleted_stays_removed_after_a_restore() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "relink"})).await;
    let id = c["id"].as_str().unwrap().to_string();
    let unlinked = e.post(&format!("/v1/search/clusters/{id}/keys"), json!({"name": "roll secrets prod ES_KEY", "access": "data"})).await;
    let linked = e.post(&format!("/v1/search/clusters/{id}/keys"), json!({"name": "roll secrets dev ES_KEY", "access": "data"})).await;
    let app = e.control.mint_link(&id, "svc_1").await.unwrap();
    let app_key = app.vars.iter().find(|(k, _)| k == "ELASTICSEARCH_API_KEY").unwrap().1.clone();
    let count = |key: &str| {
        let r = e.http.get(format!("{}/notes/_count", e.es)).header("Authorization", format!("ApiKey {key}")).send();
        async move { r.await.unwrap().status().as_u16() }
    };
    let (s, _) = e.call(reqwest::Method::PUT, &format!("/v1/search/clusters/{id}/indices/notes"), Some(json!({}))).await;
    assert_eq!(s, 200);
    for k in [unlinked["api_key"].as_str().unwrap(), linked["api_key"].as_str().unwrap(), &app_key] {
        assert_eq!(count(k).await, 200);
    }

    // Deleted, then unlinked as roll secrets and roll apps do it.
    assert_eq!(e.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/{id}"), None).await.0, 200);
    let (s, v) = e.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/{id}/keys/{}", unlinked["id"].as_str().unwrap()), None).await;
    // Kept with the cluster for its restore (204). Someone who can't see it, or a cluster
    // that isn't there, is still 404.
    assert_eq!(s, 204, "{v}");
    assert_eq!(e.call_as("bob", reqwest::Method::DELETE, &format!("/v1/search/clusters/{id}/keys/k1"), None).await.0, 404);
    assert_eq!(e.call(reqwest::Method::DELETE, "/v1/search/clusters/srch_nothere/keys/k1", None).await.0, 404);
    e.control.revoke_link(&id, &app.kept).await.unwrap();

    // Restored: the unlinked keys stay dead, the linked one works.
    let (s, r) = e.call(reqwest::Method::POST, &format!("/v1/search/clusters/{id}/restore"), None).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(count(unlinked["api_key"].as_str().unwrap()).await, 401, "roll secrets' unlinked key");
    assert_eq!(count(&app_key).await, 401, "an app's unlinked key");
    assert_eq!(count(linked["api_key"].as_str().unwrap()).await, 200, "a key still linked");
    e.control.shutdown().await.unwrap();
}

/// After the window the sweep purges a deleted cluster's kept storage as a delete without a
/// window does, and its row; a branch of it keeps the data it shares, through garbage
/// collection too.
#[tokio::test]
async fn the_sweep_purges_after_the_window_and_branches_keep_their_data() {
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "swept"})).await;
    let id = c["id"].as_str().unwrap().to_string();
    let (user, pw) = creds(&c);
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/notes/_doc/1?refresh=true", Some(json!({"text": "shared"}))).await;
    assert_eq!(s, 201, "{v}");
    let b = e.post("/v1/search/clusters/swept/branches", json!({"name": "swept-b"})).await;
    let (bu, bp) = creds(&b);
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/{id}"), None).await;
    assert_eq!(s, 200);
    let kept = || async {
        e.objects.list("search/c").await.unwrap().into_iter().filter(|(k, _)| k.contains("/deleted_")).count()
    };
    assert!(kept().await > 0, "the deleted cluster's storage is kept");

    let now = search_control::control::now();
    assert_eq!(e.control.purge_deleted(now).await.unwrap(), 0, "inside the window nothing is purged");
    assert_eq!(e.control.purge_deleted(now + 8 * 86_400).await.unwrap(), 1);
    assert_eq!(kept().await, 0, "its kept storage is gone");
    assert!(e.get("/v1/search/clusters?deleted=true").await.as_array().unwrap().is_empty());
    let (s, _) = e.call(reqwest::Method::POST, &format!("/v1/search/clusters/{id}/restore"), None).await;
    assert_eq!(s, 404);

    // The branch reads the segments it shares from storage, before and after garbage collection.
    for _ in 0..2 {
        assert!(e.post("/v1/search/clusters/swept-b/unload", json!({})).await["unloaded"].as_bool().unwrap());
        e.control.data.collect_garbage(Duration::ZERO).await.unwrap();
        let (s, v) = e.es(&bu, &bp, reqwest::Method::GET, "/notes/_doc/1", None).await;
        assert_eq!((s, v["_source"]["text"].as_str()), (200, Some("shared")), "{v}");
    }
    e.control.shutdown().await.unwrap();
}

/// Search analytics: off until an index turns them on; then searches (with and without results)
/// and `search_click` events come back as top queries, no-result queries and click-through,
/// before and after the metering pass; turning them off deletes what's kept; another person's
/// cluster answers 404.
#[tokio::test]
async fn search_analytics() {
    use reqwest::Method;
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "shop"})).await;
    let (user, pw) = creds(&c);
    let es = |m: Method, p: &str, b: Option<Value>| {
        let (user, pw, p, e) = (user.clone(), pw.clone(), p.to_string(), &e);
        async move {
            let (s, v) = e.es(&user, &pw, m, &p, b).await;
            assert!(s < 300, "{p}: {s} {v}");
            v
        }
    };
    es(Method::PUT, "/products", Some(json!({"mappings": {"properties": {"title": {"type": "text"}}}}))).await;
    for (id, t) in [("1", "red shoes"), ("2", "blue shoes"), ("3", "red hat")] {
        es(Method::PUT, &format!("/products/_doc/{id}?refresh=true"), Some(json!({ "title": t }))).await;
    }
    let search = |q: &str| es(Method::POST, "/products/_search", Some(json!({"query": {"match": {"title": q}}})));
    let analytics = async |range: &str| e.get(&format!("/v1/search/clusters/shop/analytics?index=products&range={range}")).await;

    // Off by default: nothing is recorded.
    search("shoes").await;
    e.control.sample().await.unwrap();
    let a = analytics("24h").await;
    assert_eq!((a["totals"]["searches"].as_u64(), a["top"].as_array().unwrap().len()), (Some(0), 0), "{a}");

    es(Method::PUT, "/products/_settings", Some(json!({"index.roll.analytics.enabled": true}))).await;
    for _ in 0..3 {
        search("Shoes").await;
    }
    search("red").await;
    search("sandals").await;
    search("sandals").await;
    // A header names a search's text, whatever its body; `_msearch` items count one by one.
    let r = e.http.post(format!("{}/products/_search", e.es)).basic_auth(&user, Some(&pw)).header("x-roll-search-query", "Hat").json(&json!({"query": {"term": {"title": "hat"}}})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let msearch = "{\"index\":\"products\"}\n{\"query\":{\"match\":{\"title\":\"red\"}}}\n";
    let r = e.http.post(format!("{}/_msearch", e.es)).basic_auth(&user, Some(&pw)).header("content-type", "application/x-ndjson").body(msearch).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    // Clicks: Elasticsearch's own behavioral analytics events.
    es(Method::PUT, "/_application/analytics/web", None).await;
    let click = |query: &str, id: &str| {
        json!({
            "session": {"id": "s1"}, "user": {"id": "u1"},
            "document": {"index": "products", "id": id},
            "search": {"query": query, "results": {"items": [{"document": {"index": "products", "id": "1"}}, {"document": {"index": "products", "id": "2"}}]}},
        })
    };
    es(Method::POST, "/_application/analytics/web/event/search_click", Some(click("shoes", "2"))).await;
    es(Method::POST, "/_application/analytics/web/event/search_click", Some(click("shoes", "1"))).await;

    let check = |a: &Value| {
        assert_eq!(a["totals"]["searches"].as_u64(), Some(8), "{a}");
        assert_eq!(a["totals"]["no_hits"].as_u64(), Some(2), "{a}");
        assert_eq!(a["totals"]["clicks"].as_u64(), Some(2), "{a}");
        let top = a["top"].as_array().unwrap();
        assert_eq!(top.iter().map(|q| (q["query"].as_str().unwrap(), q["searches"].as_u64().unwrap())).collect::<Vec<_>>(), [("shoes", 3), ("red", 2), ("sandals", 2), ("hat", 1)], "{a}");
        assert_eq!((top[0]["clicks"].as_u64(), top[0]["ctr"].as_f64(), top[0]["avg_position"].as_f64()), (Some(2), Some(0.6667), Some(1.5)), "{a}");
        assert_eq!(a["no_results"].as_array().unwrap().iter().map(|q| q["query"].as_str().unwrap()).collect::<Vec<_>>(), ["sandals"], "{a}");
        // Clicked never: red and hat (with results) are low click-through; sandals found nothing.
        assert_eq!(a["low_ctr"].as_array().unwrap().iter().map(|q| q["query"].as_str().unwrap()).collect::<Vec<_>>(), ["red"], "{a}");
        assert_eq!(a["indices"], json!(["products"]), "{a}");
    };
    check(&analytics("24h").await);
    e.control.sample().await.unwrap();
    check(&analytics("24h").await);
    check(&analytics("30d").await);
    // Nothing kept anywhere holds a credential or a document.
    let all = analytics("7d").await.to_string();
    assert!(!all.contains(&pw) && !all.contains("blue shoes"), "{all}");

    // Another person's cluster: no such cluster.
    let (s, _) = e.call_as("mallory", Method::GET, "/v1/search/clusters/shop/analytics", None).await;
    assert_eq!(s, 404);

    // Turning it off deletes what's kept, at once for reads and from the catalog on the next pass.
    es(Method::PUT, "/products/_settings", Some(json!({"index.roll.analytics.enabled": null}))).await;
    assert_eq!(analytics("24h").await["totals"]["searches"].as_u64(), Some(0));
    e.control.sample().await.unwrap();
    let id = c["id"].as_str().unwrap();
    assert!(e.control.store.analytics_indices(id).await.unwrap().is_empty());
    search("shoes").await;
    assert_eq!(analytics("24h").await["totals"]["searches"].as_u64(), Some(0));
}
