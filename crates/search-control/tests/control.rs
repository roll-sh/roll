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
    let router = search_control::api::router(control.clone());
    tokio::spawn(async move { axum::serve(api_listener, router).await });
    Env { control, api, es: format!("http://{es_addr}"), http: reqwest::Client::new(), home, objects, cfg }
}

async fn env() -> Env {
    env_with(|_| {}).await
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

    let shown = e.get(&format!("/v1/search/clusters/{id}")).await;
    assert_eq!(shown["status"]["state"], "loaded");
    assert_eq!(shown["status"]["indices"], 1);

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
    // Every request the node admitted: 6 from clients (also across the unload), the indices
    // listing and 5 console requests. Not counted: wrong passwords, the 413, the console's own
    // 400s.
    assert_eq!(u["requests"], 12, "the node's request counts are metered: {u}");
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
    // No request: the transform's next trigger loads its cluster again.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while !e.control.node.loaded().iter().any(|n| &**n == "jobs") {
        assert!(tokio::time::Instant::now() < deadline, "the due transform never woke its cluster");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!e.control.node.loaded().iter().any(|n| &**n == "idle"), "a cluster without jobs stays asleep");
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
