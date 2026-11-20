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
    node_with(id, leases, objects, catalog, |_| {}).await
}

/// [`node`] with `f` applied to its config.
async fn node_with(id: &str, leases: &MemoryStore, objects: &search_store::Objects, catalog: &Arc<search_control::store::Store>, f: impl FnOnce(&mut SearchConfig)) -> Node {
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
    f(&mut cfg);
    let control = Control::with_catalog(cfg, objects.clone(), catalog.clone()).await.unwrap();
    control.join(cluster.clone()).unwrap();
    let router = search_control::api::router(control.clone())
        .merge(search_control::cluster::internal_router(control.clone(), cluster.clone()))
        .layer(axum::middleware::from_fn_with_state(cluster.clone(), roll_cluster::accept_forwarded));
    tokio::spawn(async move { axum::serve(api_listener, router).await });
    // As rolld serves a fleet node's REST port; `x-test-client` from loopback (this test's
    // "load balancer") names the client.
    let opts = search_server::ListenOptions { client: search_server::ClientAddresses::new(Some("x-test-client"), Some("127.0.0.0/8")).unwrap(), ..Default::default() };
    let front = control.clone();
    tokio::spawn(async move { search_control::cluster::serve_front(rest_listener, front, &opts, std::future::pending()).await });
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
    let created = std::time::Instant::now();
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
    // A list on the other node shows where it's loaded, from the catalog.
    let (_, list) = call(&http, reqwest::Method::GET, format!("{}/v1/search/clusters", other.api), None, None).await;
    let shop = list.as_array().unwrap().iter().find(|c| c["name"] == "shop").unwrap().clone();
    assert_eq!((shop["status"]["state"].as_str(), shop["status"]["node"].as_str()), (Some("loaded"), Some(owner.cluster.node_id())), "{shop}");
    // A wrong password is refused on the owner, wherever it came in.
    let (s, _) = call(&http, reqwest::Method::GET, format!("{}/items/_count", other.rest), Some(("shop", "nope")), None).await;
    assert_eq!(s, 401);
    // An app's link made through the other node (apps' glue runs wherever the service does): its
    // key is made on the owner, works, and stops working once revoked.
    let cred = other.control.mint_link("shop", "svc_1").await.unwrap();
    assert!(other.control.node.loaded().is_empty(), "made on the owner");
    let api_key = cred.vars.iter().find(|(k, _)| k == "ELASTICSEARCH_API_KEY").map(|(_, v)| format!("ApiKey {v}")).unwrap();
    let count = || http.get(format!("{}/items/_count", other.rest)).header("Authorization", &api_key).send();
    assert_eq!(count().await.unwrap().status(), 200);
    other.control.revoke_link("shop", &cred.kept).await.unwrap();
    assert_eq!(count().await.unwrap().status(), 401, "revoked");

    // The owner drains: the other node takes the cluster with every acknowledged document.
    owner.cluster.drain(Duration::from_secs(20)).await;
    let (s, v) = call(&http, reqwest::Method::GET, format!("{}/items/_count", other.rest), auth, None).await;
    assert_eq!((s, v["count"].as_i64()), (200, Some(4)), "{v}");
    assert!(other.cluster.owns(&key), "the other node owns it now");
    assert!(owner.control.node.loaded().is_empty(), "the drained node unloaded it");

    // Unloaded, the cluster belongs to nobody.
    let (s, v) = call(&http, reqwest::Method::POST, format!("{}/v1/search/clusters/shop/unload", other.api), None, Some(json!({}))).await;
    assert_eq!(s, 200, "{v}");
    // Each owner metered its own stay, and the two never overlapped: the loaded seconds billed
    // across both nodes fit in the time the cluster existed (each stay rounds up a second).
    let id = catalog.by_name("shop").await.unwrap().unwrap().id;
    let billed = catalog.usage_since(&id, 0, i64::MAX / 2).await.unwrap().loaded_seconds;
    assert!(billed > 0 && billed <= created.elapsed().as_secs() as i64 + 2, "{billed} s billed in {:?}", created.elapsed());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while other.cluster.owns(&key) {
        assert!(tokio::time::Instant::now() < deadline, "the idle lease was never given up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The next request loads it again, under a newer epoch, with everything in it.
    let (s, v) = call(&http, reqwest::Method::GET, format!("{}/items/_count", other.rest), auth, None).await;
    assert_eq!((s, v["count"].as_i64()), (200, Some(4)), "{v}");
    // Its request metrics followed it: each node hands what it recorded to the shared catalog.
    a.control.sample().await.unwrap();
    b.control.sample().await.unwrap();
    let (s, m) = call(&http, reqwest::Method::GET, format!("{}/v1/search/clusters/shop/metrics?range=1h", owner.api), None, None).await;
    assert_eq!((s, m["totals"]["index"]["requests"].as_u64()), (200, Some(4)), "{m}");
    assert!(m["totals"]["search"]["requests"].as_u64().unwrap_or(0) >= 3, "{m}");
}

/// Every cluster on the fleet has roll's default repository: a snapshot lifecycle policy takes
/// a snapshot into it with no setup, the snapshot restores on whichever node owns the cluster
/// next, and the repository can't be unregistered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_repository_snapshots_and_restores_on_the_fleet() {
    let leases = MemoryStore::new();
    let objects = search_store::test_objects("fleet-snapshots");
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
    for i in 0..3 {
        let (s, v) = call(&http, reqwest::Method::PUT, format!("{}/items/_doc/{i}?refresh=true", a.rest), auth, Some(json!({"n": i}))).await;
        assert_eq!(s, 201, "{v}");
    }
    let (s, v) = call(&http, reqwest::Method::GET, format!("{}/_snapshot", b.rest), auth, None).await;
    assert_eq!((s, v["found-snapshots"]["type"].as_str()), (200, Some("s3")), "{v}");

    let policy = json!({"schedule": "0 30 1 * * ?", "name": "<nightly-{now/d}>", "repository": "found-snapshots", "config": {"indices": ["items"]}});
    let (s, v) = call(&http, reqwest::Method::PUT, format!("{}/_slm/policy/nightly", b.rest), auth, Some(policy)).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = call(&http, reqwest::Method::PUT, format!("{}/_slm/policy/nightly/_execute", a.rest), auth, None).await;
    assert_eq!(s, 200, "{v}");
    let name = v["snapshot_name"].as_str().unwrap().to_string();
    let (s, v) = call(&http, reqwest::Method::GET, format!("{}/_snapshot/found-snapshots/{name}", a.rest), auth, None).await;
    assert_eq!((s, v["snapshots"][0]["state"].as_str(), v["snapshots"][0]["metadata"]["policy"].as_str()), (200, Some("SUCCESS"), Some("nightly")), "{v}");

    // The owner drains; the next owner restores the deleted index from the snapshot.
    let key = search_control::cluster::key("shop");
    let (owner, other) = if a.cluster.owns(&key) { (&a, &b) } else { (&b, &a) };
    owner.cluster.drain(Duration::from_secs(20)).await;
    let (s, v) = call(&http, reqwest::Method::DELETE, format!("{}/items", other.rest), auth, None).await;
    assert_eq!(s, 200, "{v}");
    assert!(other.cluster.owns(&key), "the other node owns it now");
    let (s, v) = call(&http, reqwest::Method::POST, format!("{}/_snapshot/found-snapshots/{name}/_restore?wait_for_completion=true", owner.rest), auth, Some(json!({}))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = call(&http, reqwest::Method::GET, format!("{}/items/_count", owner.rest), auth, None).await;
    assert_eq!((s, v["count"].as_i64()), (200, Some(3)), "{v}");

    let (s, v) = call(&http, reqwest::Method::DELETE, format!("{}/_snapshot/found-snapshots", owner.rest), auth, None).await;
    assert_eq!((s, v["error"]["type"].as_str()), (400, Some("illegal_argument_exception")), "{v}");
}

/// On a fleet a cluster's CORS settings travel with it: whichever node a browser's preflight
/// reaches, it is answered the same way (on the cluster's owner, as every request is), also right
/// after a change made through another node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_node_answers_a_preflight_the_same_way() {
    let leases = MemoryStore::new();
    let objects = search_store::test_objects("fleet-cors");
    let catalog = Arc::new(search_control::store::Store::memory().unwrap());
    let domain = |c: &mut SearchConfig| c.domain = Some("search.test".into());
    let a = node_with("a", &leases, &objects, &catalog, domain).await;
    let b = node_with("b", &leases, &objects, &catalog, domain).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while a.cluster.nodes().len() != 2 || b.cluster.nodes().len() != 2 {
        assert!(tokio::time::Instant::now() < deadline, "the nodes never saw each other");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let http = reqwest::Client::new();
    let (s, c) = call(&http, reqwest::Method::POST, format!("{}/v1/search/clusters", a.api), None, Some(json!({"name": "web", "cors": {"allow_origin": ["https://shop.example"]}}))).await;
    assert_eq!(s, 200, "{c}");
    let pw = c["password"].as_str().unwrap().to_string();
    let r = http.get(format!("{}/", a.rest)).header("host", "web.search.test").basic_auth("elastic", Some(&pw)).send().await.unwrap();
    assert_eq!(r.status(), 200, "loaded on its owner");
    let preflight = |base: &str, origin: &str| {
        http.request(reqwest::Method::OPTIONS, format!("{base}/products/_search"))
            .header("host", "web.search.test")
            .header("origin", origin)
            .header("access-control-request-method", "POST")
            .send()
    };
    let allowed = |r: &reqwest::Response| r.headers().get("access-control-allow-origin").map(|v| v.to_str().unwrap().to_string());
    for node in [&a, &b] {
        let r = preflight(&node.rest, "https://shop.example").await.unwrap();
        assert_eq!((r.status().as_u16(), allowed(&r).as_deref()), (200, Some("https://shop.example")), "through {}", node.api);
    }
    // Changed through the other node: every node answers with the new origins at once.
    let (s, v) = call(&http, reqwest::Method::PATCH, format!("{}/v1/search/clusters/web", b.api), None, Some(json!({"cors": {"allow_origin": ["https://new.example"]}}))).await;
    assert_eq!(s, 200, "{v}");
    for node in [&a, &b] {
        let r = preflight(&node.rest, "https://shop.example").await.unwrap();
        assert_eq!((r.status().as_u16(), allowed(&r)), (403, None), "through {}", node.api);
        let r = preflight(&node.rest, "https://new.example").await.unwrap();
        assert_eq!(r.status().as_u16(), 200, "through {}", node.api);
    }
}

/// A cluster's IP allowlist is checked on its owner against the client's own address, which the
/// node the client reached carries in the signed forward (not the forwarding node's address),
/// and the internal route the forwards arrive on takes signed requests only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forward_carries_the_clients_address_to_the_owner() {
    let leases = MemoryStore::new();
    let objects = search_store::test_objects("fleet-allow");
    let catalog = Arc::new(search_control::store::Store::memory().unwrap());
    let a = node("a", &leases, &objects, &catalog).await;
    let b = node("b", &leases, &objects, &catalog).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while a.cluster.nodes().len() != 2 || b.cluster.nodes().len() != 2 {
        assert!(tokio::time::Instant::now() < deadline, "the nodes never saw each other");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let http = reqwest::Client::new();
    let (s, c) = call(&http, reqwest::Method::POST, format!("{}/v1/search/clusters", a.api), None, Some(json!({"name": "vault", "ip_allowlist": ["203.0.113.0/24"]}))).await;
    assert_eq!(s, 200, "{c}");
    let pw = c["password"].as_str().unwrap().to_string();
    let get = |base: &str, client: Option<&str>| {
        let mut r = http.get(format!("{base}/")).basic_auth("vault", Some(&pw));
        if let Some(ip) = client {
            r = r.header("x-test-client", ip);
        }
        r.send()
    };
    // Loaded on whichever node takes it, from an allowed client.
    assert_eq!(get(&a.rest, Some("203.0.113.5")).await.unwrap().status(), 200);
    let key = search_control::cluster::key("vault");
    let (owner, other) = if a.cluster.owns(&key) { (&a, &b) } else { (&b, &a) };
    // Through the other node: the owner sees the client, not the node that forwarded it (127.0.0.1).
    assert_eq!(get(&other.rest, Some("203.0.113.5")).await.unwrap().status(), 200);
    let r = get(&other.rest, Some("198.51.100.1")).await.unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(r.text().await.unwrap(), r#"{"ok":false,"message":"Forbidden due to traffic filtering"}"#);
    assert_eq!(get(&other.rest, None).await.unwrap().status(), 403, "the forwarding node's own address isn't on the list");
    assert!(other.control.node.loaded().is_empty(), "checked on the owner");
    // roll's own request through the other node (a link's key, made on the owner) isn't filtered.
    other.control.mint_link("vault", "svc_1").await.unwrap();

    // The internal route takes only signed forwards: a client can't name its own address there,
    // nor reach an open cluster unsigned.
    let (s, c) = call(&http, reqwest::Method::POST, format!("{}/v1/search/clusters", a.api), None, Some(json!({"name": "open"}))).await;
    assert_eq!(s, 200, "{c}");
    let open_pw = c["password"].as_str().unwrap().to_string();
    for (cluster, p) in [("open", open_pw.as_str()), ("vault", pw.as_str())] {
        let r = http.get(format!("{}/internal/search/rest/", owner.api)).basic_auth(cluster, Some(p)).send().await.unwrap();
        assert_eq!(r.status(), 401, "{cluster}");
        assert!(r.text().await.unwrap().contains("signature"), "refused as unsigned, not by the cluster");
    }
    let r = http.get(format!("{}/internal/search/rest/", owner.api)).basic_auth("vault", Some(&pw)).header("x-roll-code-client", "203.0.113.5").send().await.unwrap();
    assert_eq!(r.status(), 401, "a client's own x-roll-code-client");
}

/// A linked remote owned by another node is searched on that node through the signed forward:
/// roll's own request, which the remote's IP allowlist doesn't filter, and which loads the
/// remote only on its owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_on_another_node_is_searched_through_the_signed_forward() {
    let leases = MemoryStore::new();
    let objects = search_store::test_objects("fleet-ccs");
    let catalog = Arc::new(search_control::store::Store::memory().unwrap());
    let a = node("a", &leases, &objects, &catalog).await;
    let b = node("b", &leases, &objects, &catalog).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while a.cluster.nodes().len() != 2 || b.cluster.nodes().len() != 2 {
        assert!(tokio::time::Instant::now() < deadline, "the nodes never saw each other");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let http = reqwest::Client::new();
    let owner_of = |name: &str| if a.cluster.owns(&search_control::cluster::key(name)) { "a" } else { "b" };
    let (s, c) = call(&http, reqwest::Method::POST, format!("{}/v1/search/clusters", a.api), None, Some(json!({"name": "near"}))).await;
    assert_eq!(s, 200, "{c}");
    let near_pw = c["password"].as_str().unwrap().to_string();
    assert_eq!(call(&http, reqwest::Method::GET, format!("{}/", a.rest), Some(("near", &near_pw)), None).await.0, 200);
    // A remote that lands on the other node, with an allowlist no client is on.
    let mut far = None;
    for i in 0..16 {
        let name = format!("far-{i}");
        let (s, c) = call(&http, reqwest::Method::POST, format!("{}/v1/search/clusters", a.api), None, Some(json!({"name": name, "ip_allowlist": ["203.0.113.0/24"]}))).await;
        assert_eq!(s, 200, "{c}");
        let pw = c["password"].as_str().unwrap().to_string();
        let (s, v) = call(&http, reqwest::Method::PUT, format!("{}/items/_doc/1?refresh=true", a.rest), Some((&name, &pw)), Some(json!({"n": i}))).await;
        assert_eq!(s, 403, "filtered: {v}");
        // Written as roll itself (the console), which loads it on its owner.
        let (s, v) = call(&http, reqwest::Method::POST, format!("{}/v1/search/clusters/{name}/request", a.api), None, Some(json!({"method": "PUT", "path": "/items/_doc/1?refresh=true", "body": {"n": i}}))).await;
        assert_eq!(s, 200, "{v}");
        if owner_of(&name) != owner_of("near") {
            far = Some(name);
            break;
        }
    }
    let far = far.expect("some cluster lands on the other node");
    let (s, v) = call(&http, reqwest::Method::PUT, format!("{}/v1/search/clusters/near/remotes/far", b.api), None, Some(json!({"cluster": far}))).await;
    assert_eq!(s, 200, "{v}");
    let (near_node, far_node) = if owner_of("near") == "a" { (&a, &b) } else { (&b, &a) };
    // Through either front: the remote part runs on the remote's owner, never loaded elsewhere.
    for front in [&a, &b] {
        let (s, v) = call(&http, reqwest::Method::GET, format!("{}/far:items/_search", front.rest), Some(("near", &near_pw)), None).await;
        assert_eq!((s, v["hits"]["total"]["value"].as_i64(), v["_clusters"]["details"]["far"]["status"].as_str()), (200, Some(1), Some("successful")), "{v}");
        assert_eq!(v["hits"]["hits"][0]["_index"], "far:items", "{v}");
    }
    assert!(!near_node.control.node.loaded().iter().any(|n| **n == *far), "the remote isn't loaded on the local cluster's node");
    assert!(far_node.control.node.loaded().iter().any(|n| **n == *far), "it runs on its owner");
}

/// A scroll and a point in time whose parts live on two owner nodes: the local cluster's on
/// its owner, the remote's on the other, each reached through the signed forward; pages go
/// through either front.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scroll_over_clusters_on_two_nodes_pages_through_either_front() {
    let leases = MemoryStore::new();
    let objects = search_store::test_objects("fleet-ccs-scroll");
    let catalog = Arc::new(search_control::store::Store::memory().unwrap());
    let a = node("a", &leases, &objects, &catalog).await;
    let b = node("b", &leases, &objects, &catalog).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while a.cluster.nodes().len() != 2 || b.cluster.nodes().len() != 2 {
        assert!(tokio::time::Instant::now() < deadline, "the nodes never saw each other");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let http = reqwest::Client::new();
    let owner_of = |name: &str| if a.cluster.owns(&search_control::cluster::key(name)) { "a" } else { "b" };
    let (s, c) = call(&http, reqwest::Method::POST, format!("{}/v1/search/clusters", a.api), None, Some(json!({"name": "home"}))).await;
    assert_eq!(s, 200, "{c}");
    let pw = c["password"].as_str().unwrap().to_string();
    let index = |name: String, pw: String, ns: Vec<i64>| {
        let http = http.clone();
        let rest = a.rest.clone();
        async move {
            for n in ns {
                let (s, v) = call(&http, reqwest::Method::PUT, format!("{rest}/items/_doc/{n}?refresh=true"), Some((&name, &pw)), Some(json!({"n": n}))).await;
                assert_eq!(s, 201, "{v}");
            }
        }
    };
    index("home".into(), pw.clone(), vec![1, 3, 5, 7]).await;
    let mut away = None;
    for i in 0..16 {
        let name = format!("away-{i}");
        let (s, c) = call(&http, reqwest::Method::POST, format!("{}/v1/search/clusters", a.api), None, Some(json!({"name": name}))).await;
        assert_eq!(s, 200, "{c}");
        // Writing loads it on its owner.
        index(name.clone(), c["password"].as_str().unwrap().to_string(), vec![0, 2, 4, 6, 8]).await;
        if owner_of(&name) != owner_of("home") {
            away = Some(name);
            break;
        }
    }
    let away = away.expect("some cluster lands on the other node");
    let (s, v) = call(&http, reqwest::Method::PUT, format!("{}/v1/search/clusters/home/remotes/away", b.api), None, Some(json!({"cluster": away}))).await;
    assert_eq!(s, 200, "{v}");
    let fronts = [&a, &b];
    let (s, v) = call(&http, reqwest::Method::POST, format!("{}/items,away:items/_search?scroll=1m", a.rest), Some(("home", &pw)), Some(json!({"size": 3, "sort": [{"n": "asc"}]}))).await;
    assert_eq!(s, 200, "{v}");
    let id = v["_scroll_id"].as_str().unwrap().to_string();
    let mut seen: Vec<i64> = v["hits"]["hits"].as_array().unwrap().iter().map(|h| h["_source"]["n"].as_i64().unwrap()).collect();
    for i in 0.. {
        let (s, v) = call(&http, reqwest::Method::POST, format!("{}/_search/scroll", fronts[i % 2].rest), Some(("home", &pw)), Some(json!({"scroll_id": id, "scroll": "1m"}))).await;
        assert_eq!(s, 200, "{v}");
        let hits = v["hits"]["hits"].as_array().unwrap();
        if hits.is_empty() {
            break;
        }
        seen.extend(hits.iter().map(|h| h["_source"]["n"].as_i64().unwrap()));
    }
    assert_eq!(seen, (0..9).collect::<Vec<i64>>());
    let (s, v) = call(&http, reqwest::Method::DELETE, format!("{}/_search/scroll", b.rest), Some(("home", &pw)), Some(json!({"scroll_id": id}))).await;
    assert_eq!(s, 200, "{v}");
    // A point in time over both, searched and closed through the other front.
    let (s, v) = call(&http, reqwest::Method::POST, format!("{}/items,away:items/_pit?keep_alive=1m", a.rest), Some(("home", &pw)), None).await;
    assert_eq!(s, 200, "{v}");
    let pit = v["id"].as_str().unwrap().to_string();
    let (s, v) = call(&http, reqwest::Method::POST, format!("{}/_search", b.rest), Some(("home", &pw)), Some(json!({"pit": {"id": pit}, "size": 20, "sort": [{"n": "desc"}]}))).await;
    let ns: Vec<i64> = v["hits"]["hits"].as_array().map(|a| a.iter().map(|h| h["_source"]["n"].as_i64().unwrap()).collect()).unwrap_or_default();
    assert_eq!((s, ns), (200, (0..9).rev().collect::<Vec<i64>>()), "{v}");
    let (s, v) = call(&http, reqwest::Method::DELETE, format!("{}/_pit", b.rest), Some(("home", &pw)), Some(json!({"id": pit}))).await;
    assert_eq!((s, v["succeeded"].as_bool()), (200, Some(true)), "{v}");
    let (home_node, away_node) = if owner_of("home") == "a" { (&a, &b) } else { (&b, &a) };
    assert!(!home_node.control.node.loaded().iter().any(|n| **n == *away), "the remote's parts run only on its owner");
    assert!(away_node.control.node.loaded().iter().any(|n| **n == *away));
}
