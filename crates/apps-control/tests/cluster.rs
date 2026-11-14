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
    /// Its internal API's address (`127.0.0.1:<port>`).
    addr: String,
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
    ws: Arc<roll_workspace::Workspace>,
    db: code_sql::Db,
    members: roll_cluster::MemoryStore,
    /// One runtime for every node (Fargate, one host's Docker): what a dead node ran keeps running.
    shared: Option<Arc<Fake>>,
    _dir: tempfile::TempDir,
}

async fn fleet(n: usize) -> Fleet {
    fleet_on(n, None).await
}

async fn fleet_on(n: usize, shared: Option<Arc<Fake>>) -> Fleet {
    if let Some(f) = &shared {
        f.shared.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
    let project = ws.create("", "acme", "", None).unwrap().id;
    let db = code_sql::Db::open_sqlite(&dir.path().join("apps.db"), code_sql::SqliteOptions { readers: 2, cache_kib: 1024 }).unwrap();
    let members = roll_cluster::MemoryStore::new();
    let mut f = Fleet { nodes: Vec::new(), project, ws, db, members, shared, _dir: dir };
    for i in 0..n {
        f.join(&format!("n{i}")).await;
    }
    f
}

impl Fleet {
    /// One more node: its own server, runtime and home, on the fleet's store and membership.
    async fn join(&mut self, id: &str) {
        let (ws, db, members) = (self.ws.clone(), self.db.clone(), self.members.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let url = format!("http://{addr}");
        let cluster = roll_cluster::Cluster::join(Box::new(roll_cluster::Memory::new(members)), roll_cluster::ClusterConfig::for_tests(id, url, SECRET.to_vec())).unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut cfg = AppsConfig::new(home.path().to_path_buf());
        cfg.live.worker_grace = Duration::from_millis(20);
        cfg.live.probe_every = Duration::from_millis(2);
        cfg.live.drain = Duration::from_millis(50);
        cfg.allow_private = true;
        let fake = self.shared.clone().unwrap_or_default();
        let deps = Deps {
            runtime: fake.clone(),
            builder: fake.clone(),
            sources: None,
            secrets: Arc::new(NoSecrets),
            resources: Arc::new(NoResources),
            resolver: Arc::new(StaticDns::default()),
            tls: Arc::new(NoTls),
            // One object store for every node (a site's files, logs): what a takeover reads.
            objects: Some(pg_storage::Objects::new(&pg_storage::StorageConfig::Local(self._dir.path().join("objects"))).unwrap()),
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
        self.nodes.push(Node { addr, public: apps_control::public::router(apps.clone()), api, apps, fake, _home: home, server });
    }
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

    /// A static site from an upload (the fake's built files), deployed (asked through `via`):
    /// its id and hostname.
    async fn static_site(&self, via: &Node) -> (String, String) {
        let (_, svc) = call(&via.api, "POST", "/v1/apps/services", Some(json!({ "project": self.project, "name": "site", "source": {"kind": "upload"} }))).await;
        let site = svc["id"].as_str().unwrap().to_string();
        let tree = tempfile::tempdir().unwrap();
        std::fs::write(tree.path().join("index.html"), "<h1>hi</h1>").unwrap();
        let mut bundle = Vec::new();
        roll_fetch::pack(&mut bundle, &Default::default(), Some(tree.path()), roll_fetch::Limits::default()).unwrap();
        let req = Request::post(format!("/v1/apps/services/{site}/environments/production/deploys")).header("content-type", "application/gzip").body(Body::from(bundle)).unwrap();
        let d: Value = serde_json::from_slice(&via.api.clone().oneshot(req).await.unwrap().into_body().collect().await.unwrap().to_bytes()).unwrap();
        let dep = d["id"].as_str().unwrap().to_string();
        for _ in 0..1000 {
            let (_, d) = call(&via.api, "GET", &format!("/v1/apps/services/{site}/environments/production/deploys/{dep}"), None).await;
            if d["status"] == "active" {
                let (_, e) = call(&via.api, "GET", &format!("/v1/apps/services/{site}/environments/production"), None).await;
                return (site, host(&e["url"]));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the site didn't deploy");
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
    let (site, h) = f.static_site(via).await;
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

/// A deploy in flight when its owner dies (rolling out: its new instance still booting) finishes
/// on the node left, from the store; the dead node (still running here, as a partitioned
/// process would) can't switch the environment live any more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deploy_in_flight_on_a_killed_owner_finishes_on_the_node_left() {
    let mut f = fleet(2).await;
    let (id, _) = f.image_service(&f.nodes[0], "web").await;
    let key = apps_control::lease_key(&id);
    let owner = f.nodes.iter().position(|n| n.apps.deps.cluster.owns(&key)).unwrap();
    let base = format!("/v1/apps/services/{id}/environments/production");
    // A slow start: the rollout waits two seconds for the new instance.
    call(&f.nodes[0].api, "PATCH", &format!("{base}/variables"), Some(json!({"set": {"FAKE_BOOT_MS": "2000"}}))).await;
    let (st, d) = call(&f.nodes[0].api, "POST", &format!("{base}/redeploy"), Some(json!({}))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let dep = d["id"].as_str().unwrap().to_string();
    for _ in 0..200 {
        if call(&f.nodes[0].api, "GET", &format!("{base}/deploys/{dep}"), None).await.1["status"] == "deploying" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let dead = f.nodes.remove(owner);
    dead.server.abort();
    dead.apps.deps.cluster.fault_kill();
    let left = &f.nodes[0];
    let mut status = Value::Null;
    for _ in 0..600 {
        status = call(&left.api, "GET", &format!("{base}/deploys/{dep}"), None).await.1["status"].clone();
        if status == "active" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(status, "active", "finished on the node left");
    assert!(left.apps.deps.cluster.owns(&key));
    // The live instances are the node left's: the dead one's rollout never took the switch.
    let (_, env) = call(&left.api, "GET", &base, None).await;
    assert_eq!(env["active"]["id"], dep.as_str(), "{env}");
    assert!(left.fake.instances(&id).iter().any(|(_, d, _)| *d == dep), "the new deployment's instance was made on the node left: {:?}", left.fake.instances(&id));
    drop(dead);
}

/// A node that joins takes services over (the cluster rebalances), and no visitor's request
/// fails meanwhile: a handoff moves a service with its requests still answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joining_node_takes_services_with_no_request_failing() {
    let mut f = fleet(1).await;
    // Placement hashes each service's key: with 12, the chance that none prefers the new node
    // is 1 in 4096 (with 4 it was 1 in 16, a flaky test).
    let mut svcs = Vec::new();
    for i in 0..12 {
        svcs.push(f.image_service(&f.nodes[0], &format!("web{i}")).await);
    }
    f.join("n1").await;
    let mut failed = Vec::new();
    let mut moved = false;
    for round in 0..600 {
        for (id, h) in &svcs {
            let req = Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap();
            let st = f.nodes[round % 2].public.clone().oneshot(req).await.unwrap().status();
            if st != StatusCode::OK {
                failed.push((round, id.clone(), st));
            }
        }
        moved = svcs.iter().any(|(id, _)| f.nodes[1].apps.deps.cluster.owns(&apps_control::lease_key(id)));
        if moved && round > 40 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(moved, "the new node took at least one service");
    assert!(failed.is_empty(), "requests failed during the handoff: {failed:?}");
}

/// A static site whose owner dies is served by the node that takes it over: its files' tar
/// from object storage, nothing built or run there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_static_sites_files_answer_on_the_node_that_takes_it_over() {
    let mut f = fleet(2).await;
    let (site, h) = f.static_site(&f.nodes[0]).await;
    let key = apps_control::lease_key(&site);
    let owner = f.nodes.iter().position(|n| n.apps.deps.cluster.owns(&key)).unwrap();
    let dead = f.nodes.remove(owner);
    dead.server.abort();
    dead.apps.deps.cluster.fault_kill();
    let left = &f.nodes[0];
    let builds = apps_control::runtime::fake::Fake::count(&left.fake.counts.builds);
    let mut page = None;
    for _ in 0..300 {
        let req = Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap();
        let resp = left.public.clone().oneshot(req).await.unwrap();
        if resp.status() == StatusCode::OK {
            page = Some(String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(page.as_deref(), Some("<!doctype html><title>fake site</title>"));
    assert!(left.apps.deps.cluster.owns(&key));
    assert_eq!(apps_control::runtime::fake::Fake::count(&left.fake.counts.builds), builds, "nothing built again");
    assert!(left.fake.instances(&site).is_empty(), "nothing runs: its files answer");
    drop(dead);
}

/// A tunnel (`roll apps proxy`: a WebSocket carrying one TCP connection) opened at a node that
/// doesn't own the service reaches the owner's instance: the upgrade is forwarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tunnel_through_a_node_that_doesnt_own_the_service_reaches_its_port() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let f = fleet(2).await;
    let (id, _) = f.image_service(&f.nodes[0], "web").await;
    let (owner, other) = f.owner_and_other(&id);
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/v1/apps/services/{id}/environments/production/tunnel", other.addr)).await.unwrap();
    ws.send(Message::Binary(b"GET /through HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_vec().into())).await.unwrap();
    let mut got = Vec::new();
    while let Some(Ok(m)) = ws.next().await {
        match m {
            Message::Binary(b) if b.is_empty() => break,
            Message::Binary(b) => got.extend_from_slice(&b),
            Message::Close(_) => break,
            _ => {}
        }
    }
    let text = String::from_utf8_lossy(&got);
    assert!(text.starts_with("HTTP/1.1 200") && text.contains(r#""path":"/through""#), "{text}");
    assert!(owner.fake.instances(&id).iter().any(|(n, ..)| text.contains(n.as_str())), "the owner's instance answered: {text}");
}

/// A cron service whose owner dies keeps its schedule on the node that takes it over, and its
/// runs run there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cron_service_runs_on_the_node_that_takes_it_over() {
    let mut f = fleet(2).await;
    let via = &f.nodes[0];
    let (st, svc) = call(&via.api, "POST", "/v1/apps/services", Some(json!({ "project": f.project, "name": "nightly", "source": {"kind": "image", "image": "busybox"}, "run": {"type": "cron", "schedule": "0 3 * * *"}, "variables": {"FAKE_EXIT": "0"} }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let base = format!("/v1/apps/services/{id}/environments/production");
    let dep = svc["environments"][0]["latest"]["id"].as_str().unwrap().to_string();
    for _ in 0..1000 {
        if call(&via.api, "GET", &format!("{base}/deploys/{dep}"), None).await.1["status"] == "active" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let key = apps_control::lease_key(&id);
    let owner = f.nodes.iter().position(|n| n.apps.deps.cluster.owns(&key)).unwrap();
    // A run the owner was in the middle of when it died.
    let (sid, d) = (id.clone(), dep.clone());
    let orphan = f.nodes[owner].apps.db(move |s| s.start_run(&apps_control::model::new_id("run"), &sid, "production", &d, 3_600_000)).await.unwrap();
    let dead = f.nodes.remove(owner);
    dead.server.abort();
    dead.apps.deps.cluster.fault_kill();
    let left = &f.nodes[0];
    let mut env = Value::Null;
    for _ in 0..300 {
        env = call(&left.api, "GET", &base, None).await.1;
        if left.apps.deps.cluster.owns(&key) && env["next_run_at"].is_i64() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(left.apps.deps.cluster.owns(&key) && env["next_run_at"].is_i64(), "its schedule moved: {env}");
    let made = apps_control::runtime::fake::Fake::count(&left.fake.counts.creates);
    let (st, run) = call(&left.api, "POST", &format!("{base}/runs"), None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{run}");
    let mut last = Value::Null;
    for _ in 0..500 {
        last = call(&left.api, "GET", &format!("{base}/runs"), None).await.1[0].clone();
        if last["status"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!((last["status"].as_str(), last["exit_code"].as_i64()), (Some("succeeded"), Some(0)), "{last}");
    assert!(apps_control::runtime::fake::Fake::count(&left.fake.counts.creates) > made, "the run's container was made on the node left");
    // The dead owner's run failed, said so, and didn't hold the schedule back (not skipped).
    let runs = call(&left.api, "GET", &format!("{base}/runs"), None).await.1;
    let o = runs.as_array().unwrap().iter().find(|r| r["id"] == orphan.id.as_str()).cloned().unwrap_or_default();
    assert_eq!(o["status"], "failed", "{runs}");
    let events = call(&left.api, "GET", "/v1/apps/events", None).await.1;
    assert!(events["events"].as_array().unwrap().iter().any(|e| e["kind"] == "cron.failed" && e["data"]["run"] == orphan.id.as_str()), "{events}");
    drop(dead);
}

/// Only a node reaches a service through another node's route for public traffic: a visitor
/// who calls it, or who puts roll's headers on a request another product forwards (an API
/// call), gets no app and no way past a private environment's access.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_visitor_cant_reach_a_service_through_the_internal_route() {
    let f = fleet(2).await;
    let (id, _) = f.image_service(&f.nodes[0], "web").await;
    let (owner, other) = f.owner_and_other(&id);
    let base = format!("/v1/apps/services/{id}/environments/production");
    assert_eq!(call(&owner.api, "PUT", &base, Some(json!({"access": {"token": "private-token-0123456789"}}))).await.0, StatusCode::OK);
    let target = format!("{id}/production");
    // Straight at the owner's route, unsigned.
    let direct = reqwest::Client::new().get(format!("http://{}/internal/apps/public", owner.addr)).header("x-roll-apps-target", &target).header("x-roll-apps-uri", "/").header("x-roll-via-site", "1").send().await.unwrap();
    assert_eq!(direct.status().as_u16(), 401, "only a node's signed request gets in");
    // On an API call another node forwards to the owner: answered by the API, not the app.
    // (Before 2026-11-14's fix the owner trusted these as plain headers on any forward.)
    let (st, env) = call_with(&other.api, &base, &[("x-roll-apps-target", &target), ("x-roll-apps-uri", "/"), ("x-roll-via-site", "1"), ("x-roll-apps-public", "1"), ("x-roll-rewrite-to", &target)]).await;
    assert_eq!(st, StatusCode::OK);
    assert!(env["name"] == "production" && env.get("instance").is_none(), "the API's answer: {env}");
}

async fn call_with(r: &axum::Router, path: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut req = Request::builder().uri(path);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = r.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    (status, serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null))
}

/// On a runtime the nodes share (Fargate, one host's Docker), the node that takes a dead owner's
/// service over keeps its instance running: the next visitor gets the same instance, nothing
/// made or started again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_keeps_the_instances_the_dead_owner_ran() {
    let shared = Fake::new();
    let mut f = fleet_on(2, Some(shared.clone())).await;
    let (id, h) = f.image_service(&f.nodes[0], "web").await;
    let visit = |n: &Node| {
        let (public, h) = (n.public.clone(), h.clone());
        async move {
            let resp = public.oneshot(Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap()).await.unwrap();
            (resp.status(), serde_json::from_slice::<Value>(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null))
        }
    };
    let (st, before) = visit(&f.nodes[0]).await;
    assert_eq!(st, StatusCode::OK, "{before}");
    let key = apps_control::lease_key(&id);
    let owner = f.nodes.iter().position(|n| n.apps.deps.cluster.owns(&key)).unwrap();
    let dead = f.nodes.remove(owner);
    dead.server.abort();
    dead.apps.deps.cluster.fault_kill();
    // Its loops stop too: a dead process tends nothing (here it would stop what it ran).
    dead.apps.shutdown().await;
    let (creates, starts) = (Fake::count(&shared.counts.creates), Fake::count(&shared.counts.starts));
    let left = &f.nodes[0];
    let mut after = Value::Null;
    for _ in 0..300 {
        let (st, body) = visit(left).await;
        if st == StatusCode::OK && left.apps.deps.cluster.owns(&key) {
            after = body;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(after["instance"], before["instance"], "the same instance answers: {after}");
    assert_eq!((Fake::count(&shared.counts.creates), Fake::count(&shared.counts.starts)), (creates, starts), "nothing made or started again: {:?}", shared.instances(&id));
    drop(dead);
}

/// On a runtime the nodes share, a handoff (a node joining) moves services with their
/// instances: the old owner lets them go, the new one keeps them, nothing new is made and no
/// request fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handoff_on_a_shared_runtime_keeps_the_instances() {
    let shared = Fake::new();
    let mut f = fleet_on(1, Some(shared.clone())).await;
    let mut svcs = Vec::new();
    for i in 0..12 {
        svcs.push(f.image_service(&f.nodes[0], &format!("web{i}")).await);
    }
    // Each awake (one instance each).
    for (_, h) in &svcs {
        let req = Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap();
        assert_eq!(f.nodes[0].public.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
    }
    let made = Fake::count(&shared.counts.creates);
    f.join("n1").await;
    let mut failed = Vec::new();
    let mut moved = Vec::new();
    for round in 0..600 {
        for (id, h) in &svcs {
            let req = Request::builder().uri("/").header("host", h.as_str()).body(Body::empty()).unwrap();
            let st = f.nodes[round % 2].public.clone().oneshot(req).await.unwrap().status();
            if st != StatusCode::OK {
                failed.push((round, id.clone(), st));
            }
        }
        moved = svcs.iter().filter(|(id, _)| f.nodes[1].apps.deps.cluster.owns(&apps_control::lease_key(id))).map(|(id, _)| id.clone()).collect();
        if !moved.is_empty() && round > 40 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!moved.is_empty(), "the new node took at least one service");
    assert!(failed.is_empty(), "requests failed during the handoff: {failed:?}");
    assert_eq!(Fake::count(&shared.counts.creates), made, "the moved services kept their instances: {:?}", moved.iter().map(|m| shared.instances(m)).collect::<Vec<_>>());
    // The node it left doesn't take them back (a request that reached it as the service moved).
    let id = &moved[0];
    assert!(f.nodes[0].apps.load_live(id, "production").await.is_err());
    let req = Request::builder().uri("/").header("host", svcs.iter().find(|(s, _)| s == id).unwrap().1.as_str()).body(Body::empty()).unwrap();
    assert_eq!(f.nodes[1].public.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
    assert_eq!(Fake::count(&shared.counts.creates), made, "the new owner still has its instance");
}
