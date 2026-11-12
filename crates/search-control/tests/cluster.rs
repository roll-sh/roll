//! roll search on a roll-cluster fleet, nodes in one process (docs/search/architecture.md, "On
//! the fleet"): one catalog, one object store, one membership backend. A cluster is served by
//! the holder of its lease; any node's REST front takes its requests; a drain hands it over with
//! every acknowledged write; an idle cluster's lease is given up.

use roll_cluster::{Cluster, ClusterConfig, Memory, MemoryStore};
use search_control::{Control, SearchConfig};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const SECRET: &str = "0123456789abcdef0123456789abcdef";

struct Node {
    control: Arc<Control>,
    cluster: Arc<Cluster>,
    api: String,
    rest: String,
    _home: tempfile::TempDir,
}

async fn node(id: &str, leases: &MemoryStore, objects: &search_store::Objects, catalog: &Arc<search_control::store::Store>) -> Node {
    let home = tempfile::tempdir().unwrap();
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", api_listener.local_addr().unwrap());
    let mut cc = ClusterConfig::for_tests(id, api.clone(), SECRET.as_bytes());
    // A cluster's unload (a flush) fits in a handoff.
    cc.handoff_timeout = Duration::from_secs(10);
    let backend: Box<dyn roll_cluster::Backend> = Box::new(Memory::new(leases.clone()));
    let cluster = tokio::task::spawn_blocking(move || Cluster::join(backend, cc)).await.unwrap().unwrap();
    let rest_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest = format!("http://{}", rest_listener.local_addr().unwrap());
    let mut cfg = SearchConfig::local(home.path().to_path_buf());
    cfg.node_name = id.into();
    cfg.http_port = rest_listener.local_addr().unwrap().port();
    cfg.sync_objects = true;
    cfg.gc_interval = None;
    let control = Control::with_catalog(cfg, objects.clone(), catalog.clone()).await.unwrap();
    control.join(cluster.clone()).unwrap();
    let router = search_control::api::router(control.clone())
        .merge(search_control::cluster::internal_router(control.clone(), cluster.clone()))
        .layer(axum::middleware::from_fn_with_state(cluster.clone(), roll_cluster::accept_forwarded));
    tokio::spawn(async move { axum::serve(api_listener, router).await });
    let front = search_control::cluster::rest_router(control.clone());
    tokio::spawn(async move { axum::serve(rest_listener, front).await });
    Node { control, cluster, api, rest, _home: home }
}

async fn call(http: &reqwest::Client, method: reqwest::Method, url: String, auth: Option<(&str, &str)>, body: Option<Value>) -> (u16, Value) {
    let mut r = http.request(method, url);
    if let Some((u, p)) = auth {
        r = r.basic_auth(u, Some(p));
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    let s = r.status().as_u16();
    let t = r.text().await.unwrap();
    (s, serde_json::from_str(&t).unwrap_or(Value::String(t)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cluster_runs_on_its_owner_and_moves_on_a_drain() {
    let leases = MemoryStore::new();
    let objects = search_store::test_objects("fleet");
    let catalog = Arc::new(search_control::store::Store::memory().unwrap());
    let a = node("a", &leases, &objects, &catalog).await;
    let b = node("b", &leases, &objects, &catalog).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while a.cluster.nodes().len() != 2 || b.cluster.nodes().len() != 2 {
        assert!(tokio::time::Instant::now() < deadline, "the nodes never saw each other");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let http = reqwest::Client::new();
    let (s, c) = call(&http, reqwest::Method::POST, format!("{}/v1/search/clusters", a.api), None, Some(json!({"name": "shop"}))).await;
    assert_eq!(s, 200, "{c}");
    let pw = c["password"].as_str().unwrap().to_string();
    let auth = Some(("shop", pw.as_str()));

    // Writes and reads through either node's front land on the one owner.
    for (i, n) in [&a, &b, &a, &b].iter().enumerate() {
        let (s, v) = call(&http, reqwest::Method::PUT, format!("{}/items/_doc/{i}?refresh=true", n.rest), auth, Some(json!({"n": i}))).await;
        assert_eq!(s, 201, "{v}");
    }
    let key = search_control::cluster::key("shop");
    let owners: Vec<&Node> = [&a, &b].into_iter().filter(|n| n.cluster.owns(&key)).collect();
    assert_eq!(owners.len(), 1, "one owner");
    let (owner, other) = if a.cluster.owns(&key) { (&a, &b) } else { (&b, &a) };
    assert!(owner.control.node.loaded().iter().any(|n| &**n == "shop"), "loaded on its owner");
    assert!(other.control.node.loaded().is_empty(), "and nowhere else");
    let (s, v) = call(&http, reqwest::Method::GET, format!("{}/items/_count", other.rest), auth, None).await;
    assert_eq!((s, v["count"].as_i64()), (200, Some(4)), "{v}");
    // A wrong password is refused on the owner, wherever it came in.
    let (s, _) = call(&http, reqwest::Method::GET, format!("{}/items/_count", other.rest), Some(("shop", "nope")), None).await;
    assert_eq!(s, 401);

    // The owner drains: the other node takes the cluster with every acknowledged document.
    owner.cluster.drain(Duration::from_secs(20)).await;
    let (s, v) = call(&http, reqwest::Method::GET, format!("{}/items/_count", other.rest), auth, None).await;
    assert_eq!((s, v["count"].as_i64()), (200, Some(4)), "{v}");
    assert!(other.cluster.owns(&key), "the other node owns it now");
    assert!(owner.control.node.loaded().is_empty(), "the drained node unloaded it");

    // Unloaded, the cluster belongs to nobody.
    let (s, v) = call(&http, reqwest::Method::POST, format!("{}/v1/search/clusters/shop/unload", other.api), None, Some(json!({}))).await;
    assert_eq!(s, 200, "{v}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while other.cluster.owns(&key) {
        assert!(tokio::time::Instant::now() < deadline, "the idle lease was never given up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The next request loads it again, under a newer epoch, with everything in it.
    let (s, v) = call(&http, reqwest::Method::GET, format!("{}/items/_count", other.rest), auth, None).await;
    assert_eq!((s, v["count"].as_i64()), (200, Some(4)), "{v}");
}
