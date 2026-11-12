//! A fleet of real search nodes behind the real proxy: one owner per cluster, failover when a
//! node is killed, a deposed owner fenced, a SIGTERM handoff under load, changes and deletes
//! made through one node reaching the others, API calls forwarded to the owner, a streamed
//! `_bulk` refused mid-upload and resent whole, nodes serving TLS with pinned boot-time
//! certificates, the fleet woken from zero through a fake ECS API, and a fleet from before
//! `api_tls` upgraded in place (proxies first).
//!
//! Every node runs on its own runtime and thread, so "killing" one stops all of its tasks at
//! once (lease renewals, heartbeats, listeners), as a crash would. Metadata is a SQLite file
//! shared by the nodes (`ROLL_SEARCH_TEST_PG` runs the same against Postgres); object storage
//! is a shared temp directory (or S3 with `ROLL_E2E_S3_BUCKET`).

use base64::Engine;
use search_control::{Control, FleetConfig, SearchConfig};
use search_proxy::proxy::{ListenOptions, Port};
use search_proxy::{Directory, Proxy, ProxyConfig};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Short enough to keep failover tests quick, long enough that a loaded machine with slow
/// fsyncs (SQLite commits and WAL uploads of hundreds of ms) doesn't expire leases under
/// healthy nodes: renewals run every `ttl / 3` and acknowledgements stop at `0.8 × ttl`.
const LEASE_TTL: Duration = Duration::from_secs(4);
/// When the proxy considers a node gone (heartbeats every 250 ms).
const NODE_TTL: Duration = Duration::from_secs(3);

fn tmp(tag: &str) -> PathBuf {
    tmp_in(std::env::temp_dir(), tag)
}

fn tmp_in(base: PathBuf, tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    base.join(format!("roll-search-fleet-{tag}-{}-{n}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

/// Where the shared SQLite metadata lives: memory-backed when the host has it. It stands in
/// for the fleet's Postgres, whose commits take milliseconds; on a busy disk an fsync can take
/// seconds, longer than the short lease TTL these tests use.
fn meta_dir(tag: &str) -> PathBuf {
    let shm = PathBuf::from("/dev/shm");
    tmp_in(if shm.is_dir() { shm } else { std::env::temp_dir() }, tag)
}

/// Fleet metadata for a test: a fresh SQLite file, or the Postgres of `ROLL_SEARCH_TEST_PG`
/// (wiped first: run those tests one at a time).
fn meta(dir: &std::path::Path) -> String {
    match std::env::var("ROLL_SEARCH_TEST_PG") {
        Ok(url) => url,
        Err(_) => dir.join("fleet.db").display().to_string(),
    }
}

/// A node on its own runtime.
struct TestNode {
    id: String,
    es: SocketAddr,
    api: SocketAddr,
    control: Arc<Control>,
    rt: Option<tokio::runtime::Runtime>,
    home: PathBuf,
}

impl TestNode {
    fn start(id: &str, meta: &str, objects: search_store::Objects, az: &str) -> TestNode {
        Self::start_with(id, meta, objects, az, false)
    }

    /// With `tls`, the REST and API listeners serve a certificate made for the run, and the
    /// heartbeat publishes its pin (rolld's `--search-only --search-tls-self-signed`).
    fn start_with(id: &str, meta: &str, objects: search_store::Objects, az: &str, tls: bool) -> TestNode {
        Self::start_tls(id, meta, objects, az, tls, tls)
    }

    /// `api_tls: false` with `tls`: a node from before `api_tls`, its REST port on TLS (pinned)
    /// and its control API plain, publishing no API pin.
    fn start_tls(id: &str, meta: &str, objects: search_store::Objects, az: &str, tls: bool, api_tls: bool) -> TestNode {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let home = tmp(id);
        let (id2, meta2, az2, home2) = (id.to_string(), meta.to_string(), az.to_string(), home.clone());
        let (control, es, api) = std::thread::spawn({
            let handle = rt.handle().clone();
            move || {
                handle.block_on(async move {
                    let es_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let api_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let (es, api) = (es_l.local_addr().unwrap(), api_l.local_addr().unwrap());
                    let mut cfg = SearchConfig::local(home2);
                    cfg.node_name = id2.clone();
                    cfg.http_port = es.port();
                    cfg.gc_interval = None;
                    cfg.idle_unload = Duration::from_secs(300);
                    let mut f = FleetConfig::new(meta2, id2, es.to_string());
                    f.advertise_api = Some(api.to_string());
                    f.az = Some(az2);
                    f.heartbeat = Duration::from_millis(250);
                    f.lease_ttl = LEASE_TTL;
                    f.follow = Duration::from_millis(100);
                    let mut opts = search_server::ListenOptions::default();
                    if tls {
                        let c = search_proxy::upstream::self_signed(&["localhost".into()]).unwrap();
                        opts.tls = Some(search_server::net::tls_acceptor(c.cert_pem.as_bytes(), c.key_pem.as_bytes()).unwrap());
                        f.tls_pin = Some(c.fingerprint);
                        f.api_tls = api_tls;
                        cfg.tls = true;
                    }
                    cfg.fleet = Some(f);
                    let control = Control::new(cfg, objects).await.unwrap();
                    let api_tls = opts.tls.clone().filter(|_| api_tls);
                    tokio::spawn(search_server::serve(es_l, control.node.clone(), opts));
                    let router = search_control::api::router(control.clone());
                    match api_tls {
                        Some(a) => {
                            let l = search_control::api::TlsListener::for_fleet(api_l, a, control.clone()).unwrap();
                            tokio::spawn(async move { axum::serve(l, router).await });
                        }
                        None => {
                            tokio::spawn(async move { axum::serve(api_l, router).await });
                        }
                    }
                    control.announce().await.unwrap();
                    (control, es, api)
                })
            }
        })
        .join()
        .unwrap();
        TestNode { id: id.to_string(), es, api, control, rt: Some(rt), home }
    }

    /// A crash: every task of the node stops at once; nothing is flushed or handed over.
    fn kill(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }

    /// SIGTERM: drain and hand over (runs on the node's own runtime).
    async fn terminate(&self) {
        let c = self.control.clone();
        let h = self.rt.as_ref().unwrap().handle().clone();
        h.spawn(async move { c.shutdown_within(Duration::from_secs(20)).await.unwrap() }).await.unwrap();
    }

    fn loaded(&self) -> Vec<String> {
        self.control.node.loaded()
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

struct Fleet {
    nodes: Vec<TestNode>,
    proxy: Arc<Proxy>,
    url: String,
    /// The proxy's control API port.
    api_url: String,
    http: reqwest::Client,
    /// Nodes serve TLS (their API too): direct calls use `https` and skip verification.
    tls: bool,
    dir: PathBuf,
    meta: String,
}

impl Drop for Fleet {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn wipe_pg(url: &str) {
    let pg = search_proxy::pg::Pg::connect(url, None, None).await.unwrap();
    let c = pg.client().await.unwrap();
    for t in ["roll_search_clusters", "roll_search_usage", "roll_search_storage", "roll_search_tombstones", "roll_search_nodes", "roll_search_leases"] {
        let _ = c.execute(&format!("DELETE FROM {t}"), &[]).await;
    }
}

async fn fleet(n: usize, waker: Option<Arc<dyn search_proxy::Waker>>) -> Fleet {
    fleet_with(n, waker, false).await
}

async fn fleet_with(n: usize, waker: Option<Arc<dyn search_proxy::Waker>>, tls: bool) -> Fleet {
    let dir = meta_dir("meta");
    std::fs::create_dir_all(&dir).unwrap();
    let meta = meta(&dir);
    if meta.starts_with("postgres") {
        wipe_pg(&meta).await;
    }
    let objects = search_store::test_objects("fleet");
    let nodes: Vec<TestNode> = (0..n).map(|i| TestNode::start_with(&format!("node-{i}"), &meta, objects.clone(), "az1", tls)).collect();
    fleet_of(nodes, dir, meta, waker, tls).await
}

/// The proxy (REST and API ports) in front of `nodes`.
async fn fleet_of(nodes: Vec<TestNode>, dir: PathBuf, meta: String, waker: Option<Arc<dyn search_proxy::Waker>>, tls: bool) -> Fleet {
    let members = search_proxy::fleet::open(&meta, None, None).await.unwrap();
    let cfg = ProxyConfig { az: Some("az1".into()), node_ttl: NODE_TTL, refresh: Duration::from_millis(100), hold_timeout: Duration::from_secs(30), ..ProxyConfig::default() };
    let d = Directory::new(members, cfg);
    d.refresh().await.unwrap();
    d.spawn_refresh();
    let proxy = Proxy::new(d, waker);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let al = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let api_url = format!("http://{}", al.local_addr().unwrap());
    let (tx, rx) = tokio::sync::watch::channel(false);
    std::mem::forget(tx);
    tokio::spawn(proxy.clone().serve(l, ListenOptions::plain(Port::Rest), rx.clone()));
    tokio::spawn(proxy.clone().serve(al, ListenOptions::plain(Port::Api), rx));
    let http = if tls { reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap() } else { reqwest::Client::new() };
    Fleet { nodes, proxy, url, api_url, http, tls, dir, meta }
}

impl Fleet {
    async fn api(&self, node: usize, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        // Per node: in a fleet being upgraded some API ports serve TLS and some don't.
        let scheme = if self.tls && self.proxy.directory.tls(&self.nodes[node].api) { "https" } else { "http" };
        let mut r = self.http.request(method, format!("{scheme}://{}{path}", self.nodes[node].api));
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    async fn create(&self, name: &str) -> (String, String) {
        let (s, v) = self.api(0, reqwest::Method::POST, "/v1/search/clusters", Some(json!({ "name": name }))).await;
        assert_eq!(s, 200, "{v}");
        (v["username"].as_str().unwrap().to_string(), v["password"].as_str().unwrap().to_string())
    }

    /// A request through the proxy.
    async fn es(&self, user: &str, pw: &str, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        es_at(&self.http, &self.url, user, pw, method, path, body).await
    }

    fn owners(&self, cluster: &str) -> Vec<String> {
        self.nodes.iter().filter(|n| n.rt.is_some() && n.loaded().contains(&cluster.to_string())).map(|n| n.id.clone()).collect()
    }

    fn index(&self, id: &str) -> usize {
        self.nodes.iter().position(|n| n.id == id).unwrap()
    }
}

async fn es_at(http: &reqwest::Client, base: &str, user: &str, pw: &str, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut r = http.request(method, format!("{base}{path}")).basic_auth(user, Some(pw));
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(Value::Null))
}

async fn count(f: &Fleet, user: &str, pw: &str) -> i64 {
    let (s, v) = f.es(user, pw, reqwest::Method::GET, "/docs/_count", None).await;
    assert_eq!(s, 200, "{v}");
    v["count"].as_i64().unwrap()
}

#[tokio::test]
async fn one_owner_per_cluster_through_the_proxy() {
    let f = fleet(3, None).await;
    let (u, p) = f.create("shop").await;
    for i in 0..5 {
        let (s, v) = f.es(&u, &p, reqwest::Method::PUT, &format!("/docs/_doc/{i}"), Some(json!({ "n": i }))).await;
        assert_eq!(s, 201, "{v}");
    }
    f.es(&u, &p, reqwest::Method::POST, "/docs/_refresh", None).await;
    assert_eq!(count(&f, &u, &p).await, 5);
    let owners = f.owners("shop");
    assert_eq!(owners.len(), 1, "{owners:?}");
    // Another node, asked directly, refuses and names the owner (what the proxy follows).
    let other = f.nodes.iter().find(|n| n.id != owners[0]).unwrap();
    let r = f.http.get(format!("http://{}/docs/_count", other.es)).basic_auth(&u, Some(&p)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 503);
    assert_eq!(r.headers()["x-roll-search-owner"].to_str().unwrap(), owners[0]);
    assert_eq!(f.owners("shop"), owners, "the refusal didn't load it there");
    // The API of any node shows where it runs.
    let (_, v) = f.api(f.index(&other.id), reqwest::Method::GET, "/v1/search/clusters/shop", None).await;
    assert_eq!((v["status"]["state"].as_str(), v["status"]["node"].as_str()), (Some("loaded"), Some(owners[0].as_str())), "{v}");
}

#[tokio::test]
async fn a_killed_owner_fails_over_without_losing_acknowledged_writes() {
    let mut f = fleet(2, None).await;
    let (u, p) = f.create("orders").await;
    let mut bulk = String::new();
    for i in 0..50 {
        bulk.push_str(&format!("{{\"index\":{{\"_index\":\"docs\",\"_id\":\"{i}\"}}}}\n{{\"n\":{i}}}\n"));
    }
    let r = f.http.post(format!("{}/_bulk", f.url)).basic_auth(&u, Some(&p)).header("content-type", "application/x-ndjson").body(bulk).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    // Not refreshed, not flushed: only the WAL (in object storage) has them.
    let owner = f.owners("orders").pop().unwrap();
    let i = f.index(&owner);
    f.nodes[i].kill();
    // The proxy holds the next request until the dead owner's lease expires, then the other
    // node opens the cluster and replays the WAL.
    let t = Instant::now();
    assert_eq!(count(&f, &u, &p).await, 50);
    assert!(t.elapsed() < Duration::from_secs(15), "failover took {:?}", t.elapsed());
    let survivor = &f.nodes[1 - i];
    assert_eq!(survivor.loaded(), ["orders"]);
    // Writes continue on the new owner.
    let (s, v) = f.es(&u, &p, reqwest::Method::PUT, "/docs/_doc/new?refresh=true", Some(json!({ "n": 99 }))).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(count(&f, &u, &p).await, 51);
}

#[tokio::test]
async fn a_deposed_owner_is_fenced() {
    let f = fleet(2, None).await;
    let (u, p) = f.create("fenced").await;
    let (s, _) = f.es(&u, &p, reqwest::Method::PUT, "/docs/_doc/1?refresh=true", Some(json!({ "v": 1 }))).await;
    assert_eq!(s, 201);
    let a = f.index(&f.owners("fenced")[0]);
    let b = 1 - a;
    // A's lease is taken from it behind its back (a partition, a store that lost a write):
    // the lease row expires and B wakes the cluster under a newer epoch.
    expire_lease(&f.meta, "fenced").await;
    let r = es_at(&f.http, &format!("http://{}", f.nodes[b].es), &u, &p, reqwest::Method::PUT, "/docs/_doc/2?refresh=true", Some(json!({ "v": 2 }))).await;
    assert_eq!(r.0, 201, "{}", r.1);
    // A still believes it owns the cluster until its next renewal; whatever it is asked to
    // write is refused, never acknowledged.
    let (s, v) = es_at(&f.http, &format!("http://{}", f.nodes[a].es), &u, &p, reqwest::Method::PUT, "/docs/_doc/3?refresh=true", Some(json!({ "v": 3 }))).await;
    assert_eq!(s, 503, "{v}");
    // A let go of it; B has 1 and 2, not 3.
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.nodes[a].loaded().contains(&"fenced".to_string()) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!f.nodes[a].loaded().contains(&"fenced".to_string()));
    let (s, v) = es_at(&f.http, &format!("http://{}", f.nodes[b].es), &u, &p, reqwest::Method::GET, "/docs/_search?q=*", None).await;
    assert_eq!(s, 200, "{v}");
    let mut ids: Vec<String> = v["hits"]["hits"].as_array().unwrap().iter().map(|h| h["_id"].as_str().unwrap().to_string()).collect();
    ids.sort();
    assert_eq!(ids, ["1", "2"]);
}

/// Expires a lease in the store, as if its holder had stopped renewing long ago.
async fn expire_lease(meta: &str, cluster: &str) {
    if meta.starts_with("postgres") {
        let pg = search_proxy::pg::Pg::connect(meta, None, None).await.unwrap();
        pg.client().await.unwrap().execute("UPDATE roll_search_leases SET expires = now() - interval '1 second' WHERE cluster = $1", &[&cluster]).await.unwrap();
    } else {
        let c = rusqlite::Connection::open(meta).unwrap();
        c.busy_timeout(Duration::from_secs(5)).unwrap();
        c.execute("UPDATE search_leases SET expires_ms = 0 WHERE cluster = ?1", [cluster]).unwrap();
    }
}

#[tokio::test]
async fn a_sigterm_hands_clusters_over_under_load() {
    let f = fleet(2, None).await;
    let (u, p) = f.create("busy").await;
    let (s, _) = f.es(&u, &p, reqwest::Method::PUT, "/docs", Some(json!({}))).await;
    assert_eq!(s, 200);
    let owner = f.index(&f.owners("busy")[0]);
    let acked = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let failed = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let (http, url, u, p, acked, failed, stop) = (f.http.clone(), f.url.clone(), u.clone(), p.clone(), acked.clone(), failed.clone(), stop.clone());
            tokio::spawn(async move {
                let mut i = 0;
                while !stop.load(Ordering::SeqCst) {
                    let (s, v) = es_at(&http, &url, &u, &p, reqwest::Method::PUT, &format!("/docs/_doc/{w}-{i}"), Some(json!({ "w": w, "i": i }))).await;
                    if s == 201 {
                        acked.lock().push(format!("{w}-{i}"));
                    } else {
                        failed.lock().push((s, v));
                    }
                    i += 1;
                }
            })
        })
        .collect();
    // Writes acknowledged before, and after, the handoff (counts, not sleeps: a loaded machine
    // with slow fsyncs acknowledges few writes per second).
    let acked_at_least = async |n: usize| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while acked.lock().len() < n && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    acked_at_least(8).await;
    // Deploy / Spot reclaim: the owner drains while writes keep coming.
    let t = Instant::now();
    f.nodes[owner].terminate().await;
    assert!(t.elapsed() < Duration::from_secs(15), "the drain took {:?}: it must not wait for a pause in the writes", t.elapsed());
    let before = acked.lock().len();
    acked_at_least(before + 8).await;
    stop.store(true, Ordering::SeqCst);
    for w in writers {
        w.await.unwrap();
    }
    assert!(failed.lock().is_empty(), "every write succeeded through the handoff: {:?}", failed.lock());
    let n = acked.lock().len();
    assert!(n > 10, "{n}");
    assert_eq!(f.nodes[1 - owner].loaded(), ["busy"]);
    f.es(&u, &p, reqwest::Method::POST, "/docs/_refresh", None).await;
    let (s, v) = f.es(&u, &p, reqwest::Method::GET, "/docs/_search?size=10000&_source=false", None).await;
    assert_eq!(s, 200, "{v}");
    let present: std::collections::HashSet<&str> = v["hits"]["hits"].as_array().unwrap().iter().map(|h| h["_id"].as_str().unwrap()).collect();
    let acked_ids = acked.lock().clone();
    let missing: Vec<&String> = acked_ids.iter().filter(|id| !present.contains(id.as_str())).collect();
    assert!(missing.is_empty(), "acknowledged writes lost across the handoff: {missing:?} of {n}");
    assert_eq!(count(&f, &u, &p).await, n as i64, "every acknowledged write is there");
    assert!(f.proxy.stats.retries.load(Ordering::SeqCst) + f.proxy.stats.redirects.load(Ordering::SeqCst) > 0);
}

/// An always-on cluster (`idle_timeout_s` 0) is loaded once created, by one node, and comes
/// back up on another node when its node stops, with no request asking for it.
#[tokio::test]
async fn always_on_clusters_move_when_their_node_stops() {
    let f = fleet(2, None).await;
    let (s, v) = f.api(0, reqwest::Method::POST, "/v1/search/clusters", Some(json!({ "name": "steady", "idle_timeout_s": 0 }))).await;
    assert_eq!(s, 200, "{v}");
    let (u, p) = (v["username"].as_str().unwrap().to_string(), v["password"].as_str().unwrap().to_string());
    assert_eq!(f.owners("steady"), ["node-0"]);
    let (s, v) = f.es(&u, &p, reqwest::Method::PUT, "/docs/_doc/1?refresh=true", Some(json!({ "n": 1 }))).await;
    assert_eq!(s, 201, "{v}");
    f.nodes[0].terminate().await;
    let deadline = Instant::now() + LEASE_TTL * 3 + Duration::from_secs(10);
    while f.nodes[1].loaded() != ["steady"] {
        assert!(Instant::now() < deadline, "never loaded on the other node");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(count(&f, &u, &p).await, 1);
}

#[tokio::test]
async fn changes_and_deletes_reach_the_owner() {
    let f = fleet(2, None).await;
    let (u, old) = f.create("keys").await;
    let (s, _) = f.es(&u, &old, reqwest::Method::GET, "/", None).await;
    assert_eq!(s, 200);
    let owner = f.index(&f.owners("keys")[0]);
    let other = 1 - owner;
    // Rotated through the node that doesn't hold the cluster: the owner applies it within a
    // follow period.
    let (s, v) = f.api(other, reqwest::Method::POST, "/v1/search/clusters/keys/password", Some(json!({}))).await;
    assert_eq!(s, 200, "{v}");
    let new = v["password"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (s, _) = f.es(&u, &old, reqwest::Method::GET, "/", None).await;
        if s == 401 {
            break;
        }
        assert!(Instant::now() < deadline, "the old password still works");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(f.es(&u, &new, reqwest::Method::GET, "/", None).await.0, 200);
    assert_eq!(f.owners("keys"), [f.nodes[owner].id.clone()], "applied live, no reload");
    // A console request through the other node's API is run by the owner.
    let (s, v) = f.api(other, reqwest::Method::POST, "/v1/search/clusters/keys/request", Some(json!({ "method": "GET", "path": "/" }))).await;
    assert_eq!((s, v["status"].as_u64()), (200, Some(200)), "{v}");
    assert_eq!(f.owners("keys"), [f.nodes[owner].id.clone()], "forwarded, not woken twice");
    // Deleted through the other node: the owner lets go, the data goes, the name is free.
    let (s, v) = f.api(other, reqwest::Method::DELETE, "/v1/search/clusters/keys", None).await;
    assert_eq!(s, 200, "{v}");
    assert!(f.owners("keys").is_empty());
    assert_eq!(f.es(&u, &new, reqwest::Method::GET, "/", None).await.0, 401);
    assert!(!f.nodes[0].control.data.cluster_exists("keys").await.unwrap());
    let (s, _) = f.api(owner, reqwest::Method::GET, "/v1/search/clusters/keys", None).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn branches_are_cut_on_the_parents_owner() {
    let f = fleet(2, None).await;
    let (u, p) = f.create("main").await;
    let (s, _) = f.es(&u, &p, reqwest::Method::PUT, "/docs/_doc/1", Some(json!({ "v": 1 }))).await;
    assert_eq!(s, 201);
    let other = 1 - f.index(&f.owners("main")[0]);
    // Unflushed on the owner: only cutting the branch there sees the write.
    let (s, v) = f.api(other, reqwest::Method::POST, "/v1/search/clusters/main/branches", Some(json!({ "name": "main-preview" }))).await;
    assert_eq!(s, 200, "{v}");
    let (bu, bp) = (v["username"].as_str().unwrap().to_string(), v["password"].as_str().unwrap().to_string());
    let (s, v) = f.es(&bu, &bp, reqwest::Method::GET, "/docs/_doc/1", None).await;
    assert_eq!((s, &v["_source"]), (200, &json!({ "v": 1 })), "{v}");
}

#[tokio::test]
async fn the_fleet_wakes_from_zero() {
    use http_body_util::BodyExt;
    let dir = meta_dir("zero");
    std::fs::create_dir_all(&dir).unwrap();
    let meta = meta(&dir);
    if meta.starts_with("postgres") {
        wipe_pg(&meta).await;
    }
    let objects = search_store::test_objects("fleet-zero");
    // The catalog exists while no node runs (it lives in the shared database).
    {
        let seed = TestNode::start("seed", &meta, objects.clone(), "az1");
        let r = reqwest::Client::new().post(format!("http://{}/v1/search/clusters", seed.api)).json(&json!({ "name": "cold", "idle_timeout_s": 1 })).send().await.unwrap();
        let v: Value = r.json().await.unwrap();
        std::fs::write(dir.join("pw"), v["password"].as_str().unwrap()).unwrap();
        seed.terminate().await;
    }
    let pw = std::fs::read_to_string(dir.join("pw")).unwrap();
    // A fake ECS: UpdateService starts a node.
    let started: Arc<parking_lot::Mutex<Vec<TestNode>>> = Arc::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ecs = l.local_addr().unwrap();
    {
        let (started, calls, meta, objects) = (started.clone(), calls.clone(), meta.clone(), objects.clone());
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = l.accept().await else { return };
                let (started, calls, meta, objects) = (started.clone(), calls.clone(), meta.clone(), objects.clone());
                tokio::spawn(async move {
                    let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                        let (started, calls, meta, objects) = (started.clone(), calls.clone(), meta.clone(), objects.clone());
                        async move {
                            let target = req.headers()["x-amz-target"].to_str().unwrap().to_string();
                            let _ = req.into_body().collect().await;
                            calls.fetch_add(1, Ordering::SeqCst);
                            let desired = started.lock().len();
                            let body = if target.ends_with("DescribeServices") {
                                json!({ "services": [{ "desiredCount": desired }] })
                            } else {
                                let node = tokio::task::spawn_blocking(move || TestNode::start("woken", &meta, objects, "az1")).await.unwrap();
                                started.lock().push(node);
                                json!({ "service": { "desiredCount": 1 } })
                            };
                            Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Full::new(bytes::Bytes::from(body.to_string()))))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(s), svc).await;
                });
            }
        });
    }
    let waker = search_proxy::EcsWaker::new("roll".into(), "search".into(), 1, "us-east-1".into())
        .with_endpoint(format!("http://{ecs}"))
        .with_credentials(search_proxy::wake::Credentials { key: "k".into(), secret: "s".into(), token: None });
    let members = search_proxy::fleet::open(&meta, None, None).await.unwrap();
    let d = Directory::new(members, ProxyConfig { node_ttl: NODE_TTL, refresh: Duration::from_millis(100), hold_timeout: Duration::from_secs(30), ..ProxyConfig::default() });
    d.refresh().await.unwrap();
    assert!(d.live().is_empty(), "nothing runs: {:?}", d.live());
    d.spawn_refresh();
    let proxy = Proxy::new(d, Some(Arc::new(waker)));
    let pl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", pl.local_addr().unwrap());
    let (_tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(proxy.clone().serve(pl, ListenOptions::plain(Port::Rest), rx));
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("cold:{pw}")));
    let r = reqwest::Client::new().get(format!("{url}/")).header("authorization", auth).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["cluster_name"], "cold");
    assert_eq!(started.lock().len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "describe + update");
    assert_eq!(proxy.stats.wakes.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A chunked `_bulk` that reaches a node which doesn't own the cluster is refused before that
/// node reads the body, while the client is still uploading; the proxy resends what it kept to
/// the owner and streams the rest there. Every document arrives once.
#[tokio::test]
async fn a_bulk_refused_mid_upload_is_resent_to_the_owner() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = fleet(2, None).await;
    let (u, p) = f.create("upload").await;
    // The owner is the node the proxy would *not* pick, so its first attempt is refused.
    let candidates: Vec<search_proxy::route::Candidate> = f.nodes.iter().map(|n| search_proxy::route::Candidate { id: &n.id, az: Some("az1") }).collect();
    let rendezvous = search_proxy::route::pick(&u, &candidates, Some("az1")).unwrap().to_string();
    let owner = 1 - f.index(&rendezvous);
    let (s, v) = es_at(&f.http, &format!("http://{}", f.nodes[owner].es), &u, &p, reqwest::Method::PUT, "/docs", Some(json!({}))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(f.owners("upload"), [f.nodes[owner].id.clone()]);
    let redirects = f.proxy.stats.redirects.load(Ordering::SeqCst);

    let mut s = tokio::net::TcpStream::connect(f.url.strip_prefix("http://").unwrap()).await.unwrap();
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
    let head = format!("POST /docs/_bulk?refresh=true HTTP/1.1\r\nhost: x\r\nauthorization: Basic {auth}\r\ntransfer-encoding: chunked\r\ncontent-type: application/x-ndjson\r\n\r\n");
    s.write_all(head.as_bytes()).await.unwrap();
    let chunk = |from: usize, to: usize| {
        let lines: String = (from..to).map(|i| format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"n\":{i}}}\n")).collect();
        format!("{:x}\r\n{lines}\r\n", lines.len())
    };
    s.write_all(chunk(0, 10).as_bytes()).await.unwrap();
    // The refusal comes back while the rest is still on its way.
    tokio::time::sleep(Duration::from_millis(500)).await;
    for c in 1..10 {
        s.write_all(chunk(c * 10, c * 10 + 10).as_bytes()).await.unwrap();
    }
    s.write_all(b"0\r\n\r\n").await.unwrap();
    let mut resp = Vec::new();
    let mut buf = [0u8; 8192];
    let body = loop {
        let n = tokio::time::timeout(Duration::from_secs(30), s.read(&mut buf)).await.unwrap().unwrap();
        assert!(n > 0, "{}", String::from_utf8_lossy(&resp));
        resp.extend_from_slice(&buf[..n]);
        let t = String::from_utf8_lossy(&resp).to_string();
        if let Some(end) = t.find("\r\n\r\n") {
            let len = t[..end].lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap();
            if resp.len() >= end + 4 + len {
                assert!(t.starts_with("HTTP/1.1 200"), "{t}");
                break serde_json::from_slice::<Value>(&resp[end + 4..end + 4 + len]).unwrap();
            }
        }
    };
    assert_eq!((body["errors"].as_bool(), body["items"].as_array().map(Vec::len)), (Some(false), Some(100)), "{body}");
    assert!(f.proxy.stats.redirects.load(Ordering::SeqCst) > redirects, "the first attempt went to the rendezvous node and was refused");
    assert_eq!(f.owners("upload"), [f.nodes[owner].id.clone()]);
    assert_eq!(count(&f, &u, &p).await, 100);
    assert_eq!(f.proxy.budget.used(), 0);
}

/// Nodes serving TLS with certificates made at boot are reached over TLS that accepts only the
/// certificate each one published: routing, an owner redirect and writes all work, API calls
/// through the proxy and forwarded between nodes go over TLS too, and the nodes really refuse
/// plain HTTP on both ports.
#[tokio::test]
async fn nodes_serving_tls_are_reached_through_their_pinned_certificates() {
    let f = fleet_with(2, None, true).await;
    for n in &f.nodes {
        assert!(f.proxy.directory.tls(&n.es) && f.proxy.directory.tls(&n.api), "{} published no pin", n.id);
        for addr in [n.es, n.api] {
            let plain = f.http.get(format!("http://{addr}/v1/search")).send().await;
            assert!(!matches!(&plain, Ok(r) if r.status().is_success()), "{plain:?}");
        }
    }
    let (u, p) = f.create("secure").await;
    // Opened on the node the proxy would not pick, so the proxy's first attempt is refused and
    // redirected (both hops over pinned TLS).
    let candidates: Vec<search_proxy::route::Candidate> = f.nodes.iter().map(|n| search_proxy::route::Candidate { id: &n.id, az: Some("az1") }).collect();
    let rendezvous = search_proxy::route::pick(&u, &candidates, Some("az1")).unwrap().to_string();
    let owner = 1 - f.index(&rendezvous);
    let (s, v) = es_at(&f.http, &format!("https://{}", f.nodes[owner].es), &u, &p, reqwest::Method::PUT, "/docs", Some(json!({}))).await;
    assert_eq!(s, 200, "{v}");
    let redirects = f.proxy.stats.redirects.load(Ordering::SeqCst);
    for i in 0..3 {
        let (s, v) = f.es(&u, &p, reqwest::Method::PUT, &format!("/docs/_doc/{i}?refresh=true"), Some(json!({ "n": i }))).await;
        assert_eq!(s, 201, "{v}");
    }
    assert!(f.proxy.stats.redirects.load(Ordering::SeqCst) > redirects);
    assert_eq!(count(&f, &u, &p).await, 3);
    assert_eq!(f.owners("secure"), [f.nodes[owner].id.clone()]);
    // A console request at the other node runs on the owner (forwarded over pinned TLS) …
    let (s, v) = f.api(1 - owner, reqwest::Method::POST, "/v1/search/clusters/secure/request", Some(json!({ "method": "GET", "path": "/docs/_count" }))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(serde_json::from_str::<Value>(v["body"].as_str().unwrap()).unwrap()["count"], 3, "{v}");
    // … and the proxy's API port reaches the owner's API over TLS.
    let r = f.http.get(format!("{}/v1/search/clusters/secure/indices", f.api_url)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert!(v.to_string().contains("docs"), "{v}");
    assert_eq!(f.owners("secure"), [f.nodes[owner].id.clone()], "nothing moved");
}

/// The node records' table as fleets before `api_tls` made it (the column is added in place).
async fn old_node_table(meta: &str) {
    if meta.starts_with("postgres") {
        wipe_pg(meta).await;
        let pg = search_proxy::pg::Pg::connect(meta, None, None).await.unwrap();
        pg.client()
            .await
            .unwrap()
            .batch_execute(
                "DROP TABLE IF EXISTS roll_search_nodes; CREATE TABLE roll_search_nodes (id TEXT PRIMARY KEY, http TEXT NOT NULL,
                 api TEXT, az TEXT, draining BOOLEAN NOT NULL, loaded INTEGER NOT NULL, beat TIMESTAMPTZ NOT NULL, tls TEXT)",
            )
            .await
            .unwrap();
    } else {
        rusqlite::Connection::open(meta)
            .unwrap()
            .execute_batch(
                "CREATE TABLE search_nodes (id TEXT PRIMARY KEY, http TEXT NOT NULL, api TEXT, az TEXT, draining INTEGER NOT NULL,
                 loaded INTEGER NOT NULL, beat_ms INTEGER NOT NULL, tls TEXT)",
            )
            .unwrap();
    }
}

/// A fleet from before `api_tls` (REST pinned, control API plain, node records without the
/// column) upgraded the documented way: the proxies first, then the nodes with a deploy's
/// overlap. The new proxy serves the old node (pinned REST, plain API); old and new nodes serve
/// side by side, a new node forwards console calls to an old one in plain HTTP and answers an
/// old one's plain calls on its TLS API (until no old node is left), and the old node's SIGTERM
/// hands its clusters to the new one without losing an acknowledged write.
#[tokio::test]
async fn a_fleet_from_before_api_tls_upgrades_proxies_first() {
    let dir = meta_dir("upgrade");
    std::fs::create_dir_all(&dir).unwrap();
    let meta = meta(&dir);
    old_node_table(&meta).await;
    let objects = search_store::test_objects("upgrade");
    let old = TestNode::start_tls("node-old", &meta, objects.clone(), "az1", true, false);
    // The proxies are replaced first: a new proxy in front of the old fleet.
    let mut f = fleet_of(vec![old], dir, meta.clone(), None, true).await;
    let (old_es, old_api) = (f.nodes[0].es, f.nodes[0].api);
    assert!(f.proxy.directory.tls(&old_es) && !f.proxy.directory.tls(&old_api), "the old node publishes a REST pin only");
    let (u, p) = f.create("legacy").await;
    for i in 0..5 {
        let (s, v) = f.es(&u, &p, reqwest::Method::PUT, &format!("/docs/_doc/{i}"), Some(json!({ "n": i }))).await;
        assert_eq!(s, 201, "{v}");
    }
    f.es(&u, &p, reqwest::Method::POST, "/docs/_refresh", None).await;
    assert_eq!(count(&f, &u, &p).await, 5);
    let r = f.http.get(format!("{}/v1/search/clusters/legacy/indices", f.api_url)).send().await.unwrap();
    assert_eq!(r.status(), 200, "the proxy's API port reaches the old node's plain API");

    // The node deploy: a new node joins before the old one stops.
    f.nodes.push(TestNode::start_tls("node-new", &meta, objects.clone(), "az1", true, true));
    let new_api = f.nodes[1].api;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.proxy.directory.tls(&new_api) {
        assert!(Instant::now() < deadline, "the proxy never saw the new node's API pin");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!f.proxy.directory.tls(&old_api), "the old node's record still says plain");
    // A console request at the new node runs on the old owner, forwarded in plain HTTP.
    let (s, v) = f.api(1, reqwest::Method::POST, "/v1/search/clusters/legacy/request", Some(json!({ "method": "GET", "path": "/docs/_count" }))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(serde_json::from_str::<Value>(v["body"].as_str().unwrap()).unwrap()["count"], 5, "{v}");
    // The old node calls the new one the only way it knows, in plain HTTP (a forwarded call):
    // the new node's TLS API answers it while an old node runs.
    let plain = reqwest::Client::new();
    let r = plain.get(format!("http://{new_api}/v1/search/clusters/legacy")).header("x-roll-search-forwarded", "1").send().await.unwrap();
    assert_eq!(r.status(), 200, "an old node's plain call to a new node");
    assert_eq!(r.json::<Value>().await.unwrap()["name"], "legacy");
    // Clusters created during the deploy work wherever they open.
    let (s, v) = f.api(1, reqwest::Method::POST, "/v1/search/clusters", Some(json!({ "name": "fresh" }))).await;
    assert_eq!(s, 200, "{v}");
    let (u2, p2) = (v["username"].as_str().unwrap().to_string(), v["password"].as_str().unwrap().to_string());
    let (s, v) = f.es(&u2, &p2, reqwest::Method::PUT, "/docs/_doc/a?refresh=true", Some(json!({ "n": 1 }))).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(count(&f, &u2, &p2).await, 1);
    for i in 5..10 {
        let (s, v) = f.es(&u, &p, reqwest::Method::PUT, &format!("/docs/_doc/{i}"), Some(json!({ "n": i }))).await;
        assert_eq!(s, 201, "{v}");
    }

    // The old node stops (SIGTERM): its clusters move to the new node, every write kept.
    f.nodes[0].terminate().await;
    for i in 10..15 {
        let (s, v) = f.es(&u, &p, reqwest::Method::PUT, &format!("/docs/_doc/{i}"), Some(json!({ "n": i }))).await;
        assert_eq!(s, 201, "{v}");
    }
    f.es(&u, &p, reqwest::Method::POST, "/docs/_refresh", None).await;
    assert_eq!(count(&f, &u, &p).await, 15);
    assert_eq!(count(&f, &u2, &p2).await, 1);
    assert_eq!(f.owners("legacy"), ["node-new"]);
    // Fully upgraded: the proxy reaches the API over pinned TLS.
    let r = f.http.get(format!("{}/v1/search/clusters/legacy/indices", f.api_url)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.json::<Value>().await.unwrap().to_string().contains("docs"));
    // With no old node left, the API answers TLS only again: a plain connection is closed.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let fresh = reqwest::Client::builder().pool_max_idle_per_host(0).build().unwrap();
        if fresh.get(format!("http://{new_api}/v1/search/clusters/legacy")).send().await.is_err() {
            break;
        }
        assert!(Instant::now() < deadline, "plain HTTP still answered after the old node left");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
