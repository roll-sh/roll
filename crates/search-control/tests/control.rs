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
    // Tests without a sealer keep secrets as given (a test of the refusal turns it off).
    cfg.unsealed_secrets = true;
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
    // `x-test-grant`: a scoped token's rules (JSON), as the gate leaves them for the product, with
    // `reveal` capable when the rules allow it (the gate's `roll_auth::Capable`).
    if let Some(rules) = req.headers().get("x-test-grant").and_then(|v| serde_json::from_slice::<roll_auth::scope::Scope>(v.as_bytes()).ok()) {
        if req.headers().get("x-test-capable").is_some_and(|v| v == "reveal") {
            req.extensions_mut().insert(roll_auth::Capable(vec![roll_auth::scope::Action::Reveal]));
        }
        req.extensions_mut().insert(roll_auth::Operator);
        req.extensions_mut().insert(roll_auth::Grant { id: "tok_t".into(), name: "t".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(rules), user: None, ask_otherwise: false });
    }
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
/// With the deployment's key, passwords are sealed in the catalog and still authenticate: a
/// cluster's, its branch's (sealed under its own id) and a rotated one.
#[tokio::test]
async fn sealed_passwords_authenticate() {
    let e = env_with(|c| c.sealer = search_control::Sealer::from_secret("0123456789abcdef0123456789abcdef")).await;
    let c = e.post("/v1/search/clusters", json!({"name": "app"})).await;
    let id = c["id"].as_str().unwrap().to_string();
    let (user, pw) = creds(&c);
    assert_eq!(e.es(&user, &pw, reqwest::Method::GET, "/", None).await.0, 200);
    let (bu, bp) = creds(&e.post(&format!("/v1/search/clusters/{id}/branches"), json!({"name": "app-dev"})).await);
    assert_eq!(e.es(&bu, &bp, reqwest::Method::GET, "/", None).await.0, 200);
    let (_, rotated) = creds(&e.post(&format!("/v1/search/clusters/{id}/password"), json!({})).await);
    assert_eq!(e.es(&user, &rotated, reqwest::Method::GET, "/", None).await.0, 200);
    assert_eq!(e.es(&user, &pw, reqwest::Method::GET, "/", None).await.0, 401);
}

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

/// A grant's console request needs `reveal` on the cluster too when the API the cluster routes
/// it to answers or sets a credential (a new API key, a user's password); everything else,
/// whatever its path spells, needs only the route's `admin`.
#[tokio::test]
async fn console_credentials_need_reveal() {
    let e = env().await;
    let id = e.post("/v1/search/clusters", json!({ "name": "vault" })).await["id"].as_str().unwrap().to_string();
    let rules = |actions: Value| json!([{ "product": "search", "resources": [{ "id": id }], "actions": actions }]).to_string();
    let (admin, both) = (rules(json!({ "admin": "allow" })), rules(json!({ "admin": "allow", "reveal": "allow" })));
    let console = |grant: &str, method: &str, path: &str, body: Value| {
        let capable = if grant.contains("reveal") { "reveal" } else { "" };
        let req = e.http.post(format!("{}/v1/search/clusters/vault/request", e.api)).header("x-test-grant", grant).header("x-test-capable", capable).json(&json!({ "method": method, "path": path, "body": body }));
        async move {
            let r = req.send().await.unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    assert_eq!(console(&admin, "GET", "/_cat/indices", Value::Null).await.0, 200);
    assert_eq!(console(&admin, "GET", "/enrollments/_search?q=api_key:x", Value::Null).await.0, 200, "an index and a query string named after a credential answer none");
    for (method, path, body) in [("POST", "/_security/api_key", json!({ "name": "k" })), ("PUT", "/_security/user/ann", json!({ "password": "a-long-password-1", "roles": [] }))] {
        let (s, v) = console(&admin, method, path, body.clone()).await;
        assert!(s == 403 && v["error"].as_str().is_some_and(|m| m.contains("needs reveal")), "{method} {path}: {s} {v}");
        let (s, v) = console(&both, method, path, body).await;
        assert_eq!((s, v["status"].as_u64()), (200, Some(200)), "{method} {path}: {v}");
    }
}

/// What a deleted cluster recorded and nobody had handed on yet (slow log, request metrics,
/// analytics, requests to bill) goes with it: a cluster that takes its name at once starts with
/// none of it, before and after the next metering pass.
#[tokio::test]
async fn a_reused_name_inherits_nothing_of_the_deleted_cluster() {
    use reqwest::Method;
    let e = env().await;
    let old = e.post("/v1/search/clusters", json!({ "name": "shop" })).await;
    let (user, pw) = creds(&old);
    let settings = json!({ "settings": { "index.search.slowlog.threshold.query.warn": "0ms", "index.roll.analytics.enabled": true } });
    assert_eq!(e.es(&user, &pw, Method::PUT, "/orders", Some(settings)).await.0, 200);
    for _ in 0..3 {
        assert_eq!(e.es(&user, &pw, Method::POST, "/orders/_search", Some(json!({ "query": { "match": { "card": "4111 1111" } } }))).await.0, 200);
    }
    assert!(!e.get("/v1/search/clusters/shop/slowlog").await["entries"].as_array().unwrap().is_empty());
    assert_eq!(e.call(Method::DELETE, "/v1/search/clusters/shop", None).await.0, 200);
    let new = e.post("/v1/search/clusters", json!({ "name": "shop" })).await;
    assert_ne!(new["id"], old["id"]);
    let id = new["id"].as_str().unwrap().to_string();
    for pass in ["before the pass", "after it"] {
        let log = e.get(&format!("/v1/search/clusters/{id}/slowlog")).await;
        assert_eq!(log["entries"], json!([]), "{pass}: {log}");
        let m = e.get(&format!("/v1/search/clusters/{id}/metrics?range=1h")).await;
        assert_eq!(m["totals"]["all"]["requests"].as_u64().unwrap_or(0), 0, "{pass}: {m}");
        let a = e.get(&format!("/v1/search/clusters/{id}/analytics?range=24h")).await;
        assert_eq!(a["totals"]["searches"].as_u64(), Some(0), "{pass}: {a}");
        let u = e.get(&format!("/v1/search/clusters/{id}/usage?days=1")).await;
        assert_eq!(u["requests"].as_i64().unwrap_or(0), 0, "{pass}: {u}");
        e.control.sample().await.unwrap();
    }
}

/// Credentials pasted into a snapshot repository or a remote link are never kept in the clear: a
/// node without a sealing key refuses them (pointing at a `roll://` reference or the key) and
/// keeps nothing, and a node with one keeps them only sealed, in the cluster state as saved and in
/// the catalog.
#[tokio::test]
async fn pasted_credentials_are_sealed_or_refused() {
    use reqwest::Method;
    let (endpoint, _bucket) = MockBucket::start().await;
    for sealed in [false, true] {
        let e = env_with(|c| {
            c.node_settings.push((search_control::S3_ALLOW_PRIVATE.into(), "true".into()));
            c.unsealed_secrets = false;
            if sealed {
                c.sealer = search_control::Sealer::from_secret("0123456789abcdef0123456789abcdef");
            }
        })
        .await;
        let app = e.post("/v1/search/clusters", json!({ "name": "app" })).await;
        let (ua, pa) = creds(&app);
        let (ul, pl) = creds(&e.post("/v1/search/clusters", json!({ "name": "logs" })).await);
        let repo = json!({ "type": "s3", "settings": {
            "bucket": "backups", "base_path": "seal", "endpoint": endpoint, "path_style_access": true,
            "access_key": "AKIDPASTED", "secret_key": "pasted-secret-key",
        } });
        let (s, v) = e.es(&ua, &pa, Method::PUT, "/_snapshot/backup?verify=false", Some(repo)).await;
        let (_, k) = e.es(&ul, &pl, Method::POST, "/_security/cross_cluster/api_key", Some(json!({ "name": "mine", "access": { "search": [{ "names": ["*"] }] } }))).await;
        let key = k["encoded"].as_str().unwrap_or_else(|| panic!("{k}")).to_string();
        let (sr, vr) = e.call(Method::PUT, "/v1/search/clusters/app/remotes/logs", Some(json!({ "cluster": "logs", "api_key": key }))).await;
        if sealed {
            assert_eq!((s, sr), (200, 200), "{v} {vr}");
        } else {
            assert!(s == 400 && v.to_string().contains("roll://"), "{v}");
            assert!(sr == 400 && vr.to_string().contains("ROLL_SECRET_KEY"), "{vr}");
        }
        let state = e.control.node.get_loaded("app").unwrap().cluster.state().to_json().to_json();
        assert!(!state.contains("pasted-secret-key") && !state.contains("AKIDPASTED"), "sealed={sealed}: a credential in the saved state");
        assert_eq!(state.contains("::roll_sealed::"), sealed);
        match e.control.store.remote(app["id"].as_str().unwrap(), "logs").await.unwrap() {
            Some(r) => assert!(sealed && search_es::seal::is_sealed(&r.credentials) && !r.credentials.contains(&key)),
            None => assert!(!sealed, "the link is kept"),
        }
    }
}

/// Documents being inverted share the node's memory budget for it (`max_analysis_bytes`):
/// parallel `ngram` documents with grams of up to 1,000 characters get 429
/// `circuit_breaking_exception`, their analysis stopped, instead of growing the buffers without
/// bound; what they left is the budget's until the buffer goes.
#[tokio::test]
async fn parallel_ngram_documents_are_held_to_the_node_analysis_budget() {
    use reqwest::Method;
    let e = env_with(|c| c.max_analysis_bytes = Some(16 << 20)).await;
    let (user, pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "grams" })).await);
    let index = json!({
        "settings": { "index.max_ngram_diff": 50, "analysis": {
            "tokenizer": { "g": { "type": "ngram", "min_gram": 950, "max_gram": 1000 } },
            "analyzer": { "a": { "type": "custom", "tokenizer": "g" } } } },
        "mappings": { "properties": { "body": { "type": "text", "analyzer": "a" } } },
    });
    assert_eq!(e.es(&user, &pw, Method::PUT, "/grams", Some(index)).await.0, 200);
    // About 3 GB of grams each, were they indexed.
    let text: String = (0..60_000u32).map(|i| char::from(b'a' + (i.wrapping_mul(2_654_435_761) >> 27) as u8 % 26)).collect();
    let paths: Vec<String> = (0..4).map(|i| format!("/grams/_doc/{i}")).collect();
    let results = futures_util::future::join_all(paths.iter().map(|p| e.es(&user, &pw, Method::PUT, p, Some(json!({ "body": text }))))).await;
    for (s, v) in &results {
        assert!(*s == 429 && v["error"]["type"] == "circuit_breaking_exception", "{s} {v}");
    }
    let l = e.control.node.get_loaded("grams").unwrap();
    let heap = search_control::meter::engine_figures(&l.cluster).heap_bytes;
    assert!(heap < 64 << 20, "the buffers stayed near the budget: {heap}");
}

/// Request bodies in flight are charged to the node's budget too (`max_in_flight_bytes`): two
/// clusters each within their own budget get 429 once together they pass the node's.
#[tokio::test]
async fn bodies_in_flight_are_held_to_the_node_budget() {
    use base64::Engine;
    use tokio::io::AsyncWriteExt;
    let e = env_with(|c| c.max_in_flight_bytes = Some(1_500_000)).await;
    let (ua, pa) = creds(&e.post("/v1/search/clusters", json!({ "name": "a", "max_body_mb": 1 })).await);
    let (ub, pb) = creds(&e.post("/v1/search/clusters", json!({ "name": "b", "max_body_mb": 1 })).await);
    // a's upload: its headers are in, its 1,000,000 bytes aren't yet.
    let mut held = tokio::net::TcpStream::connect(e.es.trim_start_matches("http://")).await.unwrap();
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{ua}:{pa}"));
    let head = format!("POST /_bulk HTTP/1.1\r\nhost: x\r\nauthorization: Basic {auth}\r\ncontent-type: application/x-ndjson\r\ncontent-length: 1000000\r\n\r\n");
    held.write_all(head.as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while e.control.node.get_loaded("a").is_none_or(|l| l.bytes_in_flight() == 0) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a's body is reserved");
    let doc = json!({ "pad": "x".repeat(900_000) });
    let (s, v) = e.es(&ub, &pb, reqwest::Method::PUT, "/i/_doc/1", Some(doc.clone())).await;
    assert_eq!(s, 429, "{v}");
    assert!(v.to_string().contains("max_coordinating_and_primary_bytes=1500000"), "names the node's budget: {v}");
    drop(held);
    tokio::time::timeout(Duration::from_secs(10), async {
        while e.control.node.get_loaded("a").is_some_and(|l| l.bytes_in_flight() > 0) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a's reservation goes with its connection");
    assert_eq!(e.es(&ub, &pb, reqwest::Method::PUT, "/i/_doc/1", Some(doc)).await.0, 201);
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
    let c = e.post("/v1/search/clusters", json!({"name": "shop", "max_in_flight": 16, "reindex_whitelist": ["es.example.com:443"]})).await;
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
    // Not the parent's remotes: a branch whitelists its own.
    assert_eq!((c["reindex_whitelist"].clone(), b["reindex_whitelist"].clone()), (json!(["es.example.com:443"]), json!([])));
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

/// Search from the browser: a cluster's allowed origins (validated, kept as browsers send
/// them, shown as `http.cors.*`, behind the IP allowlist) and browser keys made through
/// `/browser-keys`: they read through their filter, never write, one limited to a search
/// application runs only its template (params checked against its dictionary), each is rate
/// limited with a 429 and `Retry-After` a page can read, and revoking one stops it at once.
#[tokio::test]
async fn browsers_search_with_browser_keys() {
    let e = env_with(|c| c.domain = Some("search.example.test".into())).await;
    let c = e.post("/v1/search/clusters", json!({"name": "shop"})).await;
    let path = format!("/v1/search/clusters/{}", c["id"].as_str().unwrap());
    let (request, keys) = (format!("{path}/request"), format!("{path}/browser-keys"));
    let console = |method: &str, p: &str, body: Value| e.post(&request, json!({"method": method, "path": p, "body": body}));
    let bulk = "{\"index\":{\"_index\":\"products\",\"_id\":\"1\"}}\n{\"title\":\"trail shoes\",\"public\":true}\n{\"index\":{\"_index\":\"products\",\"_id\":\"2\"}}\n{\"title\":\"secret shoes\",\"public\":false}\n";
    assert_eq!(console("POST", "/_bulk?refresh=true", json!(bulk)).await["status"], 200);
    let app = json!({"indices": ["products"], "template": {
        "script": {"source": {"query": {"match": {"title": "{{query}}"}}}, "params": {"query": ""}},
        "dictionary": {"properties": {"query": {"type": "string"}}, "additionalProperties": false}}});
    let r = console("PUT", "/_application/search_application/shop-search", app).await;
    assert_eq!(r["status"], 201, "{r}");

    for bad in [json!({"allow_origin": ["shop.example.com"]}), json!({"allow_origin": ["/(/"]}), json!({"allow_origin": ["*"], "allow_methods": ["FETCH"]})] {
        let (s, v) = e.call(reqwest::Method::PATCH, &path, Some(json!({ "cors": bad }))).await;
        assert_eq!(s, 400, "{bad}: {v}");
    }
    let (s, v) = e.call(reqwest::Method::PATCH, &path, Some(json!({"cors": {"allow_origin": ["HTTPS://Shop.Example.com:443"]}}))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["cors"], json!({"allow_origin": ["https://shop.example.com"]}));
    let r = console("GET", "/_cluster/settings?include_defaults&flat_settings", Value::Null).await;
    assert_eq!(body(&r)["defaults"]["http.cors.allow-origin"], "https://shop.example.com", "{r}");

    let made = |k: Value| e.post(&keys, k);
    // `alias:index` would reach a remote cluster.
    let (s, v) = e.call(reqwest::Method::POST, &keys, Some(json!({"name": "remote", "indices": ["other:products"]}))).await;
    assert!(s == 400 && v.to_string().contains("browser keys can't search across clusters"), "{s} {v}");
    let reader = made(json!({"name": "shop page", "indices": ["products"], "filter": {"term": {"public": true}}})).await;
    let runner = made(json!({"name": "app page", "application": "shop-search", "rate_limit": {"per_minute": 3}})).await;
    assert_eq!((reader["rate_limit"]["per_minute"].as_u64(), runner["application"].as_str()), (Some(600), Some("shop-search")));
    let browser = |key: &Value, method: reqwest::Method, p: &str, body: Option<Value>| {
        let mut r = e
            .http
            .request(method, format!("{}{p}", e.es))
            .header("host", "shop.search.example.test")
            .header("origin", "https://shop.example.com")
            .header("authorization", format!("ApiKey {}", key["api_key"].as_str().unwrap()));
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send()
    };
    let r = browser(&reader, reqwest::Method::POST, "/products/_search", Some(json!({"query": {"match_all": {}}}))).await.unwrap();
    assert_eq!(r.headers()["access-control-allow-origin"], "https://shop.example.com");
    assert_eq!(r.headers()["vary"], "origin");
    let v: Value = r.json().await.unwrap();
    let titles: Vec<&str> = v["hits"]["hits"].as_array().unwrap().iter().filter_map(|h| h["_source"]["title"].as_str()).collect();
    assert_eq!(titles, ["trail shoes"], "the filter holds: {v}");
    let r = browser(&reader, reqwest::Method::PUT, "/products/_doc/3", Some(json!({"title": "x"}))).await.unwrap();
    assert_eq!(r.status().as_u16(), 403, "a browser key never writes");

    // The application key runs the template, its params checked, and nothing else.
    let run = |params: Value| browser(&runner, reqwest::Method::POST, "/_application/search_application/shop-search/_search", Some(json!({ "params": params })));
    let r = run(json!({"query": "shoes"})).await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["hits"]["total"]["value"], 2);
    assert_eq!(run(json!({"query": 5})).await.unwrap().status().as_u16(), 400, "params are checked against the dictionary");
    let r = browser(&runner, reqwest::Method::POST, "/products/_search", Some(json!({}))).await.unwrap();
    assert_eq!(r.status().as_u16(), 403, "only the application's search");
    // Its fourth request in a minute is over its limit, and the page can read why.
    let r = run(json!({"query": "shoes"})).await.unwrap();
    assert_eq!(r.status().as_u16(), 429);
    assert!(r.headers()["retry-after"].to_str().unwrap().parse::<u64>().unwrap() >= 1);
    assert_eq!(r.headers()["access-control-allow-origin"], "https://shop.example.com");
    assert_eq!(r.json::<Value>().await.unwrap()["error"]["type"], "es_rejected_execution_exception");

    let listed = e.get(&keys).await;
    assert_eq!(listed["keys"].as_array().unwrap().len(), 2, "{listed}");
    assert!(listed.to_string().find("api_key").is_none(), "secrets aren't listed");
    // Only browser keys: another key of the cluster isn't this route's to revoke.
    let other = e.post(&format!("{path}/keys"), json!({"name": "ingest", "access": "data"})).await;
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("{path}/browser-keys/{}", other["id"].as_str().unwrap()), None).await;
    assert_eq!(s, 404);
    // A branch copies the cluster's keys with its security; a browser key works only where it
    // was made: not on a branch made before its revoke, nor on one cut from a point before it.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let before_revoke = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    e.post(&format!("{path}/branches"), json!({"name": "shop-staging"})).await;
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("{path}/browser-keys/{}", reader["id"].as_str().unwrap()), None).await;
    assert_eq!(s, 204);
    let r = browser(&reader, reqwest::Method::POST, "/products/_search", Some(json!({}))).await.unwrap();
    assert_eq!(r.status().as_u16(), 401);
    // Any key revoked after the point stays revoked on a point-in-time branch: it takes its
    // parent's security as it is now.
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("{path}/keys/{}", other["id"].as_str().unwrap()), None).await;
    assert_eq!(s, 204);
    e.post(&format!("{path}/branches"), json!({"name": "shop-before", "at": before_revoke})).await;
    {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let shipped = String::from_utf8(b64.decode(other["api_key"].as_str().unwrap()).unwrap()).unwrap();
        let on_branch = b64.encode(shipped.replace("@shop:", "@shop-before:"));
        let r = e.http.get(format!("{}/", e.es)).header("authorization", format!("ApiKey {on_branch}")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 401, "a regular key revoked after the point, on the point-in-time branch");
    }
    for branch in ["shop-staging", "shop-before"] {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let shipped = String::from_utf8(b64.decode(reader["api_key"].as_str().unwrap()).unwrap()).unwrap();
        let moved = json!({ "api_key": b64.encode(shipped.replace("@shop:", &format!("@{branch}:"))) });
        let r = browser(&moved, reqwest::Method::POST, "/products/_search", Some(json!({}))).await.unwrap();
        assert_eq!(r.status().as_u16(), 401, "the parent's browser key on {branch}");
        let listed = e.get(&format!("/v1/search/clusters/{branch}/browser-keys")).await;
        assert_eq!(listed["keys"], json!([]), "a branch lists no browser keys of its parent");
    }

    // The IP allowlist comes first, preflights included.
    let (s, _) = e.call(reqwest::Method::PATCH, &path, Some(json!({"ip_allowlist": ["203.0.113.0/24"]}))).await;
    assert_eq!(s, 200);
    let r = e
        .http
        .request(reqwest::Method::OPTIONS, format!("{}/products/_search", e.es))
        .header("host", "shop.search.example.test")
        .header("origin", "https://shop.example.com")
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    assert_eq!(r.text().await.unwrap(), r#"{"ok":false,"message":"Forbidden due to traffic filtering"}"#);
}

/// A cluster where a browser key's filter (`public: true`) hides document 2 of `products`, and an
/// index `other` the key can't read: the key, for [`as_key`].
async fn filtered_key(e: &Env) -> String {
    let c = e.post("/v1/search/clusters", json!({"name": "shop"})).await;
    let path = format!("/v1/search/clusters/{}", c["id"].as_str().unwrap());
    let bulk = "{\"index\":{\"_index\":\"products\",\"_id\":\"1\"}}\n{\"title\":\"trail shoes\",\"public\":true,\"color\":\"red\"}\n\
                {\"index\":{\"_index\":\"products\",\"_id\":\"2\"}}\n{\"title\":\"secret shoes\",\"public\":false,\"color\":\"red\"}\n\
                {\"index\":{\"_index\":\"other\",\"_id\":\"1\"}}\n{\"title\":\"other shoes\",\"color\":\"red\"}\n";
    let r = e.post(&format!("{path}/request"), json!({"method": "POST", "path": "/_bulk?refresh=true", "body": bulk})).await;
    assert_eq!(r["status"], 200, "{r}");
    let k = e.post(&format!("{path}/browser-keys"), json!({"name": "page", "indices": ["products"], "filter": {"term": {"public": true}}})).await;
    k["api_key"].as_str().unwrap().to_string()
}

/// A request to the cluster with a browser key.
async fn as_key(e: &Env, key: &str, method: reqwest::Method, p: &str, body: Option<Value>) -> (u16, Value) {
    let mut r = e.http.request(method, format!("{}{p}", e.es)).header("authorization", format!("ApiKey {key}"));
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    let s = r.status().as_u16();
    let t = r.text().await.unwrap();
    (s, serde_json::from_str(&t).unwrap_or(Value::String(t)))
}

fn total(v: &Value) -> i64 {
    v["hits"]["total"]["value"].as_i64().unwrap_or(-1)
}

/// Suggesters read the terms of every document, and a profile shows the filter: neither runs
/// for a key whose documents are filtered (Elasticsearch's `DlsFlsRequestValidator`).
#[tokio::test]
async fn a_filtered_key_gets_no_suggestions_from_hidden_documents() {
    let e = env().await;
    let key = filtered_key(&e).await;
    let suggest = json!({"size": 0, "suggest": {"s": {"text": "secrat", "term": {"field": "title", "suggest_mode": "always"}}}});
    let (s, v) = as_key(&e, &key, reqwest::Method::POST, "/products/_search", Some(suggest)).await;
    assert_eq!(s, 400, "{v}");
    assert!(v["error"]["reason"].as_str().unwrap().contains("Suggest isn't supported if document level security is enabled"), "{v}");
    let (s, _) = as_key(&e, &key, reqwest::Method::POST, "/products/_search", Some(json!({"profile": true}))).await;
    assert_eq!(s, 400);
}

/// A terms lookup reads its document as the key: one its filter hides is as missing as one that
/// doesn't exist, and an index it can't read is refused.
#[tokio::test]
async fn a_terms_lookup_reads_as_the_filtered_key() {
    let e = env().await;
    let key = filtered_key(&e).await;
    let lookup = |id: &str, index: &str| json!({"query": {"terms": {"color.keyword": {"index": index, "id": id, "path": "color"}}}});
    for id in ["2", "99"] {
        let (s, v) = as_key(&e, &key, reqwest::Method::POST, "/products/_search", Some(lookup(id, "products"))).await;
        assert_eq!((s, total(&v)), (200, 0), "lookup of {id}: {v}");
    }
    let (s, v) = as_key(&e, &key, reqwest::Method::POST, "/products/_search", Some(lookup("1", "other"))).await;
    assert_eq!(s, 403, "{v}");
}

/// `more_like_this` reads its `like` documents as the key, as a terms lookup does.
#[tokio::test]
async fn more_like_this_reads_its_documents_as_the_filtered_key() {
    let e = env().await;
    let key = filtered_key(&e).await;
    let mlt = |id: &str, index: &str| json!({"query": {"more_like_this": {"fields": ["title"], "like": [{"_index": index, "_id": id}], "min_term_freq": 1, "min_doc_freq": 1}}});
    for id in ["2", "99"] {
        let (s, v) = as_key(&e, &key, reqwest::Method::POST, "/products/_search", Some(mlt(id, "products"))).await;
        assert_eq!((s, total(&v)), (200, 0), "like {id}: {v}");
    }
    let (s, v) = as_key(&e, &key, reqwest::Method::POST, "/products/_search", Some(mlt("1", "other"))).await;
    assert_eq!(s, 403, "{v}");
}

/// A regular API key of cluster `shop` with the browser key's filter (`public: true` on
/// `products`): document level security for a principal that isn't a browser key.
async fn filtered_api_key(e: &Env) -> String {
    use base64::Engine;
    let r = e
        .post(
            "/v1/search/clusters/shop/request",
            json!({"method": "POST", "path": "/_security/api_key", "body": {"name": "filtered", "role_descriptors": {"r": {"indices": [{"names": ["products"], "privileges": ["read"], "query": {"term": {"public": true}}}]}}}}),
        )
        .await;
    let v = body(&r);
    base64::engine::general_purpose::STANDARD.encode(format!("{}@shop:{}", v["id"].as_str().unwrap(), v["api_key"].as_str().unwrap()))
}

/// Term vectors of a document a key's filter hides are "not found", as its get is.
#[tokio::test]
async fn term_vectors_of_a_hidden_document_are_not_found() {
    let e = env().await;
    filtered_key(&e).await;
    let key = filtered_api_key(&e).await;
    let (s, v) = as_key(&e, &key, reqwest::Method::GET, "/products/_termvectors/2", None).await;
    assert_eq!((s, &v["found"]), (200, &json!(false)), "{v}");
    let (s, v) = as_key(&e, &key, reqwest::Method::GET, "/products/_termvectors/1", None).await;
    assert_eq!((s, &v["found"]), (200, &json!(true)), "{v}");
}

/// A stored result (async search, ES|QL, SQL) is its opener's: anyone else (here another API
/// key, with the id of the cluster owner's) gets 404 for it, its status and its delete.
#[tokio::test]
async fn stored_results_belong_to_their_opener() {
    let e = env().await;
    filtered_key(&e).await;
    let key = e.post("/v1/search/clusters/shop/keys", json!({"name": "another", "access": "data"})).await["api_key"].as_str().unwrap().to_string();
    let console = |method: &str, p: &str, b: Value| e.post("/v1/search/clusters/shop/request", json!({"method": method, "path": p, "body": b}));
    let search = body(&console("POST", "/products/_async_search?keep_on_completion=true", json!({"query": {"match_all": {}}})).await)["id"].as_str().unwrap().to_string();
    let esql = body(&console("POST", "/_query/async", json!({"query": "FROM products | LIMIT 1", "keep_on_completion": true})).await)["id"].as_str().unwrap().to_string();
    let sql = body(&console("POST", "/_sql?format=json", json!({"query": "SELECT title FROM products", "keep_on_completion": true})).await)["id"].as_str().unwrap().to_string();
    let theirs = [
        (reqwest::Method::GET, format!("/_async_search/{search}")),
        (reqwest::Method::GET, format!("/_async_search/status/{search}")),
        (reqwest::Method::DELETE, format!("/_async_search/{search}")),
        (reqwest::Method::GET, format!("/_query/async/{esql}")),
        (reqwest::Method::DELETE, format!("/_query/async/{esql}")),
        (reqwest::Method::GET, format!("/_sql/async/{sql}?format=json")),
        (reqwest::Method::DELETE, format!("/_sql/async/delete/{sql}")),
    ];
    for (m, p) in theirs {
        let (s, v) = as_key(&e, &key, m.clone(), &p, None).await;
        assert_eq!(s, 404, "{m} {p}: {v}");
    }
    let r = console("GET", &format!("/_async_search/{search}"), Value::Null).await;
    assert_eq!((r["status"].as_i64(), total(&body(&r)["response"])), (Some(200), 2), "{r}");
    assert_eq!(console("GET", &format!("/_query/async/{esql}"), Value::Null).await["status"], 200);
}

/// A browser key's refusals happen where the engine has the request parsed or runs it: a query
/// type it may not run anywhere in the request (`?q=` builds a `query_string`, `percolate`),
/// painless in any form, `explain` at any level (collapse's expansions too), inference past its
/// `max_inference`, a window past its `max_result_window` however `size` is written (a
/// `_terms_enum`'s too), and APIs that hold state or read term statistics. Each answers why.
#[tokio::test]
async fn a_browser_key_only_searches() {
    let e = env().await;
    let key = filtered_key(&e).await;
    // A `semantic_text` field: its endpoint is one the key may embed with.
    let r = e.post("/v1/search/clusters/shop/request", json!({"method": "PUT", "path": "/products/_mapping", "body": {"properties": {"body": {"type": "semantic_text"}}}})).await;
    assert_eq!(r["status"], 200, "{r}");
    let terms = |sub: Value| json!({"size": 0, "aggs": {"c": {"terms": {"field": "color.keyword"}, "aggs": {"t": sub}}}});
    let five_texts: Vec<Value> = (0..5).map(|i| json!({"sparse_vector": {"field": "body", "inference_id": ".elser-2-elasticsearch", "query": format!("text {i}")}})).collect();
    let expansions: Vec<Value> = (0..101).map(|i| json!({"name": format!("i{i}"), "size": 0})).collect();
    // Why each is refused: the browser policy, `search.allow_expensive_queries: false`'s own
    // refusal, or the key's window.
    let (browser, expensive, window) = ("isn't available to a browser key", "'search.allow_expensive_queries' is set to false", "for a browser key");
    let post = reqwest::Method::POST;
    let refused = [
        (post.clone(), "/products/_search?q=title:sh*", None, browser),
        (post.clone(), "/products/_search", Some(json!({"query": {"bool": {"filter": [{"wildcard": {"title": "s*"}}]}}})), expensive),
        (post.clone(), "/products/_search", Some(json!({"size": 0, "aggs": {"suggest": {"filter": {"regexp": {"title": "s.*"}}}}})), expensive),
        (post.clone(), "/products/_search", Some(json!({"query": {"intervals": {"title": {"prefix": {"prefix": "sh"}}}}})), expensive),
        (post.clone(), "/products/_search", Some(json!({"query": {"percolate": {"field": "q", "document": {"title": "shoes"}}}})), expensive),
        (post.clone(), "/products/_search", Some(json!({"script_fields": {"x": {"script": "1"}}})), browser),
        (post.clone(), "/products/_search", Some(json!({"query": {"script_score": {"query": {"match_all": {}}, "script": {"source": "1"}}}})), browser),
        (post.clone(), "/products/_search", Some(json!({"sort": {"_script": {"type": "number", "script": "1"}}})), browser),
        (post.clone(), "/products/_search", Some(json!({"runtime_mappings": {"r": {"type": "long", "script": "emit(1)"}}})), browser),
        (post.clone(), "/products/_search", Some(terms(json!({"sum": {"script": "1"}}))), browser),
        (post.clone(), "/products/_search", Some(terms(json!({"top_hits": {"explain": true}}))), browser),
        (post.clone(), "/products/_search", Some(json!({"collapse": {"field": "color.keyword", "inner_hits": {"name": "x", "explain": true}}})), browser),
        (post.clone(), "/products/_search", Some(json!({"query": {"bool": {"should": five_texts}}})), "embedding more than [4] distinct texts"),
        (post.clone(), "/products/_search", Some(json!({"query": {"sparse_vector": {"field": "title", "inference_id": "someone-elses", "query": "shoes"}}})), "inference endpoint [someone-elses]"),
        (post.clone(), "/products/_search", Some(json!({"collapse": {"field": "color.keyword", "inner_hits": expansions}})), "expanding more than [100]"),
        (post.clone(), "/products/_search?explain=true", Some(json!({})), browser),
        (post.clone(), "/products/_search", Some(json!({"size": "5000"})), window),
        (post.clone(), "/products/_search?size=600", Some(json!({"from": 500})), window),
        (post.clone(), "/products/_terms_enum", Some(json!({"field": "color.keyword", "size": 1001})), browser),
        (post.clone(), "/products/_search?scroll=1m", Some(json!({})), browser),
        (post.clone(), "/products/_pit?keep_alive=1m", None, browser),
        (post.clone(), "/products/_async_search?keep_on_completion=true", Some(json!({})), browser),
        (reqwest::Method::GET, "/products/_termvectors/1", None, browser),
        (post.clone(), "/products/_explain/1", Some(json!({"query": {"match_all": {}}})), browser),
    ];
    for (m, p, b, why) in refused {
        let (s, v) = as_key(&e, &key, m.clone(), p, b.clone()).await;
        assert!((400..500).contains(&s) && s != 429 && v.to_string().contains(why), "{m} {p} {b:?}: {s} {v}");
    }
    let (s, v) = as_key(&e, &key, post.clone(), "/products/_search", Some(json!({"query": {"match": {"title": {"query": "shose", "fuzziness": "AUTO"}}}}))).await;
    assert_eq!((s, total(&v)), (200, 1), "search-as-you-type still works: {v}");
    // A reranker the owner allowed still reranks at most `max_rerank_window` documents.
    let rr = e.post("/v1/search/clusters/shop/browser-keys", json!({"name": "rr", "indices": ["products"], "limits": {"inference_endpoints": [".rerank-v1-elasticsearch"]}})).await["api_key"].as_str().unwrap().to_string();
    let rerank = json!({"retriever": {"text_similarity_reranker": {"retriever": {"standard": {"query": {"match_all": {}}}}, "field": "title", "inference_text": "shoes", "rank_window_size": 101}}});
    let (s, v) = as_key(&e, &rr, post.clone(), "/products/_search", Some(rerank)).await;
    assert!(s == 400 && v.to_string().contains("max_rerank_window"), "{s} {v}");
    // An `_msearch` whose items each need inference after the key's bucket is empty runs none:
    // a refused inference charge gives no token back for the next item.
    let slow = e.post("/v1/search/clusters/shop/browser-keys", json!({"name": "slow", "indices": ["products"], "rate_limit": {"per_minute": 3}})).await["api_key"].as_str().unwrap().to_string();
    let lines: String = (0..3).map(|i| format!("{{\"index\":\"products\"}}\n{{\"query\":{{\"sparse_vector\":{{\"field\":\"body\",\"inference_id\":\".elser-2-elasticsearch\",\"query\":\"t{i}\"}}}}}}\n")).collect();
    let r = e.http.post(format!("{}/_msearch", e.es)).header("authorization", format!("ApiKey {slow}")).header("content-type", "application/x-ndjson").body(lines).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    let statuses: Vec<i64> = v["responses"].as_array().unwrap().iter().map(|i| i["status"].as_i64().unwrap_or(200)).collect();
    assert_eq!(statuses, vec![429, 429, 429], "{v}");
    // `_terms_enum` works for a key that reads every document (its internal `_field_caps` call
    // runs as the key).
    let open = e.post("/v1/search/clusters/shop/browser-keys", json!({"name": "open", "indices": ["products"]})).await["api_key"].as_str().unwrap().to_string();
    let (s, v) = as_key(&e, &open, post, "/products/_terms_enum", Some(json!({"field": "color.keyword", "string": "r"}))).await;
    assert_eq!((s, &v["terms"]), (200, &json!(["red"])), "{v}");
}

/// One fetch budget per request: hits, and `top_hits` in every bucket, together within the
/// key's `max_result_window`.
#[tokio::test]
async fn a_browser_keys_fetches_share_one_budget() {
    let e = env().await;
    filtered_key(&e).await;
    let bulk: String = (0..10).map(|i| format!("{{\"index\":{{\"_index\":\"items\",\"_id\":\"{i}\"}}}}\n{{\"color\":\"{}\"}}\n", if i % 2 == 0 { "red" } else { "blue" })).collect();
    let r = e.post("/v1/search/clusters/shop/request", json!({"method": "POST", "path": "/_bulk?refresh=true", "body": bulk})).await;
    assert_eq!(r["status"], 200, "{r}");
    let key = e.post("/v1/search/clusters/shop/browser-keys", json!({"name": "five", "indices": ["items"], "limits": {"max_result_window": 5}})).await["api_key"].as_str().unwrap().to_string();
    let search = |size: i64, top: i64| json!({"size": size, "aggs": {"c": {"terms": {"field": "color.keyword"}, "aggs": {"t": {"top_hits": {"size": top}}}}}});
    let (s, v) = as_key(&e, &key, reqwest::Method::POST, "/items/_search", Some(search(1, 2))).await;
    assert_eq!(s, 200, "1 hit and 2 buckets of 2: {v}");
    let (s, v) = as_key(&e, &key, reqwest::Method::POST, "/items/_search", Some(search(3, 3))).await;
    assert_eq!(s, 400, "3 hits and 2 buckets of 3: {v}");
}

/// A browser key pays for every item the engine parses (an `_msearch`'s searches however its
/// lines or URL are written, an `_mget`'s documents however its body is given), each search
/// checked; more than its `max_items` is a 429, an `_msearch`'s counted from its lines before any
/// is parsed. A request refused after its body was parsed keeps its admission token as payment.
/// Browser requests running at once are capped per key and per cluster, below the cluster's own
/// permits: past either a browser request gets 429 while the owner's clients go on.
#[tokio::test]
async fn browser_keys_pay_per_item_and_leave_the_owner_permits() {
    let e = env().await;
    filtered_key(&e).await;
    let k = e.post("/v1/search/clusters/shop/browser-keys", json!({"name": "few", "indices": ["products"], "rate_limit": {"per_minute": 10}})).await;
    let key = k["api_key"].as_str().unwrap().to_string();
    let send = |method: reqwest::Method, url: String, body: String| e.http.request(method, url).header("authorization", format!("ApiKey {key}")).header("content-type", "application/x-ndjson").body(body).send();
    let items = |n: usize| "{\"index\":\"products\"}\n{\"size\":0}\n".repeat(n);
    // A trailing slash: 7 searches, 7 tokens.
    assert_eq!(send(reqwest::Method::POST, format!("{}/_msearch/", e.es), items(7)).await.unwrap().status().as_u16(), 200);
    // An `_mget` given in `?source=`: 4 documents, and 3 tokens left.
    let url = format!("{}/products/_mget?source={}&source_content_type=application/json", e.es, urlencoding::encode(r#"{"ids":["1","2","3","4"]}"#));
    let r = send(reqwest::Method::GET, url, String::new()).await.unwrap();
    assert_eq!(r.status().as_u16(), 429, "3 tokens left can't pay for 4 documents");
    assert!(r.headers().contains_key("retry-after"));
    // The refused `_mget` was parsed: its admission stays paid, 2 tokens left. Blank lines are
    // empty headers: 2 searches, 2 tokens, and nothing left for another request.
    let blank = "{\"index\":\"products\"}\n{}\n\n{}\n".to_string();
    assert_eq!(send(reqwest::Method::POST, format!("{}/_msearch", e.es), blank).await.unwrap().status().as_u16(), 200, "2 searches, 2 tokens");
    assert_eq!(send(reqwest::Method::POST, format!("{}/products/_search", e.es), String::new()).await.unwrap().status().as_u16(), 429, "the refused _mget's admission stayed paid");
    // Each search is checked, an empty header's too; more than the key's 100 items is a 429.
    let big = e.post("/v1/search/clusters/shop/browser-keys", json!({"name": "big", "indices": ["products"], "rate_limit": {"per_minute": 10000}})).await["api_key"].as_str().unwrap().to_string();
    let as_big = |body: String| e.http.post(format!("{}/_msearch", e.es)).header("authorization", format!("ApiKey {big}")).header("content-type", "application/x-ndjson").body(body).send();
    let explained = "{\"index\":\"products\"}\n{}\n\n{\"explain\":true}\n".to_string();
    assert_eq!(as_big(explained).await.unwrap().status().as_u16(), 400);
    // Past 100 searches by its line count, before any line is parsed: an unparseable line after
    // the cap still gets the item count's 429.
    let r = as_big(items(101) + "not json\n{\n").await.unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    assert!(status == 429 && text.contains("max_items"), "{status} {text}");

    // Running at once: a cluster of 8 permits gives browser keys 4. Four browser requests held
    // (as running requests hold their leases): the next browser request gets 429, the owner's
    // still runs.
    let (s, _) = e.call(reqwest::Method::PATCH, "/v1/search/clusters/shop", Some(json!({"max_in_flight": 8}))).await;
    assert_eq!(s, 200);
    let wide = e.post("/v1/search/clusters/shop/browser-keys", json!({"name": "wide", "indices": ["products"], "limits": {"max_concurrent": 10}})).await["api_key"].as_str().unwrap().to_string();
    let mut held = Vec::new();
    for _ in 0..4 {
        let mut r = search_es::EsRequest::new("GET", "/");
        r.headers.push(("authorization".into(), format!("ApiKey {wide}")));
        let Ok(lease) = e.control.node.enter_from(&r, None, search_server::Origin::Client("203.0.113.9".parse().unwrap())).await else { panic!("not admitted") };
        held.push(lease);
    }
    let (s, _) = as_key(&e, &wide, reqwest::Method::POST, "/products/_search", Some(json!({}))).await;
    assert_eq!(s, 429, "the cluster's browser permits are taken");
    let c = e.post("/v1/search/clusters/shop/password", json!({})).await;
    let (user, pw) = creds(&c);
    let (s, _) = e.es(&user, &pw, reqwest::Method::POST, "/products/_search", Some(json!({}))).await;
    assert_eq!(s, 200, "the owner's request isn't held back by browser keys");
    held.clear();
    let (s, _) = as_key(&e, &wide, reqwest::Method::POST, "/products/_search", Some(json!({}))).await;
    assert_eq!(s, 200);

    // At most 20 browser keys a cluster.
    for i in 4..20 {
        e.post("/v1/search/clusters/shop/browser-keys", json!({"name": format!("k{i}"), "indices": ["products"]})).await;
    }
    let (s, v) = e.call(reqwest::Method::POST, "/v1/search/clusters/shop/browser-keys", Some(json!({"name": "k20", "indices": ["products"]}))).await;
    assert_eq!(s, 409, "{v}");
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

/// Keys narrowed by `indices` and `access`: `read` searches but writes nothing, `append` adds to
/// its data stream but reads nothing and writes nowhere else, and an OpenSearch-flavor `read`
/// user is held to its indices as well.
#[tokio::test]
async fn narrowed_keys() {
    let e = env().await;
    let e = &e;
    let c = e.post("/v1/search/clusters", json!({"name": "narrow"})).await;
    let (user, pw) = creds(&c);
    for (path, doc) in [("/products/_doc/1?refresh=true", json!({"t": "shoe"})), ("/private/_doc/1?refresh=true", json!({"t": "x"}))] {
        assert_eq!(e.es(&user, &pw, reqwest::Method::PUT, path, Some(doc)).await.0, 201);
    }
    let (s, v) = e.es(&user, &pw, reqwest::Method::PUT, "/_index_template/logs", Some(json!({"index_patterns": ["logs-*"], "data_stream": {}}))).await;
    assert_eq!(s, 200, "{v}");
    let key = |body: Value| async move { e.post("/v1/search/clusters/narrow/keys", body).await };
    let send = |k: &Value, m: reqwest::Method, path: &str, body: Option<Value>| {
        let mut r = e.http.request(m, format!("{}{path}", e.es)).header("Authorization", format!("ApiKey {}", k["api_key"].as_str().unwrap()));
        if let Some(b) = body {
            r = r.json(&b);
        }
        async move { r.send().await.unwrap().status().as_u16() }
    };
    let read = key(json!({"name": "web", "access": "read", "indices": ["prod*"]})).await;
    assert_eq!((read["access"].as_str(), &read["indices"]), (Some("read"), &json!(["prod*"])));
    assert_eq!(send(&read, reqwest::Method::POST, "/products/_search", Some(json!({}))).await, 200);
    assert_eq!(send(&read, reqwest::Method::PUT, "/products/_doc/2", Some(json!({"t": "y"}))).await, 403);
    assert_eq!(send(&read, reqwest::Method::POST, "/private/_search", Some(json!({}))).await, 403);
    let drain = key(json!({"name": "drain", "access": "append", "indices": ["logs-app"]})).await;
    assert_eq!(send(&drain, reqwest::Method::POST, "/logs-app/_doc?refresh=true", Some(json!({"@timestamp": "2026-11-21T00:00:00Z", "m": "hi"}))).await, 201);
    assert_eq!(send(&drain, reqwest::Method::POST, "/logs-app/_search", Some(json!({}))).await, 403);
    assert_eq!(send(&drain, reqwest::Method::POST, "/logs-other/_doc", Some(json!({"@timestamp": "2026-11-21T00:00:00Z"}))).await, 403);
    assert_eq!(send(&drain, reqwest::Method::PUT, "/products/_doc/3", Some(json!({"t": "z"}))).await, 403);
    let (_, v) = e.es(&user, &pw, reqwest::Method::GET, "/logs-app/_count", None).await;
    assert_eq!(v["count"], 1, "{v}");
    // A data key restricted to its indices.
    let data = key(json!({"name": "app", "access": "data", "indices": ["products"]})).await;
    assert_eq!(send(&data, reqwest::Method::PUT, "/products/_doc/4", Some(json!({"t": "w"}))).await, 201);
    assert_eq!(send(&data, reqwest::Method::PUT, "/private/_doc/4", Some(json!({"t": "w"}))).await, 403);
    // Templates are the whole cluster's (this one would match `private`): not a narrowed key's.
    assert_eq!(send(&data, reqwest::Method::PUT, "/_index_template/sneaky", Some(json!({"index_patterns": ["priv*"]}))).await, 403);
    for (bad, why) in [
        (json!({"name": "x", "access": "admin", "indices": ["a"]}), "admin"),
        (json!({"name": "x", "access": "append", "indices": ["logs-*"]}), "pattern"),
        (json!({"name": "x", "access": "append"}), "names its indices"),
        (json!({"name": "x", "access": "read", "indices": ["other:logs"]}), "remote"),
        (json!({"name": "x", "access": "read", "indices": [".security"]}), "start with"),
    ] {
        let (s, v) = e.call(reqwest::Method::POST, "/v1/search/clusters/narrow/keys", Some(bad.clone())).await;
        assert!(s == 400 && v.to_string().contains(why), "{bad}: {s} {v}");
    }
    // OpenSearch: a read user of its own, on its indices only.
    let c = e.post("/v1/search/clusters", json!({"name": "narrow-os", "flavor": "opensearch"})).await;
    let (user, pw) = creds(&c);
    for path in ["/products/_doc/1?refresh=true", "/private/_doc/1?refresh=true"] {
        assert_eq!(e.es(&user, &pw, reqwest::Method::PUT, path, Some(json!({"t": "x"}))).await.0, 201);
    }
    let k = e.post("/v1/search/clusters/narrow-os/keys", json!({"name": "web", "access": "read", "indices": ["products"]})).await;
    let (ku, kp) = (k["username"].as_str().unwrap(), k["password"].as_str().unwrap());
    assert_eq!(e.es(ku, kp, reqwest::Method::POST, "/products/_search", Some(json!({}))).await.0, 200);
    assert_eq!(e.es(ku, kp, reqwest::Method::PUT, "/products/_doc/2", Some(json!({"t": "y"}))).await.0, 403);
    assert_eq!(e.es(ku, kp, reqwest::Method::POST, "/private/_search", Some(json!({}))).await.0, 403);
}

/// Relevance tuning through `/v1/search`: a synonyms set a search then uses, a query ruleset that
/// pins a document, an index's settings (roll's suggestions on; a private setting refused) and
/// one document's explanation.
#[tokio::test]
async fn relevance_tuning_routes() {
    let e = env().await;
    let e = &e;
    e.post("/v1/search/clusters", json!({"name": "rel"})).await;
    let base = "/v1/search/clusters/rel";
    let call = |m: reqwest::Method, path: &str, body: Option<Value>| {
        let p = format!("{base}{path}");
        async move { e.call(m, &p, body).await }
    };
    let (s, v) = call(reqwest::Method::PUT, "/synonyms/furniture", Some(json!({"synonyms_set": [{"id": "couch", "synonyms": "couch, sofa"}]}))).await;
    assert_eq!((s, v["result"].as_str()), (201, Some("created")), "Elasticsearch's status comes through: {v}");
    let index = json!({
        "settings": {"analysis": {"filter": {"syn": {"type": "synonym_graph", "synonyms_set": "furniture", "updateable": true}}, "analyzer": {"syn": {"tokenizer": "standard", "filter": ["lowercase", "syn"]}}}},
        "mappings": {"properties": {"title": {"type": "text", "analyzer": "standard", "search_analyzer": "syn"}}}
    });
    assert_eq!(call(reqwest::Method::PUT, "/indices/shop", Some(index)).await.0, 200);
    for (id, title) in [("1", "red sofa"), ("2", "blue chair"), ("3", "green lamp")] {
        assert_eq!(call(reqwest::Method::PUT, &format!("/indices/shop/docs/{id}?refresh=true"), Some(json!({"title": title}))).await.0, 201);
    }
    let search = |q: Value| call(reqwest::Method::POST, "/indices/shop/search", Some(q));
    let (s, v) = search(json!({"query": {"match": {"title": "couch"}}})).await;
    assert_eq!((s, v["hits"]["hits"][0]["_id"].as_str()), (200, Some("1")), "the set's synonym matched: {v}");
    // One rule more, then the set lists it.
    assert!(call(reqwest::Method::PUT, "/synonyms/furniture/seat", Some(json!({"synonyms": "seat, chair"}))).await.0 < 300);
    let (s, v) = call(reqwest::Method::GET, "/synonyms/furniture", None).await;
    assert_eq!((s, v["count"].as_i64()), (200, Some(2)), "{v}");
    let (s, v) = search(json!({"query": {"match": {"title": "seat"}}})).await;
    assert_eq!((s, v["hits"]["hits"][0]["_id"].as_str()), (200, Some("2")), "{v}");
    let (s, v) = call(reqwest::Method::GET, "/synonyms", None).await;
    assert_eq!((s, v["count"].as_i64()), (200, Some(1)), "{v}");
    // A ruleset pins the lamp to the top of a search for "sofa".
    let rules = json!({"rules": [{"rule_id": "lamp", "type": "pinned", "criteria": [{"type": "exact", "metadata": "q", "values": ["sofa"]}], "actions": {"ids": ["3"]}}]});
    let (s, v) = call(reqwest::Method::PUT, "/query_rules/promos", Some(rules)).await;
    assert!(s < 300, "{s} {v}");
    let (s, v) = call(reqwest::Method::POST, "/query_rules/promos/test", Some(json!({"match_criteria": {"q": "sofa"}}))).await;
    assert_eq!((s, v["total_matched_rules"].as_i64()), (200, Some(1)), "{v}");
    let (s, v) = search(json!({"retriever": {"rule": {"retriever": {"standard": {"query": {"match": {"title": "sofa"}}}}, "match_criteria": {"q": "sofa"}, "ruleset_ids": ["promos"]}}})).await;
    let ids: Vec<&str> = v["hits"]["hits"].as_array().unwrap().iter().filter_map(|h| h["_id"].as_str()).collect();
    assert_eq!((s, ids), (200, vec!["3", "1"]), "{v}");
    assert_eq!(call(reqwest::Method::GET, "/query_rules", None).await.1["count"], 1);
    // Settings: roll's suggestions on, a private setting refused as Elasticsearch refuses it.
    let (s, v) = call(reqwest::Method::PUT, "/indices/shop/settings", Some(json!({"index.roll.analytics.enabled": true, "index.roll.suggestions.enabled": true}))).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = call(reqwest::Method::GET, "/indices/shop/settings?flat_settings=true", None).await;
    assert_eq!(v["shop"]["settings"]["index.roll.suggestions.enabled"], "true", "{v}");
    let (s, v) = call(reqwest::Method::PUT, "/indices/shop/settings", Some(json!({"index.roll.suggestions.source": "shop"}))).await;
    assert!(s == 400 && v.to_string().contains("can not update private setting"), "{s} {v}");
    // A write names one index.
    assert_eq!(call(reqwest::Method::PUT, "/indices/shop*/settings", Some(json!({"index.refresh_interval": "1s"}))).await.0, 400);
    // Why a document matched.
    let (s, v) = call(reqwest::Method::POST, "/indices/shop/explain/1", Some(json!({"query": {"match": {"title": "sofa"}}}))).await;
    assert_eq!((s, &v["matched"]), (200, &json!(true)), "{v}");
    // Elasticsearch's answers come through: a set in use isn't deleted; `_` ids are refused.
    let (s, v) = call(reqwest::Method::DELETE, "/synonyms/furniture", None).await;
    assert_eq!(s, 400, "{v}");
    assert_eq!(call(reqwest::Method::GET, "/synonyms/_all", None).await.0, 400);
    assert_eq!(call(reqwest::Method::DELETE, "/query_rules/promos", None).await.0, 200);
    assert_eq!(call(reqwest::Method::GET, "/query_rules/promos", None).await.0, 404);
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
    /// Writes of `index.latest` fail (after the `index-N` they name landed).
    fail_latest: Arc<std::sync::atomic::AtomicBool>,
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
            "PUT" if m.fail_latest.load(Relaxed) && key.ends_with("/index.latest") => StatusCode::FORBIDDEN.into_response(),
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
        let client = search_es::s3::Client::new(at, creds, &search_es::egress::Egress::of(|_| None, true).unwrap()).unwrap();
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

/// A repository's endpoint written as an address is checked against what the node's own names
/// resolve to when its client is built, not against a copy that may be empty or stale: a
/// deny-list name leading to it is refused before anything connects.
#[tokio::test]
async fn a_repository_endpoint_at_one_of_the_nodes_own_addresses_is_refused() {
    let e = env_with(|c| c.node_settings.push((search_control::EGRESS_DENY_NAMES.into(), "1.2.3.4".into()))).await;
    let (u, p) = creds(&e.post("/v1/search/clusters", json!({"name": "own"})).await);
    let repo = json!({"type": "s3", "settings": {"bucket": "backups", "endpoint": "http://1.2.3.4:9", "path_style_access": true}});
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/own?verify=false", Some(repo)).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(&u, &p, reqwest::Method::POST, "/_snapshot/own/_verify", None).await;
    assert!(s >= 400 && v.to_string().contains("[1.2.3.4] is an address roll may not reach"), "{s} {v}");
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

/// roll secrets as a fake (rolld's is `Secrets::bind_ref`, `resolve_as` and
/// `digest_as`): keys by reference with a version, the clusters a project's canvas holds, who may
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
        // As roll secrets' `bind_ref`: a signed-in person (an admin too) is themselves; the
        // operator's token or an open daemon is the operator.
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
    fake.set(access, "AKIDONE");
    fake.set(secret, "fake-secret-one");
    e.control.set_secrets(fake.clone());
    let repo = json!({"type": "s3", "settings": {"bucket": "backups", "base_path": "refs", "endpoint": endpoint, "path_style_access": true, "access_key": access, "secret_key": secret}});
    let route = |cluster: &str| format!("/v1/search/clusters/{cluster}/repositories/backup");

    // The cluster's own endpoint (its password: a cluster admin) can't name a reference, not
    // even saying who wrote it: a client's maker header is dropped, there and through the console.
    let forged = reqwest::Client::new()
        .put(format!("{}/_snapshot/backup", e.es))
        .basic_auth(&u, Some(&p))
        .header(search_es::secret_refs::MAKER_HEADER, "usr_ada")
        .json(&repo)
        .send()
        .await
        .unwrap();
    let (s, v) = (forged.status().as_u16(), forged.json::<Value>().await.unwrap());
    assert_eq!((s, v["error"]["type"].as_str()), (400, Some("settings_exception")), "{v}");
    assert!(v["error"]["reason"].as_str().unwrap().contains("roll's API"), "{v}");
    let console = json!({"method": "PUT", "path": "/_snapshot/backup", "body": repo.clone(), "headers": [[search_es::secret_refs::MAKER_HEADER, "usr_ada"]]});
    let (s, v) = e.call_as("ada", reqwest::Method::POST, &format!("/v1/search/clusters/{id}/request"), Some(console)).await;
    assert!(s == 200 && v["status"] == 400 && v["body"].as_str().unwrap().contains("roll's API"), "{v}");
    // Through roll's API: someone who may not reveal it is refused, and so is a cluster no
    // project holds.
    let (s, v) = e.call_as("ada", reqwest::Method::PUT, &route(&id), Some(repo.clone())).await;
    assert_eq!(s, 403, "{v}");
    fake.revealers.lock().unwrap().push("usr_ada".into());
    // Only exactly `roll://…` is a reference (roll secrets' parser): a near miss is refused, never
    // kept as a value.
    for near in [" roll://production/KEY", "ROLL://production/KEY"] {
        let mut bad = repo.clone();
        bad["settings"]["access_key"] = json!(near);
        bad["settings"]["secret_key"] = json!(near);
        let (s, v) = e.call_as("ada", reqwest::Method::PUT, &route(&id), Some(bad)).await;
        assert!(s == 400 && v.to_string().contains("written like a roll secrets reference"), "{near:?}: {v}");
    }
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

    // The cluster taken off the project's canvas: the next snapshot fails, with why.
    fake.canvas.lock().unwrap().clear();
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, &snapshot("off-canvas"), Some(json!({"indices": "docs"}))).await;
    assert!(s == 500 && v.to_string().contains("isn't on any project's canvas"), "{v}");
    fake.canvas.lock().unwrap().push(id.clone());

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

/// A branch inherits its parent's bucket repository, SLM policy and watch, but runs none of
/// them as the parent: the repository is read-only there (restores work, snapshots don't), the
/// policy and its retention don't run, and the watch is inactive, so every trigger reaches the
/// webhook once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_branch_runs_none_of_its_parents_jobs() {
    use reqwest::Method;
    let (endpoint, bucket) = MockBucket::start().await;
    let hooks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_port = hook_listener.local_addr().unwrap().port();
    let counter = hooks.clone();
    let router = axum::Router::new().fallback(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async { "ok" }
    });
    tokio::spawn(async move { axum::serve(hook_listener, router).await });
    let e = env_with(|c| {
        c.node_settings.push((search_control::S3_ALLOW_PRIVATE.into(), "true".into()));
        c.node_settings.push((search_es::xpack::watcher::http::ALLOW_PRIVATE.into(), "true".into()));
    })
    .await;
    let c = e.post("/v1/search/clusters", json!({"name": "prod"})).await;
    let id = c["id"].as_str().unwrap().to_string();
    let (u, p) = creds(&c);
    let repo = json!({"type": "s3", "settings": {"bucket": "backups", "base_path": "prod", "endpoint": endpoint, "path_style_access": true}});
    let watch = json!({
        "trigger": {"schedule": {"interval": "1s"}},
        "input": {"simple": {}},
        // Every execution sends: no 5s default throttle between them.
        "throttle_period": "0s",
        "actions": {"hook": {"webhook": {"method": "post", "scheme": "http", "host": "127.0.0.1", "port": hook_port, "path": "/hook", "body": "fired"}}},
    });
    let calls = [
        (Method::PUT, "/docs/_doc/1?refresh=true", json!({"a": 1})),
        (Method::PUT, "/_snapshot/backup", repo),
        (Method::PUT, "/_snapshot/backup/base?wait_for_completion=true", json!({"indices": "docs", "metadata": {"policy": "nightly"}})),
        (Method::PUT, "/_cluster/settings", json!({"persistent": {"slm.minimum_interval": "0s"}})),
        // The parent's SLM is stopped, so only a branch could run the policy.
        (Method::POST, "/_slm/stop", Value::Null),
        (Method::PUT, "/_slm/policy/nightly", json!({"schedule": "1s", "name": "<nightly-{now/d}>", "repository": "backup", "retention": {"expire_after": "1ms"}})),
        (Method::PUT, "/_watcher/watch/ping", watch),
    ];
    for (m, uri, body) in calls {
        let (s, v) = e.es(&u, &p, m, uri, Some(body).filter(|b| !b.is_null())).await;
        assert!(s < 300, "{uri}: {v}");
    }
    let objects = bucket.keys("prod/");
    let b = e.post(&format!("/v1/search/clusters/{id}/branches"), json!({"name": "prod-dev"})).await;
    let (bu, bp) = creds(&b);

    // The repository lists the parent's snapshots and restores them, and refuses writes.
    let (s, v) = e.es(&bu, &bp, Method::GET, "/_snapshot/backup", None).await;
    assert_eq!((s, &v["backup"]["settings"]["readonly"]), (200, &json!("true")), "{v}");
    let (s, v) = e.es(&bu, &bp, Method::POST, "/_snapshot/backup/base/_restore?wait_for_completion=true", Some(json!({"rename_pattern": "docs", "rename_replacement": "restored"}))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(e.es(&bu, &bp, Method::GET, "/restored/_count", None).await.1["count"], 1);
    let (s, v) = e.es(&bu, &bp, Method::PUT, "/_snapshot/backup/mine?wait_for_completion=true", Some(json!({"indices": "docs"}))).await;
    assert_eq!((s, v["error"]["type"].as_str()), (500, Some("repository_exception")), "{v}");
    let (s, v) = e.es(&bu, &bp, Method::DELETE, "/_snapshot/backup/base", None).await;
    assert_eq!(s, 500, "{v}");
    let (s, v) = e.es(&bu, &bp, Method::PUT, "/_snapshot/backup/base/_clone/copy", Some(json!({"indices": "docs"}))).await;
    assert_eq!((s, v["error"]["type"].as_str()), (500, Some("repository_exception")), "{v}");
    // Another spelling of the parent's place is the parent's too.
    let again = json!({"type": "s3", "settings": {"bucket": "backups", "base_path": "prod/", "endpoint": endpoint, "path_style_access": true}});
    let (s, v) = e.es(&bu, &bp, Method::PUT, "/_snapshot/mine", Some(again)).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(&bu, &bp, Method::PUT, "/_snapshot/mine/mine?wait_for_completion=true", Some(json!({"indices": "docs"}))).await;
    assert_eq!((s, v["error"]["type"].as_str()), (500, Some("repository_exception")), "{v}");

    // SLM running on the branch neither runs the inherited policy nor applies its retention.
    let (s, v) = e.es(&bu, &bp, Method::POST, "/_slm/start", None).await;
    assert_eq!(s, 200, "{v}");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let (s, v) = e.es(&bu, &bp, Method::POST, "/_slm/_execute_retention", None).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = e.es(&bu, &bp, Method::GET, "/_slm/policy/nightly", None).await;
    let policy = &v["nightly"];
    assert!(policy["last_failure"]["details"].as_str().unwrap_or_default().contains("inherited from the parent"), "{v}");
    assert!(policy.get("next_execution_millis").is_none(), "{v}");
    assert_eq!((&policy["stats"]["snapshots_taken"], &policy["stats"]["snapshots_failed"], &policy["stats"]["snapshot_deletion_failures"]), (&json!(0), &json!(0), &json!(0)), "{v}");
    let (s, v) = e.es(&bu, &bp, Method::PUT, "/_slm/policy/nightly/_execute", None).await;
    assert_eq!((s, v["error"]["type"].as_str()), (409, Some("illegal_state_exception")), "{v}");
    // Marked once: later loads don't mark again.
    assert!(!e.control.store.by_name("prod-dev").await.unwrap().unwrap().inherit_pending);

    // The watch came inactive: the webhook gets one call per execution of the parent's.
    let (_, v) = e.es(&bu, &bp, Method::GET, "/_watcher/watch/ping", None).await;
    assert_eq!(v["status"]["state"]["active"], false, "{v}");
    let (s, v) = e.es(&u, &p, Method::PUT, "/_watcher/watch/ping/_deactivate", None).await;
    assert_eq!(s, 200, "{v}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    e.es(&u, &p, Method::POST, "/.watcher-history-*/_refresh?expand_wildcards=all", None).await;
    let (_, v) = e.es(&u, &p, Method::GET, "/.watcher-history-*/_count?expand_wildcards=all", None).await;
    let executions = v["count"].as_u64().unwrap() as usize;
    assert!(executions >= 2, "{v}");
    assert_eq!(hooks.load(std::sync::atomic::Ordering::SeqCst), executions);

    // The parent's snapshots and bucket are as they were.
    let (_, v) = e.es(&u, &p, Method::GET, "/_snapshot/backup/_all", None).await;
    let names: Vec<&str> = v["snapshots"].as_array().unwrap().iter().filter_map(|s| s["snapshot"].as_str()).collect();
    assert_eq!(names, ["base"], "{v}");
    assert_eq!(bucket.keys("prod/"), objects);

    // A branch made before marking existed (no pending mark) keeps its jobs as its own.
    let old = e.post(&format!("/v1/search/clusters/{id}/branches"), json!({"name": "prod-old"})).await;
    e.control.store.set_inherit_pending("prod-old", false).await.unwrap();
    let (ou, op) = creds(&old);
    let (_, v) = e.es(&ou, &op, Method::GET, "/_snapshot/backup", None).await;
    assert_eq!(v["backup"]["settings"]["readonly"], Value::Null, "{v}");
    let (_, v) = e.es(&ou, &op, Method::GET, "/_slm/policy/nightly", None).await;
    assert!(v["nightly"]["next_execution_millis"].is_i64() && v["nightly"].get("last_failure").is_none(), "{v}");
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

    // Elasticsearch's repository at base path `roll`, under a roll repository at the bucket's
    // root (whose own files would be under `roll/`): neither a cleanup nor a snapshot touches it.
    let root = json!({"type": "s3", "settings": {"bucket": "backups", "endpoint": endpoint, "path_style_access": true}});
    assert_eq!(es(reqwest::Method::PUT, "/_snapshot/root", Some(root)).await.0, 200);
    put("roll/index.latest".into(), 20i64.to_be_bytes().to_vec());
    put("roll/indices/Xq/0/__a".into(), b"shard data".to_vec());
    expect("cleanup", es(reqwest::Method::POST, "/_snapshot/root/_cleanup", None).await, 500, "isn't roll's");
    expect("snapshot", es(reqwest::Method::PUT, "/_snapshot/root/s?wait_for_completion=true", Some(json!({"indices": "docs"}))).await, 500, "isn't roll's");
    assert_eq!(object("roll/index.latest"), 20i64.to_be_bytes());
    assert_eq!(object("roll/indices/Xq/0/__a"), b"shard data");
    // Elasticsearch finding its generation by listing writes no `index.latest`: its `index-N` and
    // shard files under the prefix still make it someone else's. Registering it is refused;
    // registered anyway (no `verify`), it is neither written nor cleaned, and a listing across
    // repositories fails as Elasticsearch's does.
    put("nested/roll/index-5".into(), br#"{"min_version":"7.12.0","uuid":"u","snapshots":[]}"#.to_vec());
    put("nested/roll/indices/Xq/0/__b".into(), b"shard data".to_vec());
    let nested = json!({"type": "s3", "settings": {"bucket": "backups", "base_path": "nested", "endpoint": endpoint, "path_style_access": true}});
    expect("register", es(reqwest::Method::PUT, "/_snapshot/nested", Some(nested.clone())).await, 500, "isn't roll's");
    expect("not registered", es(reqwest::Method::GET, "/_snapshot/nested", None).await, 404, "repository_missing_exception");
    assert_eq!(es(reqwest::Method::PUT, "/_snapshot/nested?verify=false", Some(nested)).await.0, 200);
    expect("cleanup without index.latest", es(reqwest::Method::POST, "/_snapshot/nested/_cleanup", None).await, 500, "isn't roll's");
    expect("snapshot without index.latest", es(reqwest::Method::PUT, "/_snapshot/nested/s?wait_for_completion=true", Some(json!({"indices": "docs"}))).await, 500, "isn't roll's");
    expect("listing", es(reqwest::Method::GET, "/_snapshot/_all/_all", None).await, 500, "isn't roll's");
    assert_eq!(object("nested/roll/indices/Xq/0/__b"), b"shard data");
    assert!(bucket.keys("nested/").len() == 2, "{:?}", bucket.keys("nested/"));
    // Elasticsearch's first snapshot before it's finalized: only its shard files are there.
    put("mid/roll/indices/Xq/0/__c".into(), b"shard data".to_vec());
    put("mid/roll/indices/Xq/0/snap-Yz.dat".into(), b"shard snapshot".to_vec());
    let mid = json!({"type": "s3", "settings": {"bucket": "backups", "base_path": "mid", "endpoint": endpoint, "path_style_access": true}});
    assert_eq!(es(reqwest::Method::PUT, "/_snapshot/mid?verify=false", Some(mid)).await.0, 200);
    expect("snapshot beside a first snapshot", es(reqwest::Method::PUT, "/_snapshot/mid/s?wait_for_completion=true", Some(json!({"indices": "docs"}))).await, 500, "isn't roll's");
    expect("cleanup beside a first snapshot", es(reqwest::Method::POST, "/_snapshot/mid/_cleanup", None).await, 500, "isn't roll's");
    assert!(bucket.keys("mid/").len() == 2, "{:?}", bucket.keys("mid/"));
    // `index-N` is the commit: an `index.latest` that can't be written after it leaves the
    // snapshot listed with every file it needs.
    bucket.fail_latest.store(true, std::sync::atomic::Ordering::Relaxed);
    let (s, v) = es(reqwest::Method::PUT, "/_snapshot/backup/late?wait_for_completion=true", Some(json!({"indices": "docs"}))).await;
    bucket.fail_latest.store(false, std::sync::atomic::Ordering::Relaxed);
    expect("committed without index.latest", (s, v), 200, "SUCCESS");
    expect("listed", es(reqwest::Method::GET, "/_snapshot/backup/late", None).await, 200, "SUCCESS");
    expect("restored", e.es(&u, &p, reqwest::Method::POST, "/_snapshot/backup/late/_restore?wait_for_completion=true", Some(json!({"rename_pattern": "docs", "rename_replacement": "late"}))).await, 200, "late");

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
    // An expiring key: the user carries the expiry the route answered, and once that moment has
    // passed it is refused. No clock is waited on: the expiry is moved into the past instead.
    let k = e.post("/v1/search/clusters/os-link/keys", json!({"name": "brief", "expires_in_s": 3600})).await;
    let expires_at = k["expires_at"].as_i64().unwrap_or_else(|| panic!("no expires_at: {k}"));
    let (u, p) = (k["username"].as_str().unwrap().to_string(), k["password"].as_str().unwrap().to_string());
    assert_eq!(as_user(&u, &p, reqwest::Method::GET, "/items/_doc/2", None).await, 200);
    let user_path = format!("/_plugins/_security/api/internalusers/{}", k["id"].as_str().unwrap());
    let stored = body(&e.post("/v1/search/clusters/os-link/request", json!({"method": "GET", "path": user_path})).await);
    let at_ms: i64 = stored[k["id"].as_str().unwrap()]["attributes"]["roll_expires_at"].as_str().and_then(|v| v.parse().ok()).unwrap_or_else(|| panic!("{stored}"));
    assert_eq!(at_ms.div_euclid(1000), expires_at, "{stored}");
    let moved = e.post("/v1/search/clusters/os-link/request", json!({"method": "PATCH", "path": user_path, "body": [{"op": "replace", "path": "/attributes/roll_expires_at", "value": "1"}]})).await;
    assert_eq!(moved["status"], 200, "{moved}");
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

/// Cross-cluster search between one person's clusters (docs/search/README.md, "Cross-cluster
/// search"): linked through roll's API with a cross-cluster key of the remote, which roll keeps
/// sealed and never shows; a search, `_msearch`, `_field_caps`, `_resolve/cluster` and
/// `_resolve/index` reach the remote's indices as `alias:index`, within the key's access; another
/// person's cluster can't be named, and answers as a missing one; revoking the key cuts the link.
#[tokio::test]
async fn cross_cluster_search_between_a_persons_own_clusters() {
    use reqwest::Method;
    let e = env().await;
    let mk = |name: &'static str, who: &'static str| {
        let e = &e;
        async move {
            let (s, c) = e.call_as(who, Method::POST, "/v1/search/clusters", Some(json!({ "name": name }))).await;
            assert_eq!(s, 200, "{c}");
            creds(&c)
        }
    };
    let (app, app_pw) = mk("ann-app", "ann").await;
    let (logs, logs_pw) = mk("ann-logs", "ann").await;
    mk("bob-vault", "bob").await;
    for (u, p, path, doc) in [
        (&app, &app_pw, "/app/_doc/1", json!({ "level": "info", "msg": "app up" })),
        (&logs, &logs_pw, "/logs-1/_doc/1", json!({ "level": "error", "msg": "disk full" })),
        (&logs, &logs_pw, "/logs-1/_doc/2", json!({ "level": "info", "msg": "disk ok" })),
        (&logs, &logs_pw, "/private/_doc/1", json!({ "level": "secret" })),
    ] {
        let (s, v) = e.es(u, p, Method::PUT, &format!("{path}?refresh=true"), Some(doc)).await;
        assert_eq!(s, 201, "{v}");
    }

    // Another person's cluster can't be linked, and answers like one that doesn't exist; so does
    // naming it in the settings.
    for target in ["bob-vault", "nope"] {
        let (s, v) = e.call_as("ann", Method::PUT, "/v1/search/clusters/ann-app/remotes/r", Some(json!({ "cluster": target }))).await;
        assert_eq!((s, v["error"].as_str()), (404, Some(&*format!("no search cluster {target}"))), "{v}");
    }
    let refused: Vec<(u16, Value)> = futures::future::join_all(["bob-vault", "nope"].map(|t| {
        e.es(&app, &app_pw, Method::PUT, "/_cluster/settings", Some(json!({ "persistent": { "cluster.remote.r.mode": "proxy", "cluster.remote.r.proxy_address": t } })))
    }))
    .await;
    assert_eq!(refused[0].0, 400, "{:?}", refused[0]);
    assert_eq!(refused[0].1["error"]["reason"].as_str().map(|r| r.replace("bob-vault", "X")), refused[1].1["error"]["reason"].as_str().map(|r| r.replace("nope", "X")));

    // Her own: linked with a key that may search `logs-*` only.
    let (s, link) = e.call_as("ann", Method::PUT, "/v1/search/clusters/ann-app/remotes/logs", Some(json!({ "cluster": "ann-logs", "access": { "search": [{ "names": ["logs-*"] }] } }))).await;
    assert_eq!((s, link["cluster"].as_str(), link["minted"].as_bool()), (200, Some("ann-logs"), Some(true)), "{link}");
    let key_id = link["key_id"].as_str().unwrap().to_string();
    let (_, listed) = e.call_as("ann", Method::GET, "/v1/search/clusters/ann-app/remotes", None).await;
    assert_eq!(listed["remotes"][0]["alias"], "logs", "{listed}");
    assert!(!listed.to_string().contains("encoded") && !link.to_string().contains("api_key"), "credentials never shown: {listed} {link}");
    let (_, info) = e.es(&app, &app_pw, Method::GET, "/_remote/info", None).await;
    assert_eq!((info["logs"]["connected"].as_bool(), info["logs"]["cluster_credentials"].as_str()), (Some(true), Some("::es_redacted::")), "{info}");

    // One search over both: hits named by cluster, aggregations merged, a `_clusters` section.
    let q = json!({ "query": { "match_all": {} }, "aggs": { "levels": { "terms": { "field": "level.keyword" } } }, "sort": [{ "_index": "asc" }] });
    let (s, v) = e.es(&app, &app_pw, Method::POST, "/app,logs:logs-*/_search", Some(q.clone())).await;
    assert_eq!(s, 200, "{v}");
    let indices: Vec<&str> = v["hits"]["hits"].as_array().unwrap().iter().map(|h| h["_index"].as_str().unwrap()).collect();
    assert_eq!(indices, ["app", "logs:logs-1", "logs:logs-1"], "{v}");
    assert_eq!(v["hits"]["total"]["value"], 3, "{v}");
    let buckets: Vec<(String, i64)> = v["aggregations"]["levels"]["buckets"].as_array().unwrap().iter().map(|b| (b["key"].as_str().unwrap().into(), b["doc_count"].as_i64().unwrap())).collect();
    assert_eq!(buckets, [("info".to_string(), 2), ("error".to_string(), 1)], "{v}");
    assert_eq!((v["_clusters"]["total"].as_i64(), v["_clusters"]["successful"].as_i64()), (Some(2), Some(2)), "{v}");
    assert_eq!(v["_clusters"]["details"]["logs"]["indices"], "logs-*", "{v}");
    // `_msearch` items too.
    let r = e.http.post(format!("{}/_msearch", e.es)).basic_auth(&app, Some(&app_pw)).header("content-type", "application/x-ndjson").body("{\"index\":\"logs:logs-1\"}\n{\"query\":{\"match\":{\"msg\":\"disk\"}}}\n").send().await.unwrap();
    let m: Value = r.json().await.unwrap();
    assert_eq!(m["responses"][0]["hits"]["total"]["value"], 2, "{m}");

    // The key's access is all the link reaches: `private` isn't in it (skipped, the default).
    let (s, v) = e.es(&app, &app_pw, Method::GET, "/logs:private/_search", None).await;
    assert_eq!((s, v["_clusters"]["details"]["logs"]["status"].as_str()), (200, Some("skipped")), "{v}");
    assert_eq!(v["_clusters"]["details"]["logs"]["failures"][0]["reason"]["type"], "security_exception", "{v}");
    assert_eq!(v["hits"]["total"]["value"], 0, "{v}");

    // Field capabilities, `_resolve/cluster` and `_resolve/index` across the link.
    let (_, fc) = e.es(&app, &app_pw, Method::GET, "/app,logs:*/_field_caps?fields=level", None).await;
    assert_eq!(fc["indices"], json!(["app", "logs:logs-1"]), "{fc}");
    assert_eq!(fc["fields"]["level"]["text"]["searchable"], true, "{fc}");
    let (_, rc) = e.es(&app, &app_pw, Method::GET, "/_resolve/cluster/app,logs:logs-*", None).await;
    assert_eq!((rc["(local)"]["matching_indices"].as_bool(), rc["logs"]["connected"].as_bool(), rc["logs"]["matching_indices"].as_bool()), (Some(true), Some(true), Some(true)), "{rc}");
    let (_, ri) = e.es(&app, &app_pw, Method::GET, "/_resolve/index/logs:logs-*", None).await;
    assert_eq!(ri["indices"][0]["name"], "logs:logs-1", "{ri}");

    // A user of the local cluster reaches the remote only through `remote_indices` naming it.
    let role = json!({ "indices": [{ "names": ["app"], "privileges": ["read"] }], "remote_indices": [{ "clusters": ["logs"], "names": ["logs-*"], "privileges": ["read"] }] });
    assert_eq!(e.es(&app, &app_pw, Method::PUT, "/_security/role/reader", Some(role)).await.0, 200);
    assert_eq!(e.es(&app, &app_pw, Method::PUT, "/_security/role/local_only", Some(json!({ "indices": [{ "names": ["*"], "privileges": ["read"] }] }))).await.0, 200);
    for (user, role) in [("reader", "reader"), ("local", "local_only")] {
        let u = json!({ "password": "pass-word-1", "roles": [role] });
        assert_eq!(e.es(&app, &app_pw, Method::PUT, &format!("/_security/user/{user}"), Some(u)).await.0, 200);
    }
    let (s, v) = e.es(&format!("reader@{app}"), "pass-word-1", Method::GET, "/app,logs:logs-1/_search", None).await;
    assert_eq!((s, v["hits"]["total"]["value"].as_i64()), (200, Some(3)), "{v}");
    let (s, v) = e.es(&format!("local@{app}"), "pass-word-1", Method::GET, "/logs:logs-1/_search", None).await;
    assert_eq!((s, v["_clusters"]["details"]["logs"]["failures"][0]["reason"]["type"].as_str()), (200, Some("security_exception")), "{v}");

    // A cross-cluster key is no REST credential, also with the header only roll's own calls
    // may carry.
    let (s, k) = e.es(&logs, &logs_pw, Method::POST, "/_security/cross_cluster/api_key", Some(json!({ "name": "mine", "access": { "search": [{ "names": ["*"] }] } }))).await;
    assert_eq!(s, 200, "{k}");
    use base64::Engine;
    let key = base64::engine::general_purpose::STANDARD.encode(format!("{}@{logs}:{}", k["id"].as_str().unwrap(), k["api_key"].as_str().unwrap()));
    let r = e.http.get(format!("{}/logs-1/_search", e.es)).header("Authorization", format!("ApiKey {key}")).header("x-roll-remote-access", r#"{"cluster":"ann-app","roles":[{"x":{"indices":[{"names":["*"],"privileges":["all"]}]}}]}"#).send().await.unwrap();
    assert_eq!(r.status(), 401, "{}", r.text().await.unwrap());

    // Revoking the key on the remote cuts the link: skipped, or an error once it may not be.
    let (s, v) = e.es(&logs, &logs_pw, Method::DELETE, "/_security/api_key", Some(json!({ "ids": [key_id] }))).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = e.es(&app, &app_pw, Method::GET, "/app,logs:logs-1/_search", None).await;
    assert_eq!((v["hits"]["total"]["value"].as_i64(), v["_clusters"]["details"]["logs"]["status"].as_str()), (Some(1), Some("skipped")), "{v}");
    assert_eq!(e.es(&app, &app_pw, Method::PUT, "/_cluster/settings", Some(json!({ "persistent": { "cluster.remote.logs.skip_unavailable": false } }))).await.0, 200);
    let (s, v) = e.es(&app, &app_pw, Method::GET, "/app,logs:logs-1/_search", None).await;
    assert_eq!((s, v["error"]["type"].as_str()), (401, Some("security_exception")), "{v}");

    // Unlinked: the remote's settings and credentials go.
    assert_eq!(e.call_as("ann", Method::DELETE, "/v1/search/clusters/ann-app/remotes/logs", None).await.0, 204);
    let (_, info) = e.es(&app, &app_pw, Method::GET, "/_remote/info", None).await;
    assert!(info.get("logs").is_none(), "{info}");
}

/// A linked remote that sleeps wakes for the search; one that's gone is skipped (the default),
/// or fails the search when it may not be.
#[tokio::test]
async fn a_sleeping_remote_wakes_and_a_gone_one_is_skipped() {
    use reqwest::Method;
    let e = env().await;
    let (near, near_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "near" })).await);
    let (far, far_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "far" })).await);
    let (s, v) = e.es(&far, &far_pw, Method::PUT, "/items/_doc/1?refresh=true", Some(json!({ "n": 1 }))).await;
    assert_eq!(s, 201, "{v}");
    let (s, v) = e.call(Method::PUT, "/v1/search/clusters/near/remotes/far", Some(json!({ "cluster": "far" }))).await;
    assert_eq!(s, 200, "{v}");
    e.post("/v1/search/clusters/far/unload", json!({})).await;
    assert!(!e.control.node.loaded().iter().any(|n| &**n == "far"), "asleep");
    let (s, v) = e.es(&near, &near_pw, Method::GET, "/far:items/_search", None).await;
    assert_eq!((s, v["hits"]["total"]["value"].as_i64()), (200, Some(1)), "{v}");
    assert!(e.control.node.loaded().iter().any(|n| &**n == "far"), "woken for it");

    assert_eq!(e.call(Method::DELETE, "/v1/search/clusters/far", None).await.0, 200);
    let (s, v) = e.es(&near, &near_pw, Method::GET, "/far:items/_search", None).await;
    assert_eq!((s, v["_clusters"]["skipped"].as_i64(), v["_clusters"]["details"]["far"]["failures"][0]["reason"]["type"].as_str()), (200, Some(1), Some("connect_transport_exception")), "{v}");
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_cluster/settings", Some(json!({ "persistent": { "cluster.remote.far.skip_unavailable": false } }))).await.0, 200);
    let (s, v) = e.es(&near, &near_pw, Method::GET, "/far:items/_search", None).await;
    assert_eq!((s, v["error"]["type"].as_str()), (500, Some("connect_transport_exception")), "{v}");
}


/// Cross-cluster tests' data: `items` documents `0..n`, with values whose sums are exact in
/// binary, so every reduce order gives the same numbers.
fn ccs_doc(i: i64) -> Value {
    let tag = ["a", "b", "c", "d"][(i % 4) as usize];
    json!({ "n": i, "price": i as f64 * 1.5, "w": 1 + i % 4, "tag": tag, "at": 1_704_067_200_000i64 + i * 86_400_000 * 3, "loc": { "lat": (i % 40) as f64 - 20.0, "lon": (i % 90) as f64 * 2.0 - 90.0 } })
}

/// Creates `items` on a cluster and indexes the documents `ids` names.
async fn ccs_items(e: &Env, user: &str, pw: &str, ids: impl Iterator<Item = i64>) {
    let mapping = json!({ "settings": { "number_of_shards": 2 }, "mappings": { "properties": { "n": { "type": "long" }, "price": { "type": "double" }, "w": { "type": "long" }, "tag": { "type": "keyword" }, "at": { "type": "date" }, "loc": { "type": "geo_point" } } } });
    let (s, v) = e.es(user, pw, reqwest::Method::PUT, "/items", Some(mapping)).await;
    assert_eq!(s, 200, "{v}");
    let mut bulk = String::new();
    for i in ids {
        bulk.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{}\n", ccs_doc(i)));
    }
    let r = e.http.post(format!("{}/items/_bulk?refresh=true", e.es)).basic_auth(user, Some(pw)).header("content-type", "application/x-ndjson").body(bulk).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["errors"], false, "{v}");
}

/// A search's answer with remote index names (`far:items`) as local ones, for comparing a
/// cross-cluster answer with one cluster's.
fn ccs_local_names(v: &Value) -> Value {
    serde_json::from_str(&v.to_string().replace("\"far:items\"", "\"items\"")).unwrap()
}

/// Aggregations across linked clusters reduce as one cluster's do (Elasticsearch's
/// `ccs_minimize_roundtrips`): each cluster reduces its part partially and the coordinating one
/// does the final reduce from their internal state (sketches, digests, sums and counts, top
/// hits), so `avg`, `cardinality`, percentiles, `top_hits`, geo and pipeline aggregations over
/// an index split across two clusters equal one cluster holding all of it.
#[tokio::test]
async fn aggregations_across_clusters_equal_one_clusters() {
    use reqwest::Method;
    let e = env().await;
    let (all, all_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "agg-all" })).await);
    let (near, near_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "agg-near" })).await);
    let (far, far_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "agg-far" })).await);
    ccs_items(&e, &all, &all_pw, 0..90).await;
    ccs_items(&e, &near, &near_pw, (0..90).filter(|i| i % 3 != 0)).await;
    ccs_items(&e, &far, &far_pw, (0..90).filter(|i| i % 3 == 0)).await;
    let (s, v) = e.call(Method::PUT, "/v1/search/clusters/agg-near/remotes/far", Some(json!({ "cluster": "agg-far" }))).await;
    assert_eq!(s, 200, "{v}");
    let aggs = json!({
        "avg_n": { "avg": { "field": "n" } },
        "tags": { "cardinality": { "field": "tag" } },
        "pct": { "percentiles": { "field": "price", "percents": [1, 25, 50, 75, 99] } },
        "ranks": { "percentile_ranks": { "field": "price", "values": [10, 60.5, 120] } },
        "hdr": { "percentiles": { "field": "n", "hdr": { "number_of_significant_value_digits": 3 } } },
        "mad": { "median_absolute_deviation": { "field": "n" } },
        "ext": { "extended_stats": { "field": "price" } },
        "wavg": { "weighted_avg": { "value": { "field": "n" }, "weight": { "field": "w" } } },
        "top": { "top_hits": { "size": 4, "sort": [{ "n": "desc" }], "_source": ["n", "tag"] } },
        "last": { "top_metrics": { "metrics": [{ "field": "price" }, { "field": "w" }], "sort": { "n": "desc" }, "size": 2 } },
        "box": { "geo_bounds": { "field": "loc" } },
        "mid": { "geo_centroid": { "field": "loc" } },
        "by_tag": { "terms": { "field": "tag", "order": { "avg_n": "desc" } }, "aggs": {
            "avg_n": { "avg": { "field": "n" } },
            "best": { "top_hits": { "size": 1, "sort": [{ "price": "desc" }], "_source": ["n"] } },
            "kinds": { "cardinality": { "field": "w" } }
        } },
        "best_tag": { "max_bucket": { "buckets_path": "by_tag>avg_n" } },
        "monthly": { "date_histogram": { "field": "at", "calendar_interval": "month" }, "aggs": {
            "sum_n": { "sum": { "field": "n" } },
            "change": { "derivative": { "buckets_path": "sum_n" } },
            "running": { "cumulative_sum": { "buckets_path": "sum_n" } },
            "avg_p": { "avg": { "field": "price" } }
        } },
        "monthly_avg": { "avg_bucket": { "buckets_path": "monthly>avg_p" } },
        "stats_tags": { "stats_bucket": { "buckets_path": "by_tag>_count" } }
    });
    for typed in [false, true] {
        let q = if typed { "?typed_keys=true" } else { "" };
        let body = json!({ "size": 0, "aggs": aggs });
        let (s, one) = e.es(&all, &all_pw, Method::POST, &format!("/items/_search{q}"), Some(body.clone())).await;
        assert_eq!(s, 200, "{one}");
        let (s, two) = e.es(&near, &near_pw, Method::POST, &format!("/items,far:items/_search{q}"), Some(body.clone())).await;
        assert_eq!(s, 200, "{two}");
        assert_eq!((two["_clusters"]["successful"].as_i64(), two["hits"]["total"]["value"].as_i64()), (Some(2), Some(90)), "{two}");
        // A centroid's sums add up in another order (Elasticsearch's own differ in the last
        // digit the same way): equal within rounding; everything else exactly.
        let (mut two_aggs, mut one_aggs) = (ccs_local_names(&two["aggregations"]), one["aggregations"].clone());
        let mid = if typed { "geo_centroid#mid" } else { "mid" };
        let (a, b) = (two_aggs.as_object_mut().unwrap().remove(mid).unwrap(), one_aggs.as_object_mut().unwrap().remove(mid).unwrap());
        assert_eq!(a["count"], b["count"]);
        for k in ["lat", "lon"] {
            let (x, y) = (a["location"][k].as_f64().unwrap(), b["location"][k].as_f64().unwrap());
            assert!((x - y).abs() <= 1e-12 * y.abs().max(1.0), "{mid}.{k}: {x} vs {y}");
        }
        assert_eq!(two_aggs, one_aggs, "typed_keys {typed}: across clusters\n{two_aggs}\none cluster\n{one_aggs}");
        // Only the remote's part: the same reduce.
        let (s, remote) = e.es(&near, &near_pw, Method::POST, &format!("/far:items/_search{q}"), Some(body)).await;
        let (_, alone) = e.es(&far, &far_pw, Method::POST, &format!("/items/_search{q}"), Some(json!({ "size": 0, "aggs": aggs }))).await;
        assert_eq!((s, ccs_local_names(&remote["aggregations"])), (200, alone["aggregations"].clone()), "{remote}");
    }
    // The partial state is the link's only: a client asking for it gets the public answer.
    let r = e.http.post(format!("{}/items/_search", e.es)).basic_auth(&far, Some(&far_pw)).header("x-roll-partial-aggs", "1").json(&json!({ "size": 0, "aggs": { "a": { "avg": { "field": "n" } } } })).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert!(v.get("__roll_partial_aggs").is_none() && v["aggregations"]["a"]["value"].is_number(), "{v}");
}

/// An ES|QL query's answer, for comparing clusters: its columns and values, remote index names
/// (`far:items`) as local ones.
fn esql_answer(v: &Value) -> Value {
    ccs_local_names(&json!({ "columns": v["columns"], "values": v["values"] }))
}

/// Creates enrich policy `name` (`tag` to `label`) over a cluster's `tags`, labelled `prefix-tag`.
async fn ccs_policy(e: &Env, user: &str, pw: &str, name: &str, prefix: &str) {
    use reqwest::Method;
    for t in ["a", "b", "c", "d"] {
        let (s, v) = e.es(user, pw, Method::PUT, &format!("/tags/_doc/{t}?refresh=true"), Some(json!({ "tag": t, "label": format!("{prefix}-{t}") }))).await;
        assert!(s < 300, "{v}");
    }
    let (s, v) = e.es(user, pw, Method::PUT, &format!("/_enrich/policy/{name}"), Some(json!({ "match": { "indices": "tags", "match_field": "tag", "enrich_fields": ["label"] } }))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(user, pw, Method::POST, &format!("/_enrich/policy/{name}/_execute"), None).await;
    assert_eq!(s, 200, "{v}");
}

/// ES|QL over linked clusters (`FROM items,far:items`) answers what one cluster holding all the
/// data answers, as Elasticsearch 9.5.5 does: each cluster runs the query up to its first
/// breaker and `STATS` states merge exactly; `_index` names remote indices `far:items`;
/// `_clusters` with `include_ccs_metadata` (`_query/async` too); a missing remote is a 404 and
/// a missing remote index skips that cluster (`skip_unavailable`) or fails the query; `ENRICH`
/// runs with each cluster's policy in `_remote` mode, the coordinator's in `_coordinator` mode,
/// and where it lands in `_any` mode, refused after `STATS` and when a cluster lacks the policy;
/// the query's own errors and a remote's views are errors, never a skipped remote; and the
/// remote reduces `STATS` itself (its rows wouldn't fit the breaker its answer is held on).
#[tokio::test]
async fn esql_across_clusters_answers_as_one_cluster() {
    use reqwest::Method;
    let e = env().await;
    let (all, all_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "esql-all" })).await);
    let (near, near_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "esql-near" })).await);
    let (far, far_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "esql-far" })).await);
    ccs_items(&e, &all, &all_pw, 0..90).await;
    ccs_items(&e, &near, &near_pw, (0..90).filter(|i| i % 3 != 0)).await;
    ccs_items(&e, &far, &far_pw, (0..90).filter(|i| i % 3 == 0)).await;
    let bulk: String = (0..3000).map(|i| format!("{{\"index\":{{}}}}\n{{\"k\":{}}}\n", i % 7)).collect();
    let r = e.http.post(format!("{}/many/_bulk?refresh=true", e.es)).basic_auth(&far, Some(&far_pw)).header("content-type", "application/x-ndjson").body(bulk).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["errors"], false);
    let (s, v) = e.call(Method::PUT, "/v1/search/clusters/esql-near/remotes/far", Some(json!({ "cluster": "esql-far" }))).await;
    assert_eq!(s, 200, "{v}");
    let query = |user: &str, pw: &str, body: Value| {
        let (user, pw) = (user.to_string(), pw.to_string());
        let e = &e;
        async move { e.es(&user, &pw, Method::POST, "/_query", Some(body)).await }
    };

    // Partial `STATS`, `SORT | LIMIT`, `LIMIT` and the commands after the breaker: one cluster's
    // answers.
    for tail in [
        "| STATS c = COUNT(*), s = SUM(n), a = AVG(price), cd = COUNT_DISTINCT(w), lo = MIN(at), hi = MAX(price), med = MEDIAN(n), p90 = PERCENTILE(price, 90), ws = MV_SORT(VALUES(w)) BY tag | SORT tag",
        "| STATS c = COUNT(*), s = SUM(price) | EVAL mean = s / c",
        "| WHERE n > 40 | STATS c = COUNT(*) BY b = BUCKET(at, 1 month) | SORT b",
        "| EVAL x = n % 5 | STATS c = COUNT(*) BY x, tag | SORT x, tag | LIMIT 5",
        "| SORT price DESC, n | LIMIT 7 | KEEP n, price, tag",
        "| WHERE tag == \"b\" | EVAL d = n * 2 | SORT d | LIMIT 4 | KEEP d, tag",
        "| INLINE STATS m = MAX(n) BY tag | WHERE n == m | KEEP n, tag | SORT n",
    ] {
        let (s, one) = query(&all, &all_pw, json!({ "query": format!("FROM items {tail}") })).await;
        assert_eq!(s, 200, "{one}");
        let (s, two) = query(&near, &near_pw, json!({ "query": format!("FROM items,far:items {tail}") })).await;
        assert_eq!(s, 200, "{tail}: {two}");
        assert_eq!(esql_answer(&two), esql_answer(&one), "{tail}");
        assert!(two.get("_clusters").is_none(), "only with include_ccs_metadata: {two}");
    }
    // `*:items` is every remote's, not the local cluster's.
    let (_, only) = query(&near, &near_pw, json!({ "query": "FROM *:items | STATS c = COUNT(*), s = SUM(n)" })).await;
    let (_, one) = query(&all, &all_pw, json!({ "query": "FROM items | WHERE n % 3 == 0 | STATS c = COUNT(*), s = SUM(n)" })).await;
    assert_eq!(esql_answer(&only), esql_answer(&one), "{only}");
    // An exclusion of a remote's is that remote's own (`-many` there).
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM far:*,-far:many | STATS c = COUNT(*)" })).await;
    assert_eq!((s, v["values"].clone()), (200, json!([[30]])), "{v}");

    // `_index` names the cluster; `_clusters` reports every cluster's part.
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM items,far:items METADATA _index | WHERE n < 4 | KEEP n, _index | SORT n", "include_ccs_metadata": true })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["values"], json!([[0, "far:items"], [1, "items"], [2, "items"], [3, "far:items"]]), "{v}");
    let c = &v["_clusters"];
    assert_eq!((c["total"].as_i64(), c["successful"].as_i64(), c["skipped"].as_i64(), v["is_partial"].as_bool()), (Some(2), Some(2), Some(0), Some(false)), "{v}");
    for name in ["(local)", "far"] {
        let d = &c["details"][name];
        assert_eq!((d["status"].as_str(), d["indices"].as_str(), d["_shards"]["total"].as_i64(), d["_shards"]["successful"].as_i64()), (Some("successful"), Some("items"), Some(2), Some(2)), "{name}: {v}");
    }
    // `_query/async` answers the same.
    let (s, a) = e.es(&near, &near_pw, Method::POST, "/_query/async", Some(json!({ "query": "FROM items,far:items | STATS c = COUNT(*)", "include_ccs_metadata": true, "keep_on_completion": true }))).await;
    assert_eq!((s, a["values"].clone(), a["_clusters"]["successful"].as_i64()), (200, json!([[90]]), Some(2)), "{a}");
    let (_, got) = e.es(&near, &near_pw, Method::GET, &format!("/_query/async/{}", a["id"].as_str().unwrap()), None).await;
    assert_eq!((got["values"].clone(), got["_clusters"]["total"].as_i64()), (json!([[90]]), Some(2)), "{got}");

    // A remote that isn't configured is a 404; a remote's missing index skips it (the default)
    // or fails the query.
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM nope:items | LIMIT 1" })).await;
    assert_eq!((s, v["error"]["type"].as_str(), v["error"]["reason"].as_str()), (404, Some("no_such_remote_cluster_exception"), Some("no such remote cluster: [nope]")), "{v}");
    // A quoted `METADATA` name is no metadata field, whatever it holds (here the text of a
    // command): the query's error, before any cluster runs anything.
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM far:items METADATA `_index | ENRICH _remote:tag_label ON tag WITH tag = label` | LIMIT 5" })).await;
    assert!(s == 400 && v["error"]["reason"].as_str().is_some_and(|r| r.starts_with("Found 2 problems\nline 1:1: unresolved metadata fields: [?`_index |")), "{v}");
    // The query's own error is the query's, never a skipped remote.
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM far:items | WHERE nosuch > 1" })).await;
    assert_eq!((s, v["error"]["reason"].as_str()), (400, Some("Found 1 problem\nline 1:24: Unknown column [nosuch]")), "{v}");
    // A remote's ES|QL view isn't read across clusters (Elasticsearch's 400).
    let (s, v) = e.es(&far, &far_pw, Method::PUT, "/_query/view/itemsview", Some(json!({ "query": "FROM items" }))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM far:itemsview | STATS c = COUNT(*)" })).await;
    assert_eq!((s, v["error"]["type"].as_str(), v["error"]["reason"].as_str()), (400, Some("remote_view_not_supported_exception"), Some("ES|QL queries with remote views are not supported. Matched [far:itemsview]. Remove them from the query pattern or exclude them with [far:-itemsview] if matched by a wildcard.")), "{v}");
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM items,far:missing | STATS c = COUNT(*)", "include_ccs_metadata": true })).await;
    assert_eq!((s, v["values"].clone(), v["is_partial"].as_bool()), (200, json!([[60]]), Some(true)), "{v}");
    let far_part = &v["_clusters"]["details"]["far"];
    assert_eq!((far_part["status"].as_str(), far_part["failures"][0]["reason"]["type"].as_str(), far_part["failures"][0]["reason"]["reason"].as_str()), (Some("skipped"), Some("verification_exception"), Some("Unknown index [far:missing]")), "{v}");
    // With that remote skipped, another one mapping fields this cluster doesn't still merges:
    // each part reduces over just the columns the STATS reads.
    let (s, v) = e.es(&all, &all_pw, Method::PUT, "/logs/_doc/1?refresh=true", Some(json!({ "msg": "a", "code": 1 }))).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(e.call(Method::PUT, "/v1/search/clusters/esql-near/remotes/far2", Some(json!({ "cluster": "esql-all" }))).await.0, 200);
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM items,far:missing,far2:logs | STATS c = COUNT(*), m = MAX(code)" })).await;
    assert_eq!((s, v["values"].clone()), (200, json!([[61, 1]])), "only far2 maps `code`: {v}");
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_cluster/settings", Some(json!({ "persistent": { "cluster.remote.far.skip_unavailable": false } }))).await.0, 200);
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM items,far:missing | LIMIT 1" })).await;
    assert_eq!((s, v["error"]["reason"].as_str()), (400, Some("Unknown index [far:missing]")), "{v}");

    // ENRICH: each cluster's own policy (`_remote`, `_any` before a breaker), the coordinator's
    // (`_coordinator`, `_any` after one).
    ccs_policy(&e, &near, &near_pw, "tag_label", "N").await;
    ccs_policy(&e, &far, &far_pw, "tag_label", "F").await;
    ccs_policy(&e, &near, &near_pw, "only_near", "N").await;
    for (q, want) in [
        ("FROM items,far:items | WHERE n < 2 | ENRICH _remote:tag_label | KEEP n, label | SORT n", json!([[0, "F-a"], [1, "N-b"]])),
        ("FROM items,far:items | WHERE n < 2 | ENRICH tag_label | KEEP n, label | SORT n", json!([[0, "F-a"], [1, "N-b"]])),
        ("FROM items,far:items | WHERE n < 2 | ENRICH _coordinator:tag_label | KEEP n, label | SORT n", json!([[0, "N-a"], [1, "N-b"]])),
        ("FROM items,far:items | SORT n | LIMIT 2 | ENRICH _remote:tag_label | KEEP n, label", json!([[0, "F-a"], [1, "N-b"]])),
        ("FROM items,far:items | SORT n | LIMIT 2 | ENRICH tag_label | KEEP n, label", json!([[0, "N-a"], [1, "N-b"]])),
        // Sorted on the parts' `n`, which the policy then overwrites.
        ("FROM items,far:items | WHERE n > 0 | SORT n | LIMIT 3 | ENRICH _remote:tag_label WITH n = label | KEEP n", json!([["N-b"], ["N-c"], ["F-d"]])),
        // Each cluster's policy also after the commands between `SORT` and `LIMIT`.
        ("FROM items,far:items | SORT n | KEEP n, tag | LIMIT 3 | ENRICH _remote:tag_label | KEEP n, label", json!([[0, "F-a"], [1, "N-b"], [2, "N-c"]])),
        // After a `FORK` (whose remote rows are read here), `_any` is this cluster's policy.
        ("FROM far:items | FORK (WHERE n < 4) (WHERE n > 86) | ENRICH tag_label | KEEP n, label | SORT n", json!([[0, "N-a"], [3, "N-d"], [87, "N-d"]])),
    ] {
        let (s, v) = query(&near, &near_pw, json!({ "query": q })).await;
        assert_eq!((s, v["values"].clone()), (200, want), "{q}: {v}");
    }
    for (q, reason) in [
        ("FROM items,far:items | STATS c=COUNT(*) BY tag | ENRICH _remote:tag_label | SORT tag", "Found 1 problem\nline 1:50: ENRICH with remote policy can't be executed after [STATS c=COUNT(*) BY tag]@1:24"),
        ("FROM items,far:items | ENRICH only_near | LIMIT 1", "Found 1 problem\nline 1:31: cannot find enrich policy [only_near] on clusters [far]"),
        // Never this cluster's lookup index or policy where Elasticsearch reads each cluster's.
        ("FROM far:items | ENRICH _coordinator:tag_label | LOOKUP JOIN lk ON tag", "Found 1 problem\nline 1:50: LOOKUP JOIN with remote indices can't be executed after [ENRICH _coordinator:tag_label]@1:18"),
        ("FROM far:items | INLINE STATS m = MAX(n) | ENRICH _remote:tag_label | KEEP n, m, label", "Found 1 problem\nline 1:44: ENRICH with remote policy can't be executed after [INLINE STATS m = MAX(n)]@1:18"),
        // A remote missing the policy its `_any` use needs, reported there, not at this cluster's
        // `_coordinator` use of the same name.
        ("FROM items,far:items | ENRICH _coordinator:only_near | ENRICH only_near", "Found 1 problem\nline 1:63: cannot find enrich policy [only_near] on clusters [far]"),
        // Shapes not cut into parts read the remotes' rows here (FORK, subqueries, `SET
        // unmapped_fields`): Elasticsearch's placement errors, FORK branches included...
        ("FROM far:items | STATS c = COUNT(*) BY tag | ENRICH _remote:tag_label | FORK (WHERE c > 0) (WHERE c <= 0)", "Found 1 problem\nline 1:46: ENRICH with remote policy can't be executed after [STATS c = COUNT(*) BY tag]@1:18"),
        ("FROM far:items | FORK (STATS c = COUNT(*) BY tag | ENRICH _remote:tag_label) (WHERE n <= 0)", "Found 1 problem\nline 1:52: ENRICH with remote policy can't be executed after [STATS c = COUNT(*) BY tag]@1:24"),
    ] {
        let (s, v) = query(&near, &near_pw, json!({ "query": q })).await;
        assert_eq!((s, v["error"]["reason"].as_str()), (400, Some(reason)), "{q}: {v}");
    }
    // ...and a refusal where Elasticsearch would read each cluster's own policy or lookup index.
    let why = "roll reads the remote clusters' rows here in a query with FORK, subqueries, views, TS or SET unmapped_fields";
    for (q, reason) in [
        ("FROM far:items | LOOKUP JOIN lk ON tag | FORK (WHERE n > 0) (WHERE n <= 0)", format!("Found 1 problem\nline 1:30: LOOKUP JOIN can't use each cluster's own lookup index: {why}")),
        ("SET unmapped_fields=\"nullify\"; FROM far:items | SORT n | LIMIT 5 | ENRICH _remote:tag_label", format!("Found 1 problem\nline 1:68: ENRICH [_remote:tag_label] can't use each cluster's own policy: {why}")),
        ("FROM (FROM far:items | ENRICH _remote:tag_label)", format!("Found 1 problem\nline 1:24: ENRICH [_remote:tag_label] can't use each cluster's own policy: {why}")),
    ] {
        let (s, v) = query(&near, &near_pw, json!({ "query": q })).await;
        assert_eq!((s, v["error"]["reason"].as_str()), (400, Some(reason.as_str())), "{q}: {v}");
    }

    // Clusters' own resources may differ: a policy's enrich fields must be the same everywhere
    // (Elasticsearch's 400), a lookup index's mappings needn't be (a union type).
    for (user, pw, label, fields) in [(&near, &near_pw, "N", json!(["label"])), (&far, &far_pw, "F", json!(["label", "extra"]))] {
        let (s, v) = e.es(user, pw, Method::PUT, "/tags2/_doc/a?refresh=true", Some(json!({ "tag": "a", "label": format!("{label}-a"), "extra": "x" }))).await;
        assert!(s < 300, "{v}");
        assert_eq!(e.es(user, pw, Method::PUT, "/_enrich/policy/wide", Some(json!({ "match": { "indices": "tags2", "match_field": "tag", "enrich_fields": fields } }))).await.0, 200);
        assert_eq!(e.es(user, pw, Method::POST, "/_enrich/policy/wide/_execute", None).await.0, 200);
        let ty = if label == "N" { "keyword" } else { "text" };
        let (s, v) = e.es(user, pw, Method::PUT, "/lk", Some(json!({ "settings": { "index.mode": "lookup" }, "mappings": { "properties": { "tag": { "type": "keyword" }, "lab": { "type": ty } } } }))).await;
        assert_eq!(s, 200, "{v}");
        assert!(e.es(user, pw, Method::PUT, "/lk/_doc/a?refresh=true", Some(json!({ "tag": "a", "lab": label }))).await.0 < 300);
    }
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM items,far:items | ENRICH wide | LIMIT 1" })).await;
    assert_eq!((s, v["error"]["reason"].as_str()), (400, Some("Found 1 problem\nline 1:31: enrich policy [wide] has different enrich fields across clusters; these fields are missing in some policies: [extra]")), "{v}");
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM items,far:items | LOOKUP JOIN lk ON tag | KEEP n, lab | SORT n | LIMIT 2" })).await;
    assert_eq!((s, v["columns"][1].clone(), v["values"].clone()), (200, json!({ "name": "lab", "type": "unsupported", "original_types": ["keyword", "text"], "suggested_cast": "keyword" }), json!([[0, null], [1, null]])), "{v}");
    // Using such a field is Elasticsearch's 400: a lookup field's before any part runs, an
    // enrich field's (its policy index mapped its own way per cluster) once the parts answer.
    for (user, pw, label) in [(&near, &near_pw, "N"), (&far, &far_pw, "F")] {
        let ty = if label == "N" { "keyword" } else { "text" };
        assert_eq!(e.es(user, pw, Method::PUT, "/tags3", Some(json!({ "mappings": { "properties": { "tag": { "type": "keyword" }, "label": { "type": ty } } } }))).await.0, 200);
        assert!(e.es(user, pw, Method::PUT, "/tags3/_doc/a?refresh=true", Some(json!({ "tag": "a", "label": format!("{label}-a") }))).await.0 < 300);
        assert_eq!(e.es(user, pw, Method::PUT, "/_enrich/policy/typed", Some(json!({ "match": { "indices": "tags3", "match_field": "tag", "enrich_fields": ["label"] } }))).await.0, 200);
        assert_eq!(e.es(user, pw, Method::POST, "/_enrich/policy/typed/_execute", None).await.0, 200);
    }
    for (q, reason) in [
        ("FROM items,far:items | LOOKUP JOIN lk ON tag | SORT lab | LIMIT 2", "Found 1 problem\nline 1:53: Cannot use field [lab] due to ambiguities being mapped as [2] incompatible types: [keyword] in [lk], [text] in [far:lk]"),
        ("FROM items,far:items | ENRICH typed | SORT label | LIMIT 2", "Found 1 problem\nline 1:44: Cannot use field [label] due to ambiguities being mapped as [2] incompatible types: [keyword] in [typed], [text] in [far:typed]"),
    ] {
        let (s, v) = query(&near, &near_pw, json!({ "query": q })).await;
        assert_eq!((s, v["error"]["reason"].as_str()), (400, Some(reason)), "{q}: {v}");
    }

    // The implicit limit runs in the parts wherever it commutes: 1,000 of the remote's 3,000
    // rows, read there, for the ENRICH that runs here.
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM far:many | EVAL tag = \"a\" | ENRICH _coordinator:tag_label | KEEP k, label" })).await;
    assert_eq!((s, v["values"].as_array().map(Vec::len), v["values"][0][1].as_str()), (200, Some(1000), Some("N-a")), "{}", v["error"]);
    assert!(v["documents_found"].as_i64().is_some_and(|d| d <= 1000), "{}", v["documents_found"]);

    // `STATS` merges each cluster's partial states: 3,000 remote rows wouldn't fit the request
    // breaker this cluster holds remote answers on, the remote's states do.
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_cluster/settings", Some(json!({ "transient": { "indices.breaker.request.limit": "100kb" } }))).await.0, 200);
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM far:many | STATS c = COUNT(*), s = SUM(k), d = COUNT_DISTINCT(k)" })).await;
    assert_eq!((s, v["values"].clone()), (200, json!([[3000, 8994, 7]])), "{v}");
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM far:many | KEEP k | LIMIT 3000" })).await;
    assert_eq!((s, v["error"]["type"].as_str()), (429, Some("circuit_breaking_exception")), "{v}");
    // Parts nothing bounds share what the breaker has left: one remote's 429 rows fit, two
    // remotes' don't, though each sends what one sent alone.
    let rows = search_es::xpack::esql::ccs::row_bytes(&[search_es::xpack::esql::value::V::Long(0)]) * 429;
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_cluster/settings", Some(json!({ "transient": { "indices.breaker.request.limit": format!("{}b", rows * 3 / 2) } }))).await.0, 200);
    assert_eq!(e.call(Method::PUT, "/v1/search/clusters/esql-near/remotes/far3", Some(json!({ "cluster": "esql-far" }))).await.0, 200);
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM far:many | WHERE k == 0 | INLINE STATS m = MAX(k) | STATS c = COUNT(*)" })).await;
    assert_eq!((s, v["values"].clone()), (200, json!([[429]])), "{v}");
    let (s, v) = query(&near, &near_pw, json!({ "query": "FROM far:many,far3:many | WHERE k == 0 | INLINE STATS m = MAX(k) | STATS c = COUNT(*)" })).await;
    let reason = v["error"]["reason"].as_str().unwrap_or_default();
    assert!(s == 429 && reason.contains("this cluster's part of the query would answer more than"), "a remote stopped at its share: {v}");
}

/// ES|QL's part on a linked remote reads as the link allows, as `_search` across clusters does:
/// the `cross_cluster` key's access (its document and field level security included)
/// intersected with the caller's `remote_indices` (a field the key hides reads null); a caller
/// without them for the remote is refused (403), a remote's `ENRICH` needs `monitor_enrich` there
/// (`remote_cluster`), and a client can't ask a cluster for a part's internal answer.
#[tokio::test]
async fn esql_across_clusters_reads_as_the_link_allows() {
    use reqwest::Method;
    let e = env().await;
    let (near, near_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "esql-sec-near" })).await);
    let (far, far_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "esql-sec-far" })).await);
    ccs_items(&e, &far, &far_pw, 0..20).await;
    let (s, v) = e.es(&far, &far_pw, Method::PUT, "/secret/_doc/1?refresh=true", Some(json!({ "n": 1 }))).await;
    assert_eq!(s, 201, "{v}");
    let access = json!({ "search": [{ "names": ["items"], "query": { "term": { "tag": "a" } }, "field_security": { "grant": ["n", "tag"] } }] });
    let (s, v) = e.call(Method::PUT, "/v1/search/clusters/esql-sec-near/remotes/far", Some(json!({ "cluster": "esql-sec-far", "access": access }))).await;
    assert_eq!(s, 200, "{v}");

    // Only the key's documents (tag `a`: 0, 4, 8, 12, 16) and fields.
    let (s, v) = e.es(&near, &near_pw, Method::POST, "/_query", Some(json!({ "query": "FROM far:items | STATS c = COUNT(*), s = SUM(n)" }))).await;
    assert_eq!((s, v["values"].clone()), (200, json!([[5, 40]])), "{v}");
    let (s, v) = e.es(&near, &near_pw, Method::POST, "/_query", Some(json!({ "query": "FROM far:items | SORT n | LIMIT 2" }))).await;
    let names: Vec<&str> = v["columns"].as_array().map(|c| c.iter().filter_map(|c| c["name"].as_str()).collect()).unwrap_or_default();
    assert_eq!((s, names, v["values"].clone()), (200, vec!["n", "tag"], json!([[0, "a"], [4, "a"]])), "{v}");
    // A field this cluster maps that the key hides on the remote reads null there.
    let (s, v) = e.es(&near, &near_pw, Method::PUT, "/items/_doc/1?refresh=true", Some(json!({ "n": 100, "price": 7.5, "tag": "a" }))).await;
    assert_eq!(s, 201, "{v}");
    let (s, v) = e.es(&near, &near_pw, Method::POST, "/_query", Some(json!({ "query": "FROM items,far:items | STATS p = MAX(price), c = COUNT(*)" }))).await;
    assert_eq!((s, v["values"].clone()), (200, json!([[7.5, 6]])), "{v}");
    let (s, v) = e.es(&near, &near_pw, Method::POST, "/_query", Some(json!({ "query": "FROM far:secret | LIMIT 1", "include_ccs_metadata": true }))).await;
    assert_eq!((s, v["_clusters"]["details"]["far"]["status"].as_str()), (200, Some("skipped")), "{v}");

    // A user reaches the remote only through `remote_indices` naming it.
    let reader = json!({ "indices": [{ "names": ["*"], "privileges": ["read"] }], "remote_indices": [{ "clusters": ["far"], "names": ["items"], "privileges": ["read"] }] });
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_security/role/reader", Some(reader)).await.0, 200);
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_security/role/local_only", Some(json!({ "indices": [{ "names": ["*"], "privileges": ["read"] }] }))).await.0, 200);
    for (user, role) in [("reader", "reader"), ("local", "local_only")] {
        assert_eq!(e.es(&near, &near_pw, Method::PUT, &format!("/_security/user/{user}"), Some(json!({ "password": "pass-word-1", "roles": [role] }))).await.0, 200);
    }
    let body = json!({ "query": "FROM far:items | STATS c = COUNT(*)", "include_ccs_metadata": true });
    let (s, v) = e.es(&format!("reader@{near}"), "pass-word-1", Method::POST, "/_query", Some(body.clone())).await;
    assert_eq!((s, v["values"].clone()), (200, json!([[5]])), "{v}");
    let (s, v) = e.es(&format!("local@{near}"), "pass-word-1", Method::POST, "/_query", Some(body)).await;
    let reason = v["error"]["reason"].as_str().unwrap_or_default();
    assert!(s == 403 && reason.starts_with("action [indices:data/read/esql/resolve_fields] towards remote cluster [far] is unauthorized for user [local]"), "{v}");

    // `remote_cluster` grants only Elasticsearch's remote cluster privileges.
    let (s, v) = e.es(&near, &near_pw, Method::PUT, "/_security/role/bad", Some(json!({ "remote_cluster": [{ "clusters": ["far"], "privileges": ["all"] }] }))).await;
    assert_eq!((s, v["error"]["reason"].as_str()), (400, Some("failed to parse remote_cluster for role [bad]. [monitor_enrich, monitor_stats] are the only values allowed for [privileges] within [remote_cluster]. Found [all]")), "{v}");
    let (s, v) = e.es(&near, &near_pw, Method::PUT, "/_security/role/bad", Some(json!({ "remote_cluster": [{ "clusters": ["far"] }] }))).await;
    assert_eq!((s, v["error"]["reason"].as_str()), (400, Some("failed to parse remote_cluster for role [bad]. [privileges] must be defined when [clusters] are defined ")), "{v}");
    // A remote's ENRICH needs `monitor_enrich` there (`remote_cluster`), whatever the key's own.
    ccs_policy(&e, &far, &far_pw, "tag_label", "F").await;
    let enrich = json!({ "query": "FROM far:items | ENRICH _remote:tag_label | KEEP n, label | SORT n | LIMIT 1" });
    let (s, v) = e.es(&format!("reader@{near}"), "pass-word-1", Method::POST, "/_query", Some(enrich.clone())).await;
    let reason = v["error"]["reason"].as_str().unwrap_or_default();
    assert!(s == 403 && reason.starts_with("action [cluster:monitor/xpack/enrich/esql/resolve_policy] towards remote cluster is unauthorized for user [reader]"), "{v}");
    let reader = json!({ "indices": [{ "names": ["*"], "privileges": ["read"] }], "remote_indices": [{ "clusters": ["far"], "names": ["items"], "privileges": ["read"] }], "remote_cluster": [{ "clusters": ["far"], "privileges": ["monitor_enrich"] }] });
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_security/role/reader", Some(reader)).await.0, 200);
    let (s, v) = e.es(&format!("reader@{near}"), "pass-word-1", Method::POST, "/_query", Some(enrich)).await;
    assert_eq!((s, v["values"].clone()), (200, json!([[0, "F-a"]])), "{v}");

    // A remote runs only what it parses a part to be: a STATS part whose text is no STATS, or a
    // part naming no alias, is refused whatever the request says.
    {
        let links = e.control.node.cfg.links.clone().unwrap();
        let near_id = e.get("/v1/search/clusters/esql-sec-near").await["id"].as_str().unwrap().to_string();
        let roles = json!([{ "r0": { "cluster": [], "indices": [{ "names": ["*"], "privileges": ["read"] }] } }]);
        for (alias, mode, want) in [(json!("far"), "stats", "an ES|QL part this cluster won't run: its commands aren't a STATS part"), (Value::Null, "rows", "an ES|QL part roll can't read")] {
            let body = json!({ "query": "FROM \"items\" | LIMIT 5", "__roll_esql_part": { "alias": alias, "mode": mode, "schema": [], "policies": [] } });
            let mut req = search_es::rest::EsRequest::json("POST", "/_query", body.to_string().into_bytes());
            req.headers.push(("x-roll-remote-access".into(), json!({ "cluster": "esql-sec-near", "who": "user [t]", "roles": roles }).to_string()));
            req.headers.push(("x-roll-esql-part".into(), "1".into()));
            let r = links.send(&near_id, "far", "esql-sec-far", req).await.unwrap();
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!((r.status, v["error"]["reason"].as_str()), (400, Some(want)), "{v}");
        }
    }

    // A part's answer is a link's only: a client asking gets an ordinary one.
    let part = r#"{"mode":"rows","schema":[],"policies":[]}"#;
    let r = e.http.post(format!("{}/_query", e.es)).basic_auth(&far, Some(&far_pw)).header("x-roll-esql-part", part).json(&json!({ "query": "FROM items | STATS c = COUNT(*)" })).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert!(v.get("__roll_esql_part").is_none() && v["values"] == json!([[20]]), "{v}");
}

/// Scrolls and points in time over linked clusters: one opaque id holds each cluster's own
/// scroll or point in time, pages go through every document once and in order (aggregations on
/// the first page), `search_after` pages a point in time exactly, clearing and closing fan out
/// to the parts, and the id answers only on the cluster that made it, to the caller that made it.
#[tokio::test]
async fn scroll_and_point_in_time_across_clusters() {
    use reqwest::Method;
    let e = env().await;
    let (near, near_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "pg-near" })).await);
    let (far, far_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "pg-far" })).await);
    ccs_items(&e, &near, &near_pw, (0..90).filter(|i| i % 3 != 0)).await;
    ccs_items(&e, &far, &far_pw, (0..90).filter(|i| i % 3 == 0)).await;
    let (s, v) = e.call(Method::PUT, "/v1/search/clusters/pg-near/remotes/far", Some(json!({ "cluster": "pg-far" }))).await;
    assert_eq!(s, 200, "{v}");
    // Another user of the same cluster, who may read both: still not the scroll's caller.
    let role = json!({ "indices": [{ "names": ["*"], "privileges": ["read"] }], "remote_indices": [{ "clusters": ["far"], "names": ["*"], "privileges": ["read"] }] });
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_security/role/all_reader", Some(role)).await.0, 200);
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_security/user/other", Some(json!({ "password": "pass-word-1", "roles": ["all_reader"] }))).await.0, 200);
    let other = format!("other@{near}");
    let missing = |s: u16, v: &Value| s == 404 && v.to_string().contains("search_context_missing_exception");

    // Scroll: every document once, in order, 7 a page.
    let body = json!({ "size": 7, "sort": [{ "n": "asc" }], "_source": false, "aggs": { "a": { "avg": { "field": "n" } } } });
    let (s, v) = e.es(&near, &near_pw, Method::POST, "/items,far:items/_search?scroll=1m", Some(body.clone())).await;
    assert_eq!((s, v["aggregations"]["a"]["value"].as_f64(), v["hits"]["total"]["value"].as_i64()), (200, Some(44.5), Some(90)), "{v}");
    let id = v["_scroll_id"].as_str().unwrap().to_string();
    assert!(!id.contains("far") && !id.contains("pg-"), "opaque: {id}");
    let mut seen: Vec<i64> = v["hits"]["hits"].as_array().unwrap().iter().map(|h| h["sort"][0].as_i64().unwrap()).collect();
    let mut indices: Vec<String> = Vec::new();
    let mut pages = 1;
    loop {
        let (s, v) = e.es(&near, &near_pw, Method::POST, "/_search/scroll", Some(json!({ "scroll_id": id, "scroll": "1m" }))).await;
        assert_eq!((s, v["hits"]["total"]["value"].as_i64()), (200, Some(90)), "{v}");
        let hits = v["hits"]["hits"].as_array().unwrap();
        if hits.is_empty() {
            break;
        }
        assert!(hits.len() <= 7, "{v}");
        seen.extend(hits.iter().map(|h| h["sort"][0].as_i64().unwrap()));
        indices.extend(hits.iter().map(|h| h["_index"].as_str().unwrap().to_string()));
        pages += 1;
        // Bound to its caller and its cluster.
        if pages == 2 {
            let (s, v) = e.es(&other, "pass-word-1", Method::POST, "/_search/scroll", Some(json!({ "scroll_id": id }))).await;
            assert!(missing(s, &v), "another caller: {s} {v}");
            let (s, v) = e.es(&far, &far_pw, Method::POST, "/_search/scroll", Some(json!({ "scroll_id": id }))).await;
            assert!(missing(s, &v), "another cluster: {s} {v}");
        }
    }
    assert_eq!(seen, (0..90).collect::<Vec<i64>>());
    assert_eq!(pages, 13);
    indices.sort();
    indices.dedup();
    assert_eq!(indices, ["far:items", "items"]);
    // Clearing frees each cluster's contexts (two shards each); the id is gone after.
    let (_, v) = e.es(&near, &near_pw, Method::POST, "/items,far:items/_search?scroll=1m", Some(body)).await;
    let id = v["_scroll_id"].as_str().unwrap().to_string();
    let (s, v) = e.es(&near, &near_pw, Method::DELETE, "/_search/scroll", Some(json!({ "scroll_id": id }))).await;
    assert_eq!((s, v["num_freed"].as_i64()), (200, Some(4)), "{v}");
    let (s, v) = e.es(&near, &near_pw, Method::POST, "/_search/scroll", Some(json!({ "scroll_id": id, "scroll": "1m" }))).await;
    assert!(missing(s, &v), "{s} {v}");

    // A point in time on each cluster: later writes don't show, `search_after` pages exactly.
    let (s, v) = e.es(&near, &near_pw, Method::POST, "/items,far:items/_pit?keep_alive=1m", None).await;
    assert_eq!((s, v["_shards"]["total"].as_i64(), v["_clusters"]["successful"].as_i64()), (200, Some(4), Some(2)), "{v}");
    let pit = v["id"].as_str().unwrap().to_string();
    for (u, p) in [(&near, &near_pw), (&far, &far_pw)] {
        assert_eq!(e.es(u, p, Method::PUT, "/items/_doc/late?refresh=true", Some(ccs_doc(1000))).await.0, 201);
    }
    let mut hits: Vec<(String, String, i64)> = Vec::new();
    let mut after: Option<Value> = None;
    loop {
        let mut q = json!({ "size": 7, "pit": { "id": pit, "keep_alive": "1m" }, "sort": [{ "w": "desc" }], "_source": false });
        if let Some(a) = &after {
            q["search_after"] = a.clone();
        }
        let (s, v) = e.es(&near, &near_pw, Method::POST, "/_search", Some(q)).await;
        assert_eq!((s, v["pit_id"].as_str()), (200, Some(pit.as_str())), "{v}");
        let page = v["hits"]["hits"].as_array().unwrap();
        let Some(last) = page.last() else { break };
        after = Some(last["sort"].clone());
        hits.extend(page.iter().map(|h| (h["_index"].as_str().unwrap().to_string(), h["_id"].as_str().unwrap().to_string(), h["sort"][0].as_i64().unwrap())));
    }
    assert_eq!(hits.len(), 90, "every document once, none written later");
    let mut ids: Vec<(String, String)> = hits.iter().map(|(i, d, _)| (i.clone(), d.clone())).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 90);
    assert!(hits.windows(2).all(|w| w[0].2 >= w[1].2), "sorted by w, descending");
    let (s, v) = e.es(&other, "pass-word-1", Method::POST, "/_search", Some(json!({ "pit": { "id": pit } }))).await;
    assert!(missing(s, &v), "another caller: {s} {v}");
    let (s, v) = e.es(&near, &near_pw, Method::DELETE, "/_pit", Some(json!({ "id": pit }))).await;
    assert_eq!((s, v["num_freed"].as_i64()), (200, Some(4)), "{v}");
    let (s, v) = e.es(&near, &near_pw, Method::DELETE, "/_pit", Some(json!({ "id": pit }))).await;
    assert_eq!((s, v["succeeded"].as_bool()), (404, Some(false)), "{v}");

    // Every search on a point in time is authorized for its caller: one who lost the local
    // index loses it there too, as on a point in time of one cluster.
    let role = |names: &[&str]| json!({ "indices": [{ "names": names, "privileges": ["read"] }], "remote_indices": [{ "clusters": ["far"], "names": ["*"], "privileges": ["read"] }] });
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_security/role/shrinking", Some(role(&["items"]))).await.0, 200);
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_security/user/shrinks", Some(json!({ "password": "pass-word-1", "roles": ["shrinking"] }))).await.0, 200);
    let shrinks = format!("shrinks@{near}");
    let (s, v) = e.es(&shrinks, "pass-word-1", Method::POST, "/items,far:items/_pit?keep_alive=1m", None).await;
    assert_eq!(s, 200, "{v}");
    let pit = v["id"].as_str().unwrap().to_string();
    let (s, v) = e.es(&shrinks, "pass-word-1", Method::POST, "/_search", Some(json!({ "pit": { "id": pit }, "size": 0 }))).await;
    assert_eq!((s, v["hits"]["total"]["value"].as_i64()), (200, Some(92)), "{v}");
    // `_msearch` items on a point in time are authorized the same way.
    let msearch = |pit: &str| {
        let body = format!("{{}}\n{}\n", json!({ "pit": { "id": pit }, "size": 0 }));
        e.http.post(format!("{}/_msearch", e.es)).basic_auth(&shrinks, Some("pass-word-1")).header("content-type", "application/x-ndjson").body(body).send()
    };
    let m: Value = msearch(&pit).await.unwrap().json().await.unwrap();
    assert_eq!(m["responses"][0]["hits"]["total"]["value"].as_i64(), Some(92), "{m}");
    assert_eq!(e.es(&near, &near_pw, Method::PUT, "/_security/role/shrinking", Some(role(&["other-*"]))).await.0, 200);
    let (s, v) = e.es(&shrinks, "pass-word-1", Method::POST, "/_search", Some(json!({ "pit": { "id": pit }, "size": 0 }))).await;
    assert_eq!((s, v["error"]["type"].as_str()), (403, Some("security_exception")), "{v}");
    let m: Value = msearch(&pit).await.unwrap().json().await.unwrap();
    assert_eq!((m["responses"][0]["status"].as_i64(), m["responses"][0]["error"]["type"].as_str()), (Some(403), Some("security_exception")), "{m}");
    // A point in time names its own indices: naming others is refused before anything reads them.
    let (s, v) = e.es(&shrinks, "pass-word-1", Method::POST, "/items/_search", Some(json!({ "pit": { "id": pit } }))).await;
    assert_eq!((s, v["error"]["type"].as_str()), (400, Some("action_request_validation_exception")), "{v}");

    // One cluster's point in time is its opener's too: another user can't search or close it.
    let (_, v) = e.es(&near, &near_pw, Method::POST, "/items/_pit?keep_alive=1m", None).await;
    let local = v["id"].as_str().unwrap().to_string();
    let (s, v) = e.es(&other, "pass-word-1", Method::POST, "/_search", Some(json!({ "pit": { "id": local } }))).await;
    assert!(missing(s, &v), "{s} {v}");
    let (s, v) = e.es(&other, "pass-word-1", Method::DELETE, "/_pit", Some(json!({ "id": local }))).await;
    assert_eq!((s, v["num_freed"].as_i64()), (404, Some(0)), "{v}");
    let (s, v) = e.es(&near, &near_pw, Method::POST, "/_search", Some(json!({ "pit": { "id": local }, "size": 0 }))).await;
    assert_eq!(s, 200, "still its opener's: {v}");
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

/// An alias pattern matching a link and a loopback remote (whose seed is the node's own address,
/// which any tenant may set) is authorized locally, whole: a user who may read `app` alone gets
/// nothing of `secret` through `*:secret`, as through `secret` and `me:secret` (the review of
/// #347 found `*:secret` skipped the local check while the loopback searched it).
#[tokio::test]
async fn an_alias_pattern_over_a_link_and_a_loopback_is_authorized_locally() {
    use reqwest::Method;
    let e = env().await;
    let (app, app_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "wild-app" })).await);
    e.post("/v1/search/clusters", json!({ "name": "wild-logs" })).await;
    for path in ["/app/_doc/1", "/secret/_doc/1"] {
        let (s, v) = e.es(&app, &app_pw, Method::PUT, &format!("{path}?refresh=true"), Some(json!({ "msg": "secret doc" }))).await;
        assert_eq!(s, 201, "{v}");
    }
    let (s, v) = e.call(Method::PUT, "/v1/search/clusters/wild-app/remotes/logs", Some(json!({ "cluster": "wild-logs" }))).await;
    assert_eq!(s, 200, "{v}");
    let (_, nodes) = e.es(&app, &app_pw, Method::GET, "/_nodes/_local", None).await;
    let me = nodes["nodes"].as_object().unwrap().values().next().unwrap()["transport_address"].as_str().unwrap().to_string();
    let (s, v) = e.es(&app, &app_pw, Method::PUT, "/_cluster/settings", Some(json!({ "persistent": { "cluster.remote.me.seeds": me } }))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(e.es(&app, &app_pw, Method::PUT, "/_security/role/app_only", Some(json!({ "indices": [{ "names": ["app"], "privileges": ["read"] }] }))).await.0, 200);
    assert_eq!(e.es(&app, &app_pw, Method::PUT, "/_security/user/u", Some(json!({ "password": "pass-word-1", "roles": ["app_only"] }))).await.0, 200);
    let user = format!("u@{app}");
    for path in ["/secret/_search", "/me:secret/_search"] {
        assert_eq!(e.es(&user, "pass-word-1", Method::GET, path, None).await.0, 403, "{path}");
    }
    let (s, v) = e.es(&user, "pass-word-1", Method::GET, "/*:secret/_search", None).await;
    assert!(s != 200 || v["hits"]["hits"].as_array().is_some_and(|h| h.is_empty()), "{s} {v}");
    assert!(!v.to_string().contains("secret doc"), "{v}");
    let r = e.http.post(format!("{}/_msearch", e.es)).basic_auth(&user, Some("pass-word-1")).header("content-type", "application/x-ndjson").body("{\"index\":\"*:secret\"}\n{}\n").send().await.unwrap();
    let m = r.text().await.unwrap();
    assert!(!m.contains("secret doc"), "{m}");
}

/// A migration from another Elasticsearch-compatible cluster (here another roll cluster on this
/// node, reached by its REST address as an outside cluster would be): the remote's indices are
/// listed through roll, the picked one's mappings, settings and aliases copied with what roll
/// doesn't take dropped and said why, its documents reindexed by a task the console follows;
/// the remote's password is in no answer, error or task. An alias roll already has is never
/// joined, and the cluster's reindex whitelist gains exactly the remote's host (nothing else is
/// readable).
#[tokio::test]
async fn migrating_from_another_cluster() {
    use reqwest::Method;
    let e = env_with(|c| c.node_settings.push((search_control::REINDEX_ALLOW_PRIVATE.into(), "true".into()))).await;
    let (old, old_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "old" })).await);
    e.post("/v1/search/clusters", json!({ "name": "new" })).await;
    let index = json!({
        "settings": { "number_of_shards": 2, "refresh_interval": "5s", "routing": { "allocation": { "include": { "_tier_preference": "data_hot" } } } },
        "mappings": { "properties": { "name": { "type": "text" }, "sku": { "type": "keyword" }, "price": { "type": "double" } } },
        "aliases": { "catalog": {}, "shop": { "is_write_index": true } },
    });
    let (s, v) = e.es(&old, &old_pw, Method::PUT, "/products", Some(index)).await;
    assert_eq!(s, 200, "{v}");
    for (i, name) in ["kettle", "teapot", "mug"].iter().enumerate() {
        let (s, v) = e.es(&old, &old_pw, Method::PUT, &format!("/products/_doc/{i}?refresh=true"), Some(json!({ "name": name, "sku": format!("s{i}"), "price": i as f64 + 0.5 }))).await;
        assert_eq!(s, 201, "{v}");
    }
    // A write block on the source is the remote's business: it isn't copied.
    let (s, v) = e.es(&old, &old_pw, Method::PUT, "/products/_settings", Some(json!({ "index.blocks.write": true }))).await;
    assert_eq!(s, 200, "{v}");
    let source = json!({ "host": e.es, "username": old, "password": old_pw });
    // On roll, `shop` already names another index's alias.
    let (_, made) = e.call(Method::POST, "/v1/search/clusters/new/request", Some(json!({ "method": "PUT", "path": "/other", "body": { "aliases": { "shop": {} } } }))).await;
    assert_eq!(made["status"], 200, "{made}");

    // A wrong password: the remote's 401, never the password.
    let (s, v) = e.call(Method::POST, "/v1/search/clusters/new/migrate/inspect", Some(json!({ "source": { "host": e.es, "username": old, "password": "wrong-pass-123" } }))).await;
    assert_eq!(s, 400, "{v}");
    // Its status and error type only, never its body.
    let msg = v["error"].as_str().unwrap_or_default();
    assert!(msg.contains("answered 401") && msg.contains("error type [security_exception]") && !msg.contains("unable to authenticate") && !v.to_string().contains("wrong-pass-123"), "{v}");

    let listed = e.post("/v1/search/clusters/new/migrate/inspect", json!({ "source": source })).await;
    let row = listed["indices"].as_array().unwrap().iter().find(|r| r["index"] == "products").cloned().unwrap_or_default();
    let mut aliases: Vec<&str> = row["aliases"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    aliases.sort();
    assert_eq!((row["docs"].as_i64(), aliases, row["aliases_on_roll"].clone(), row["exists"].as_bool()), (Some(3), vec!["catalog", "shop"], json!(["shop"]), Some(false)), "{listed}");
    assert!(listed["remote"]["version"].as_str().is_some() && !listed.to_string().contains(&old_pw), "{listed}");

    let started = e.post("/v1/search/clusters/new/migrate", json!({ "source": source, "indices": [{ "index": "products" }], "size": 2 })).await;
    let r = &started["indices"][0];
    let task = r["task"].as_str().unwrap_or_else(|| panic!("{started}")).to_string();
    let dropped: Vec<&str> = r["dropped"].as_array().unwrap().iter().filter_map(|d| d["setting"].as_str()).collect();
    for s in ["index.uuid", "index.creation_date", "index.blocks.write", "index.routing.allocation.include._tier_preference", "aliases.shop"] {
        assert!(dropped.contains(&s), "{s} dropped: {started}");
    }
    assert_eq!(r["aliases"], json!(["catalog"]), "{started}");
    assert!(!started.to_string().contains(&old_pw), "{started}");
    // The cluster may now read exactly that host, and no other.
    let port = e.es.rsplit(':').next().unwrap();
    assert_eq!(e.get("/v1/search/clusters/new").await["reindex_whitelist"], json!([format!("127.0.0.1:{port}")]));
    let other = json!({ "source": { "remote": { "host": "http://127.0.0.1:1" }, "index": "products" }, "dest": { "index": "x" } });
    let (_, refused) = e.call(Method::POST, "/v1/search/clusters/new/request", Some(json!({ "method": "POST", "path": "/_reindex", "body": other }))).await;
    assert_eq!(body(&refused)["error"]["reason"], "[127.0.0.1:1] not whitelisted in reindex.remote.whitelist", "{refused}");

    // The console follows the task to its end; nothing it shows holds the password.
    let (_, done) = e.call(Method::POST, "/v1/search/clusters/new/request", Some(json!({ "method": "GET", "path": format!("/_tasks/{task}?wait_for_completion=true&timeout=30s") }))).await;
    let t = body(&done);
    assert_eq!((t["completed"].as_bool(), t["response"]["created"].as_i64(), t["response"]["batches"].as_i64()), (Some(true), Some(3), Some(2)), "{t}");
    let desc = t["task"]["description"].as_str().unwrap_or_default();
    assert!(desc.starts_with("reindex from [host=127.0.0.1 port=") && desc.ends_with("][products] to [products]"), "{desc}");
    assert!(!done.to_string().contains(&old_pw), "{done}");

    // The task refreshed its copy: no refresh needed before searching it.
    let (_, found) = e.call(Method::POST, "/v1/search/clusters/new/request", Some(json!({ "method": "POST", "path": "/catalog/_search", "body": { "query": { "term": { "sku": "s1" } } } }))).await;
    let found = body(&found);
    assert_eq!(found["hits"]["hits"][0]["_source"]["name"], "teapot", "{found}");
    let (_, settings) = e.call(Method::POST, "/v1/search/clusters/new/request", Some(json!({ "method": "GET", "path": "/products/_settings?flat_settings=true" }))).await;
    let settings = body(&settings);
    assert_eq!(settings["products"]["settings"]["index.number_of_shards"], "2", "{settings}");
    assert!(settings["products"]["settings"]["index.blocks.write"].is_null(), "{settings}");
    // `shop` still names `other` alone.
    let (_, shop) = e.call(Method::POST, "/v1/search/clusters/new/request", Some(json!({ "method": "GET", "path": "/_alias/shop" }))).await;
    assert_eq!(index_names(&body(&shop)), ["other"], "{shop}");

    // Again: the index is there now, so it isn't overwritten.
    let again = e.post("/v1/search/clusters/new/migrate", json!({ "source": source, "indices": [{ "index": "products" }] })).await;
    assert!(again["indices"][0]["error"].as_str().is_some_and(|m| m.contains("already exists")), "{again}");
}

/// Query suggestions from search analytics: with analytics and suggestions on, the metering
/// pass builds `<index>-suggestions` from the index's searches (weighted by count, raised by
/// click-through), which the completion suggester and `search_as_you_type` serve; a rare query,
/// a blocked one and one that finds nothing are never suggested. A rebuild swaps the alias in
/// one step (searches during it see the old build or the new, never none) and writes nothing
/// when nothing changed. A sleeping cluster is woken only when its suggestions are due and
/// would change. Turning analytics off deletes the suggestions index.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn query_suggestions_from_search_analytics() {
    use reqwest::Method;
    let e = env_with(|c| c.job_wake_interval = Some(Duration::from_millis(200))).await;
    let c = e.post("/v1/search/clusters", json!({"name": "shop"})).await;
    let id = c["id"].as_str().unwrap().to_string();
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
    for (i, t) in ["red shoes", "blue shoes", "rare query", "blocked one", "secret sale", "green shoes", "purple hat"].iter().enumerate() {
        es(Method::PUT, &format!("/products/_doc/{i}?refresh=true"), Some(json!({ "title": t }))).await;
    }
    let status = async || e.get("/v1/search/clusters/shop/indices/products/suggestions").await;
    let search = |q: &str| es(Method::POST, "/products/_search", Some(json!({"query": {"match_phrase": {"title": q}}})));
    let complete = async |prefix: &str| {
        let v = es(Method::POST, "/products-suggestions/_search", Some(json!({"suggest": {"s": {"prefix": prefix, "completion": {"field": "suggest", "size": 10}}}}))).await;
        v["suggest"]["s"][0]["options"].as_array().unwrap().iter().map(|o| (o["text"].as_str().unwrap().to_string(), o["_score"].as_f64().unwrap() as u64)).collect::<Vec<_>>()
    };

    // Off: nothing to rebuild.
    let (s, v) = e.call(Method::POST, "/v1/search/clusters/shop/indices/products/suggestions/rebuild", None).await;
    assert_eq!(s, 400, "{v}");
    es(Method::PUT, "/products/_settings", Some(json!({"index.roll.analytics.enabled": true, "index.roll.suggestions": {"enabled": true, "block": ["Blocked One"], "block_prefix": ["secret"]}}))).await;
    for (q, n) in [("red shoes", 6), ("blue shoes", 5), ("rare query", 2), ("blocked one", 6), ("secret sale", 6), ("sandals", 6)] {
        for _ in 0..n {
            search(q).await;
        }
    }
    // Every search for blue shoes was clicked through: its weight doubles past red shoes'.
    es(Method::PUT, "/_application/analytics/web", None).await;
    for _ in 0..5 {
        let click = json!({"session": {"id": "s"}, "user": {"id": "u"}, "document": {"index": "products", "id": "1"}, "search": {"query": "blue shoes"}});
        es(Method::POST, "/_application/analytics/web/event/search_click", Some(click)).await;
    }
    // The metering pass stores the analytics and builds what's due: these never were.
    e.control.sample().await.unwrap();
    let st = status().await;
    assert_eq!((st["enabled"].as_bool(), st["analytics"].as_bool(), st["last_build"]["count"].as_i64()), (Some(true), Some(true), Some(2)), "{st}");
    assert_eq!(st["suggestions"].as_array().unwrap().iter().map(|s| s["query"].as_str().unwrap()).collect::<Vec<_>>(), ["blue shoes", "red shoes"], "{st}");
    assert_eq!(complete("b").await, [("blue shoes".to_string(), 10)]);
    assert_eq!(complete("r").await, [("red shoes".to_string(), 6)], "the rare query isn't suggested");
    assert!(complete("s").await.is_empty(), "blocked by prefix, and a query that found nothing");
    let v = es(Method::POST, "/products-suggestions/_search", Some(json!({"query": {"multi_match": {"query": "sho", "type": "bool_prefix", "fields": ["query", "query._2gram", "query._3gram"]}}}))).await;
    assert_eq!(v["hits"]["total"]["value"].as_u64(), Some(2), "{v}");
    let first = st["last_build"]["index"].as_str().unwrap().to_string();

    // Nothing changed: a rebuild writes nothing.
    let v = e.post("/v1/search/clusters/shop/indices/products/suggestions/rebuild", json!({})).await;
    assert_eq!((v["changed"].as_bool(), v["last_build"]["index"].as_str()), (Some(false), Some(first.as_str())), "{v}");

    // New searches: a rebuild swaps the alias while clients search it.
    for _ in 0..5 {
        search("green shoes").await;
    }
    e.control.sample().await.unwrap();
    assert_eq!(status().await["last_build"]["index"].as_str(), Some(first.as_str()), "not due again yet");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let (stop, url, user, pw) = (stop.clone(), format!("{}/products-suggestions/_search", e.es), user.clone(), pw.clone());
        tokio::spawn(async move {
            let http = reqwest::Client::new();
            let mut seen = 0;
            // At least one search, even when the rebuild returns before this task first runs.
            loop {
                let r = http.post(&url).basic_auth(&user, Some(&pw)).json(&json!({"size": 0, "query": {"match_all": {}}})).send().await.unwrap();
                assert_eq!(r.status().as_u16(), 200);
                let v: Value = r.json().await.unwrap();
                let n = v["hits"]["total"]["value"].as_u64().unwrap();
                assert!(n == 2 || n == 3, "a reader saw {n} suggestions: {v}");
                seen += 1;
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
            }
            seen
        })
    };
    let v = e.post("/v1/search/clusters/shop/indices/products/suggestions/rebuild", json!({})).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(reader.await.unwrap() > 0);
    let second = v["last_build"]["index"].as_str().unwrap().to_string();
    assert!(v["changed"] == true && second != first, "{v}");
    let aliases = es(Method::GET, "/_alias/products-suggestions", None).await;
    assert_eq!(aliases.as_object().unwrap().keys().collect::<Vec<_>>(), [&second], "{aliases}");
    let (s, _) = e.es(&user, &pw, Method::GET, &format!("/{first}"), None).await;
    assert_eq!(s, 404, "the previous build is gone");
    assert_eq!(complete("g").await, [("green shoes".to_string(), 5)]);

    // Blocking a term (the dashboard's one click: the setting, then a rebuild).
    es(Method::PUT, "/products/_settings", Some(json!({"index.roll.suggestions.block": ["blocked one", "red shoes"]}))).await;
    e.control.sample().await.unwrap();
    assert!(complete("r").await.is_empty(), "a changed setting rebuilds on the next pass");

    // Asleep: not woken before it's due, nor when due with nothing new to suggest.
    let unload = async || {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let v = e.post("/v1/search/clusters/shop/unload", json!({})).await;
            if v["unloaded"] == true || !e.control.node.loaded().iter().any(|n| &**n == "shop") {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "never unloaded: {v}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let loaded = || e.control.node.loaded().iter().any(|n| &**n == "shop");
    e.control.sample().await.unwrap();
    unload().await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!loaded(), "woken before its suggestions were due");
    let row = e.control.store.suggestion(&id, "products").await.unwrap().unwrap();
    assert!(row.due_at > search_control::meter::now_ms() + 3_600_000, "{row:?}");
    e.control.store.set_suggestion_due(&id, "products", 0, None).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while e.control.store.suggestion(&id, "products").await.unwrap().unwrap().due_at == 0 {
        assert!(tokio::time::Instant::now() < deadline, "the due check never ran");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!loaded(), "woken though its suggestions wouldn't change");
    // Due with something new (searches another node handed on): woken, and rebuilt.
    let mut b = search_es::analytics::Bucket::new(100, 50);
    b.searches = 9;
    b.top.add("purple hat", &search_es::analytics::QueryStats { searches: 9, hits: 9, ..Default::default() });
    let hour = (search_control::meter::now_ms() / 3_600_000) as u64;
    e.control.store.record_analytics(&id, &[], None, &[("products".to_string(), hour, b)]).await.unwrap();
    e.control.store.set_suggestion_due(&id, "products", 0, None).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !status().await["suggestions"].as_array().unwrap().iter().any(|s| s["query"] == "purple hat") {
        assert!(tokio::time::Instant::now() < deadline, "the due rebuild never woke its cluster");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(loaded());
    assert_eq!(complete("p").await, [("purple hat".to_string(), 9)]);

    // Another person's cluster: no such cluster.
    let (s, _) = e.call_as("mallory", Method::GET, "/v1/search/clusters/shop/indices/products/suggestions", None).await;
    assert_eq!(s, 404);
    // Analytics off: the suggestions (query text) go with them.
    es(Method::PUT, "/products/_settings", Some(json!({"index.roll.analytics.enabled": null}))).await;
    e.control.sample().await.unwrap();
    let (s, v) = e.es(&user, &pw, Method::GET, "/products-suggestions", None).await;
    assert_eq!(s, 404, "{v}");
    assert!(e.control.store.suggestion(&id, "products").await.unwrap().is_none());
}

/// A suggestions build touches only its own names: the alias is always `<index>-suggestions`
/// (no setting names another), only roll's build can mark an index as one (the mark's settings
/// are private: refused on a user's create and update), and when that name is already someone else's index, or an alias
/// over an index roll didn't build from this source, the build records the error and writes
/// nothing: the index keeps its documents, the alias keeps pointing where it did, and no build
/// is left behind.
#[tokio::test]
async fn suggestions_never_touch_a_name_they_did_not_build() {
    use reqwest::Method;
    let e = env().await;
    let c = e.post("/v1/search/clusters", json!({"name": "mall"})).await;
    let (user, pw) = creds(&c);
    let es = |m: Method, p: &str, b: Option<Value>| {
        let (user, pw, p, e) = (user.clone(), pw.clone(), p.to_string(), &e);
        async move { e.es(&user, &pw, m, &p, b).await }
    };
    let ok = async |m: Method, p: &str, b: Option<Value>| {
        let (s, v) = es(m, p, b).await;
        assert!(s < 300, "{p}: {s} {v}");
        v
    };
    // The alias can't be set to another name.
    ok(Method::PUT, "/orders", Some(json!({"mappings": {"properties": {"title": {"type": "text"}}}}))).await;
    let (s, v) = es(Method::PUT, "/orders/_settings", Some(json!({"index.roll.suggestions.index": "accounts"}))).await;
    assert_eq!(s, 400, "{v}");
    // Nor can anyone mark an index as a build: the mark is private, refused on create and update.
    let uuid = ok(Method::GET, "/orders/_settings/index.uuid?flat_settings=true", None).await["orders"]["settings"]["index.uuid"].as_str().unwrap().to_string();
    let mark = json!({"index.roll.suggestions.source": "orders", "index.roll.suggestions.source_uuid": uuid});
    let (s, v) = es(Method::PUT, "/fake-build", Some(json!({ "settings": mark }))).await;
    assert!(s == 400 && v.to_string().contains("private index setting [index.roll.suggestions.source"), "{s} {v}");
    let (s, v) = es(Method::GET, "/fake-build", None).await;
    assert_eq!(s, 404, "{v}");
    let (s, v) = es(Method::PUT, "/orders/_settings", Some(mark.clone())).await;
    assert!(s == 400 && v.to_string().contains("can not update private setting [index.roll.suggestions.source"), "{s} {v}");
    // Someone else's index under `orders-suggestions`, and an alias `crm-suggestions` over theirs.
    ok(Method::PUT, "/orders-suggestions/_doc/1?refresh=true", Some(json!({"theirs": true}))).await;
    ok(Method::PUT, "/accounts/_doc/1?refresh=true", Some(json!({"theirs": true}))).await;
    ok(Method::POST, "/_aliases", Some(json!({"actions": [{"add": {"index": "accounts", "alias": "crm-suggestions"}}]}))).await;
    for index in ["orders", "crm"] {
        ok(Method::PUT, &format!("/{index}/_doc/1?refresh=true"), Some(json!({"title": "blue bag"}))).await;
        ok(Method::PUT, &format!("/{index}/_settings"), Some(json!({"index.roll.analytics.enabled": true, "index.roll.suggestions.enabled": true}))).await;
        for _ in 0..5 {
            ok(Method::POST, &format!("/{index}/_search"), Some(json!({"query": {"match": {"title": "blue bag"}}}))).await;
        }
    }
    // The scheduled build and a rebuild both refuse, and say why.
    e.control.sample().await.unwrap();
    for (index, why) in [("orders", "[orders-suggestions] is an index roll didn't build"), ("crm", "the alias [crm-suggestions] points at [accounts]")] {
        let st = e.get(&format!("/v1/search/clusters/mall/indices/{index}/suggestions")).await;
        assert!(st["last_build"]["error"].as_str().is_some_and(|m| m.contains(why)), "{st}");
        assert!(st["last_build"]["at"].is_null(), "{st}");
        let (s, v) = e.call(Method::POST, &format!("/v1/search/clusters/mall/indices/{index}/suggestions/rebuild"), None).await;
        assert!(s == 409 && v["error"].as_str().is_some_and(|m| m.contains(why)), "{s} {v}");
    }
    // Untouched: their documents, where the alias points, and no build left behind.
    for index in ["orders-suggestions", "accounts"] {
        let v = ok(Method::GET, &format!("/{index}/_doc/1"), None).await;
        assert_eq!(v["_source"], json!({"theirs": true}), "{v}");
    }
    let v = ok(Method::GET, "/_alias/crm-suggestions", None).await;
    assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), ["accounts"], "{v}");
    let v = ok(Method::GET, "/orders-suggestions/_alias", None).await;
    assert_eq!(v["orders-suggestions"]["aliases"], json!({}), "{v}");
    let v = ok(Method::GET, "/_cat/indices?format=json&h=index", None).await;
    let names: Vec<&str> = v.as_array().unwrap().iter().filter_map(|r| r["index"].as_str()).collect();
    assert!(!names.iter().any(|n| n.starts_with("orders-suggestions-") || n.starts_with("crm-suggestions-")), "{names:?}");
}

/// The node's embedding pool across tenants, on the tiny made-up model (`search_embed::tiny`):
/// `semantic_text` with the E5 endpoint end to end, a reindex into it on two workers (a smoke
/// test: the batches of 10 with no deadline are search-es's `tests/e5.rs`), each cluster's queue capped with Elasticsearch's 429, the per-call cap, an input
/// with no spaces cut instead of refused, the CPU each cluster's inputs took metered to it, and
/// the model dropped once idle. (The order of turns is `search_server::embed`'s unit tests'.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_embedding_pool_takes_turns_caps_queues_and_meters_cpu() {
    use search_server::embed::{EmbedConfig, ModelSource};
    let model = std::env::temp_dir().join(format!("roll-search-e5-tiny-{}", std::process::id()));
    search_embed::tiny::write(&model).unwrap();
    let dir = model.clone();
    let e = env_with(move |c| {
        let mut ec = EmbedConfig::new(ModelSource::Dir(dir), c.home.join("models"));
        ec.unchecked = true;
        ec.workers = 2;
        ec.queue_inputs = 72;
        ec.max_inputs = 100;
        ec.idle_unload = Duration::from_secs(2);
        c.embed = Some(ec);
    })
    .await;
    let mut clusters = Vec::new();
    for name in ["a", "b"] {
        let c = e.post("/v1/search/clusters", json!({"name": name})).await;
        clusters.push((c["id"].as_str().unwrap().to_string(), creds(&c)));
    }
    let ((a_id, (au, ap)), (b_id, (bu, bp))) = (clusters[0].clone(), clusters[1].clone());
    let e5 = "/_inference/text_embedding/.multilingual-e5-small-elasticsearch?timeout=120s";

    // semantic_text on the preconfigured endpoint, no key anywhere.
    let mapping = json!({"mappings": {"properties": {"body": {"type": "semantic_text", "inference_id": ".multilingual-e5-small-elasticsearch"}}}});
    let (s, v) = e.es(&au, &ap, reqwest::Method::PUT, "/docs", Some(mapping)).await;
    assert_eq!(s, 200, "{v}");
    let mut bulk = String::new();
    for i in 0..20 {
        bulk.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"body\":\"document number {i} about foxes and dogs\"}}\n"));
    }
    let r = e.http.post(format!("{}/docs/_bulk?refresh=true", e.es)).basic_auth(&au, Some(&ap)).header("content-type", "application/x-ndjson").body(bulk).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["errors"], false, "{v}");
    let (s, v) = e.es(&au, &ap, reqwest::Method::POST, "/docs/_search", Some(json!({"query": {"semantic": {"field": "body", "query": "foxes"}}}))).await;
    assert_eq!(s, 200, "{v}");
    assert!(v["hits"]["total"]["value"].as_i64().unwrap() >= 10, "{v}");

    // A reindex into semantic_text through the pool (smoke test).
    let mut bulk = String::new();
    for i in 0..200 {
        bulk.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"body\":\"source document {i} on reindexing\"}}\n"));
    }
    let r = e.http.post(format!("{}/src/_bulk?refresh=true", e.es)).basic_auth(&au, Some(&ap)).header("content-type", "application/x-ndjson").body(bulk).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["errors"], false);
    let (s, v) = e.es(&au, &ap, reqwest::Method::PUT, "/docs2", Some(json!({"mappings": {"properties": {"body": {"type": "semantic_text", "inference_id": ".multilingual-e5-small-elasticsearch"}}}}))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = e.es(&au, &ap, reqwest::Method::POST, "/_reindex?refresh=true", Some(json!({"source": {"index": "src"}, "dest": {"index": "docs2"}}))).await;
    assert_eq!((s, v["created"].as_i64(), v["failures"].as_array().map(Vec::len)), (200, Some(200), Some(0)), "{v}");
    let (s, v) = e.es(&bu, &bp, reqwest::Method::POST, e5, Some(json!({"input": "short"}))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["text_embedding"][0]["embedding"].as_array().unwrap().len(), 384);
    // Past a cluster's queue (72 inputs) a call is refused whole, as a full deployment refuses.
    let many: Vec<String> = (0..73).map(|i| format!("more {i}")).collect();
    let (s, v) = e.es(&au, &ap, reqwest::Method::POST, e5, Some(json!({"input": many}))).await;
    assert_eq!((s, v["error"]["type"].as_str(), v["error"]["reason"].as_str()), (429, Some("es_rejected_execution_exception"), Some("inference process queue is full. Unable to execute command")), "{v}");
    let (s, v) = e.es(&bu, &bp, reqwest::Method::POST, e5, Some(json!({"input": many[..72]}))).await;
    assert_eq!(s, 200, "{v}");
    // The model is held on the node's memory while loaded.
    let pool = e.control.node.cfg.embedder.clone().unwrap();
    assert!(pool.resident_bytes() > 0);

    // The per-call cap; a long input without a space is cut to its first 512 tokens, not
    // refused.
    let many: Vec<String> = (0..101).map(|i| i.to_string()).collect();
    let (s, v) = e.es(&bu, &bp, reqwest::Method::POST, e5, Some(json!({"input": many}))).await;
    assert_eq!(s, 400, "{v}");
    let (s, v) = e.es(&au, &ap, reqwest::Method::POST, e5, Some(json!({"input": "x".repeat(300_000)}))).await;
    assert_eq!(s, 200, "{v}");

    // Each cluster is billed the CPU its inputs took: a's 220 documents and a 512-token input
    // more than b's 74 short inputs.
    e.control.sample().await.unwrap();
    let ua = e.get(&format!("/v1/search/clusters/{a_id}/usage?days=1")).await;
    let ub = e.get(&format!("/v1/search/clusters/{b_id}/usage?days=1")).await;
    let (ca, cb) = (ua["inference_cpu_seconds"].as_f64().unwrap(), ub["inference_cpu_seconds"].as_f64().unwrap());
    assert!(ca > cb && cb > 0.0, "a {ca}, b {cb}");
    assert!(ua["cost"]["inference_usd"].as_f64().unwrap() > 0.0, "{ua}");

    // Idle: the model leaves memory.
    tokio::time::timeout(Duration::from_secs(10), async {
        while pool.loaded() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the model stays loaded while idle");
    assert_eq!(pool.resident_bytes(), 0);
    let _ = std::fs::remove_dir_all(&model);
}

/// A migration's bounds: a host that isn't a name or an address (`https://*`) is refused and
/// nothing is whitelisted; one migration call runs per cluster at a time (another gets a 429);
/// and copies past the free task slots wait (`queued`), with nothing created for them.
#[tokio::test]
async fn a_migration_is_bounded() {
    use reqwest::Method;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let e = env_with(|c| c.node_settings.push((search_control::REINDEX_ALLOW_PRIVATE.into(), "true".into()))).await;
    let (old, old_pw) = creds(&e.post("/v1/search/clusters", json!({ "name": "src" })).await);
    e.post("/v1/search/clusters", json!({ "name": "dst" })).await;

    for host in ["https://*", "https://*.example.com", "https://h?x.example"] {
        let (s, v) = e.call(Method::POST, "/v1/search/clusters/dst/migrate", Some(json!({ "source": { "host": host }, "indices": [{ "index": "a" }] }))).await;
        assert_eq!(s, 400, "{host}: {v}");
    }
    assert_eq!(e.get("/v1/search/clusters/dst").await["reindex_whitelist"], json!([]), "nothing whitelisted");
    // A name leading to an address the node denies reads like any unreachable remote to
    // `inspect`: no oracle for which names point at roll's own addresses.
    let denied = env_with(|c| {
        c.node_settings.push((search_control::REINDEX_ALLOW_PRIVATE.into(), "true".into()));
        c.node_settings.push((search_control::EGRESS_DENY_ADDRESSES.into(), "127.0.0.0/8,::1".into()));
    })
    .await;
    denied.post("/v1/search/clusters", json!({ "name": "d" })).await;
    let port = e.es.rsplit(':').next().unwrap().to_string();
    let (s, v) = denied.call(Method::POST, "/v1/search/clusters/d/migrate/inspect", Some(json!({ "source": { "host": format!("http://localhost.:{port}") } }))).await;
    assert_eq!((s, v["error"].as_str()), (400, Some(&*format!("couldn't reach the remote at http://localhost:{port}"))), "{v}");

    // A remote that holds its answer: the first call keeps the cluster's slot meanwhile.
    let slow = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow_url = format!("http://{}", slow.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = slow.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                tokio::time::sleep(Duration::from_secs(3)).await;
                let _ = sock.write_all(b"HTTP/1.1 503 X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
            });
        }
    });
    let first = {
        let (http, url) = (e.http.clone(), format!("{}/v1/search/clusters/dst/migrate/inspect", e.api));
        let body = json!({ "source": { "host": slow_url } });
        tokio::spawn(async move { http.post(url).json(&body).send().await.unwrap().status().as_u16() })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (s, v) = e.call(Method::POST, "/v1/search/clusters/dst/migrate/inspect", Some(json!({ "source": { "host": e.es } }))).await;
    assert_eq!(s, 429, "{v}");
    assert_eq!(first.await.unwrap(), 400, "the held call ends with the remote's 503");

    // Three indices, two task slots: the third waits, and isn't created.
    for i in ["a", "b", "c"] {
        let (s, v) = e.es(&old, &old_pw, Method::PUT, &format!("/{i}/_doc/1?refresh=true"), Some(json!({ "n": 1 }))).await;
        assert_eq!(s, 201, "{v}");
    }
    let source = json!({ "host": e.es, "username": old, "password": old_pw });
    let started = e.post("/v1/search/clusters/dst/migrate", json!({ "source": source, "indices": [{ "index": "a" }, { "index": "b" }, { "index": "c" }], "requests_per_second": 0.0001 })).await;
    let rows = started["indices"].as_array().unwrap();
    // Each copy reserves a task slot before reading the remote's definitions, and its `_reindex`
    // uses that reservation: with two slots per cluster, `b` starts only because `a`'s
    // reservation became `a`'s task rather than being held besides it.
    assert!(rows[0]["task"].is_string() && rows[1]["task"].is_string(), "{started}");
    assert_eq!((rows[2]["queued"].as_bool(), rows[2]["created"].as_bool()), (Some(true), None), "{started}");
    let (_, c) = e.call(Method::POST, "/v1/search/clusters/dst/request", Some(json!({ "method": "HEAD", "path": "/c" }))).await;
    assert_eq!(c["status"], 404, "no empty copy: {c}");
    for r in &rows[..2] {
        let t = r["task"].as_str().unwrap();
        e.call(Method::POST, "/v1/search/clusters/dst/request", Some(json!({ "method": "POST", "path": format!("/_tasks/{t}/_cancel") }))).await;
        e.call(Method::POST, "/v1/search/clusters/dst/request", Some(json!({ "method": "GET", "path": format!("/_tasks/{t}?wait_for_completion=true&timeout=10s") }))).await;
    }
    // The slots are free again (no reservation outlived its copy): the waiting index starts.
    let again = e.post("/v1/search/clusters/dst/migrate", json!({ "source": source, "indices": [{ "index": "c" }] })).await;
    assert!(again["indices"][0]["task"].is_string(), "{again}");
}
