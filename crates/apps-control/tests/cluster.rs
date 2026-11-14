//! roll apps on several nodes in one process: each an `Apps` with its own HTTP server on a
//! random port (rolld's internal API as it mounts it: apps' API, `public::with_forwarded` and
//! `accept_forwarded`), sharing one SQLite store, one workspace and one `roll_cluster` Memory
//! store. Each node has a fake runtime of its own; a service's instances run on its owner.
//!
//! Covered: a visitor's request that reaches a node not owning the service is served by the
//! owner (the forward lands on the owner's internal API, which hands it to the public router),
//! and so is an API call through either node.

use apps_control::integrations::{NoResources, NoSecrets, NoTls, StaticDns};
use apps_control::runtime::fake::Fake;
use apps_control::store::{Sealer, Store};
use apps_control::{Apps, AppsConfig, Deps};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

struct Node {
    apps: Arc<Apps>,
    api: axum::Router,
    public: axum::Router,
    fake: Arc<Fake>,
    _home: tempfile::TempDir,
    server: tokio::task::AbortHandle,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct Fleet {
    nodes: Vec<Node>,
    project: String,
    _dir: tempfile::TempDir,
}

async fn fleet(n: usize) -> Fleet {
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
    let project = ws.create("", "acme", "", None).unwrap().id;
    let db = code_sql::Db::open_sqlite(&dir.path().join("apps.db"), code_sql::SqliteOptions { readers: 2, cache_kib: 1024 }).unwrap();
    let members = roll_cluster::MemoryStore::new();
    let mut nodes = Vec::new();
    for i in 0..n {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let cluster = roll_cluster::Cluster::join(Box::new(roll_cluster::Memory::new(members.clone())), roll_cluster::ClusterConfig::for_tests(format!("n{i}"), url, SECRET.to_vec())).unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut cfg = AppsConfig::new(home.path().to_path_buf());
        cfg.live.worker_grace = Duration::from_millis(20);
        cfg.live.probe_every = Duration::from_millis(2);
        cfg.live.drain = Duration::from_millis(50);
        cfg.allow_private = true;
        let fake = Fake::new();
        let deps = Deps {
            runtime: fake.clone(),
            builder: fake.clone(),
            sources: None,
            secrets: Arc::new(NoSecrets),
            resources: Arc::new(NoResources),
            resolver: Arc::new(StaticDns::default()),
            tls: Arc::new(NoTls),
            objects: None,
            cluster: cluster.clone(),
            workspace: Some(ws.clone()),
        };
        let apps = Apps::new(cfg, Store::new(db.clone(), Sealer::new("0123456789abcdef0123456789abcdef").unwrap()).unwrap(), deps);
        apps.start();
        let api = apps_control::api::router(apps.clone());
        let internal = apps_control::public::with_forwarded(api.clone(), apps.clone()).layer(axum::middleware::from_fn_with_state(cluster.clone(), roll_cluster::accept_forwarded));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, internal.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
        })
        .abort_handle();
        nodes.push(Node { public: apps_control::public::router(apps.clone()), api, apps, fake, _home: home, server });
    }
    Fleet { nodes, project, _dir: dir }
}

async fn call(r: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(path).header("content-type", "application/json");
    let req = req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty)).unwrap();
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn host(url: &Value) -> String {
    url.as_str().unwrap().trim_start_matches("http://").split(':').next().unwrap().to_string()
}

impl Fleet {
    /// The node that owns `service` now, and one that doesn't.
    fn owner_and_other(&self, service: &str) -> (&Node, &Node) {
        let key = apps_control::lease_key(service);
        let owner = self.nodes.iter().position(|n| n.apps.deps.cluster.owns(&key)).expect("an owner");
        (&self.nodes[owner], &self.nodes[(owner + 1) % self.nodes.len()])
    }

    /// An image service, deployed (asked through `via`).
    async fn image_service(&self, via: &Node, name: &str) -> (String, String) {
        let (st, svc) = call(&via.api, "POST", "/v1/apps/services", Some(json!({ "project": self.project, "name": name, "source": {"kind": "image", "image": "nginx:1.27"}, "run": {"port": 80} }))).await;
        assert_eq!(st, StatusCode::CREATED, "{svc}");
        let id = svc["id"].as_str().unwrap().to_string();
        let dep = svc["environments"][0]["latest"]["id"].as_str().unwrap().to_string();
        let path = format!("/v1/apps/services/{id}/environments/production/deploys/{dep}");
        for _ in 0..1000 {
            let (_, d) = call(&via.api, "GET", &path, None).await;
            if d["status"] == "active" {
                return (id, host(&svc["environments"][0]["url"]));
            }
            assert!(!["failed", "canceled"].contains(&d["status"].as_str().unwrap_or("")), "{d}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{name} didn't deploy");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_visitor_at_a_node_that_doesnt_own_the_service_gets_the_owners_answer() {
    let f = fleet(2).await;
    let (id, h) = f.image_service(&f.nodes[0], "web").await;
    let (owner, other) = f.owner_and_other(&id);
    // Through the node that doesn't own it: forwarded, served by the owner's instance.
    let req = Request::builder().uri("/hello").header("host", h.as_str()).body(Body::empty()).unwrap();
    let resp = other.public.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["path"], "/hello");
    let instance = body["instance"].as_str().unwrap();
    assert!(owner.fake.instances(&id).iter().any(|(n, ..)| n == instance), "the owner's instance answered");
    assert!(other.fake.instances(&id).is_empty(), "nothing ran on the other node");
    // The API through either node.
    for n in &f.nodes {
        let (st, env) = call(&n.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
        assert_eq!((st, env["active"].is_object()), (StatusCode::OK, true), "{env}");
    }
}

/// A static site's `service:` rewrite reaches the project's service where it lives: the site on
/// one node, the API on the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sites_rewrite_reaches_a_service_another_node_owns() {
    let f = fleet(2).await;
    let via = &f.nodes[0];
    // The site: an upload the fake builds into a static site whose `_redirects` sends
    // `/api/*` to `service:api/v1/:splat`.
    let (_, svc) = call(&via.api, "POST", "/v1/apps/services", Some(json!({ "project": f.project, "name": "site", "source": {"kind": "upload"} }))).await;
    let site = svc["id"].as_str().unwrap().to_string();
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("index.html"), "<h1>hi</h1>").unwrap();
    let mut bundle = Vec::new();
    roll_fetch::pack(&mut bundle, &Default::default(), Some(tree.path()), roll_fetch::Limits::default()).unwrap();
    let req = Request::post(format!("/v1/apps/services/{site}/environments/production/deploys")).header("content-type", "application/gzip").body(Body::from(bundle)).unwrap();
    let d: Value = serde_json::from_slice(&via.api.clone().oneshot(req).await.unwrap().into_body().collect().await.unwrap().to_bytes()).unwrap();
    let dep = d["id"].as_str().unwrap().to_string();
    let mut h = String::new();
    for _ in 0..1000 {
        let (_, d) = call(&via.api, "GET", &format!("/v1/apps/services/{site}/environments/production/deploys/{dep}"), None).await;
        if d["status"] == "active" {
            let (_, e) = call(&via.api, "GET", &format!("/v1/apps/services/{site}/environments/production"), None).await;
            h = host(&e["url"]);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!h.is_empty(), "the site didn't deploy");
    // The api on the node that doesn't own the site (made again until it is).
    let (site_owner, _) = f.owner_and_other(&site);
    let site_owner = site_owner.apps.deps.cluster.node_id().to_string();
    let mut api = None;
    for _ in 0..20 {
        let (id, _) = f.image_service(via, "api").await;
        let (o, _) = f.owner_and_other(&id);
        if o.apps.deps.cluster.node_id() != site_owner {
            api = Some(id);
            break;
        }
        assert_eq!(call(&via.api, "DELETE", &format!("/v1/apps/services/{id}"), None).await.0, StatusCode::NO_CONTENT);
    }
    let api = api.expect("an api owned by the other node");
    let (api_owner, _) = f.owner_and_other(&api);
    // Asked at either node: the site's owner rewrites, the api's owner answers.
    for n in &f.nodes {
        let req = Request::builder().uri("/api/users?page=2").header("host", h.as_str()).body(Body::empty()).unwrap();
        let resp = n.public.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let body: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null);
        assert_eq!((status, body["path"].as_str()), (StatusCode::OK, Some("/v1/users")), "{body}");
        assert!(api_owner.fake.instances(&api).iter().any(|(i, ..)| body["instance"] == *i), "the api's owner answered");
    }
}

/// A killed owner (its port closed, its membership gone without a goodbye): once its lease
/// expires the other node takes the service, wakes it there for the next visitor, and deploys it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_owners_service_moves_to_the_node_left() {
    let mut f = fleet(2).await;
    let (id, h) = f.image_service(&f.nodes[0], "web").await;
    let key = apps_control::lease_key(&id);
    let owner = f.nodes.iter().position(|n| n.apps.deps.cluster.owns(&key)).unwrap();
    let dead = f.nodes.remove(owner);
    dead.server.abort();
    dead.apps.deps.cluster.fault_kill();
    let left = &f.nodes[0];
    let mut owned = false;
    for _ in 0..300 {
        if left.apps.deps.cluster.owns(&key) {
            owned = true;
            break;
        }
        // A request routes the service, which takes its lease once the old one has expired.
        let _ = call(&left.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(owned, "the node left took the service");
    let mut ok = None;
    for _ in 0..100 {
        let req = Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap();
        let resp = left.public.clone().oneshot(req).await.unwrap();
        if resp.status() == StatusCode::OK {
            ok = Some(serde_json::from_slice::<Value>(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let body = ok.expect("a visitor gets the app on the node left");
    assert!(left.fake.instances(&id).iter().any(|(n, ..)| body["instance"] == *n), "woken there: {body}");
    // And it deploys there.
    let (st, d) = call(&left.api, "POST", &format!("/v1/apps/services/{id}/environments/production/redeploy"), Some(json!({}))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let path = format!("/v1/apps/services/{id}/environments/production/deploys/{}", d["id"].as_str().unwrap());
    let mut status = Value::Null;
    for _ in 0..1000 {
        status = call(&left.api, "GET", &path, None).await.1["status"].clone();
        if status == "active" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(status, "active");
    drop(dead);
}
