//! roll code as a cluster (docs/code/cluster.md): three nodes in one process, each a `Code` with
//! its own HTTP server on a random port, sharing one database pair and one object directory.
//! With `ROLL_CODE_TEST_DATABASE` the databases are two fresh Postgres schemas and every node
//! has pools of its own, as separate processes would; without it, one SQLite pair shared by
//! every node. Cluster timings are `ClusterConfig::for_tests` (twice as long on Postgres).
//!
//! Covered: every kind of repo request through every node runs on the repo's owner (API, git
//! push and clone, event streams, MCP); concurrent landings through every node land exactly
//! once; a killed owner's repos move after its leases expire with nothing acknowledged lost, its
//! merge-queue batch landing and its running review run again; a joining node takes repos over
//! with no request failing; draining moves everything off a node; a rename keeps the owner; an
//! event stream ends when its repo moves; the health check.

mod common;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use code_cluster::ClusterConfig;
use code_control::model::{Change, Check, CheckStatus, RepoRow};
use code_control::{Code, Config, DatabaseConfig, Databases, Runner};
use common::{git, git_raw, write};
use parking_lot::Mutex;
use pg_storage::{Objects, StorageConfig};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::task::AbortHandle;

const SECRET: &[u8] = b"cluster test secret, 32 bytes!!!";

// ── the cluster under test ──

enum Backend {
    Sqlite { store: code_sql::Db, control: code_sql::Db },
    Postgres { url: String, store: String, control: String },
}

struct Env {
    dir: tempfile::TempDir,
    db: Backend,
}

impl Env {
    fn new(name: &str) -> Env {
        // Warnings (slow renewals, fenced leases, failed forwards) show up for failing tests.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "code_cluster=info,code_control=warn".into()))
            .with_test_writer()
            .try_init();
        let dir = tempfile::tempdir().unwrap();
        let db = match std::env::var(code_sql::TEST_DATABASE_ENV) {
            Ok(url) if !url.is_empty() => {
                let s = format!("{:x}", rand::random::<u64>());
                Backend::Postgres { url, store: format!("t_{s}_{name}_st"), control: format!("t_{s}_{name}_ct") }
            }
            _ => {
                let open = |f: &str| code_sql::Db::open_sqlite(&dir.path().join(f), code_sql::SqliteOptions::default()).unwrap();
                Backend::Sqlite { store: open("code.db"), control: open("control.db") }
            }
        };
        Env { dir, db }
    }

    /// Statements on a shared Postgres can take a good part of a second: twice the timings there.
    fn scale(&self) -> u32 {
        if matches!(self.db, Backend::Postgres { .. }) { 2 } else { 1 }
    }

    fn cluster_config(&self, id: &str, url: &str) -> ClusterConfig {
        let mut c = ClusterConfig::for_tests(id, url, SECRET);
        let k = self.scale();
        for d in [
            &mut c.heartbeat,
            &mut c.node_ttl,
            &mut c.lease_ttl,
            &mut c.renew_every,
            &mut c.lease_margin,
            &mut c.sweep_every,
            &mut c.rebalance_every,
            &mut c.handoff_timeout,
        ] {
            *d *= k;
        }
        c
    }

    /// How long a takeover can take: the dead node's lease expires, then a sweep takes it.
    fn takeover(&self) -> Duration {
        Duration::from_millis(1500 * u64::from(self.scale())) + Duration::from_secs(10)
    }
}


struct Node {
    id: String,
    code: Arc<Code>,
    url: String,
    _home: tempfile::TempDir,
    accept: AbortHandle,
    conns: Arc<Mutex<Vec<AbortHandle>>>,
    dead: AtomicBool,
}

impl Node {
    fn cluster(&self) -> &Arc<code_cluster::Cluster> {
        self.code.cluster().expect("a cluster node")
    }

    fn owns(&self, repo_id: &str) -> bool {
        !self.dead.load(Ordering::SeqCst) && self.cluster().owns(repo_id)
    }

    fn served(&self) -> u64 {
        self.code.repo_requests_served()
    }

    /// Die like a killed process: the port closes, open connections drop, and the cluster
    /// membership stops without a goodbye (no `Lost`, no release: its leases expire).
    fn kill(&self) {
        self.dead.store(true, Ordering::SeqCst);
        self.accept.abort();
        for c in self.conns.lock().drain(..) {
            c.abort();
        }
        self.cluster().fault_kill();
    }
}

async fn start_node(env: &Env, id: &str, f: &(dyn Fn(&mut Config) + Send + Sync)) -> Node {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let home = tempfile::tempdir().unwrap();
    let mut config = Config::local(home.path().to_path_buf());
    config.cluster = Some(env.cluster_config(id, &url));
    config.cluster_single_host = true;
    config.review.index = false;
    f(&mut config);
    let objects = Objects::new(&StorageConfig::Local(env.dir.path().join("objects"))).unwrap();
    let code = match &env.db {
        Backend::Sqlite { store, control } => {
            Code::open_on(config, objects, Databases { store: store.clone(), control: control.clone(), cluster: None }).unwrap()
        }
        Backend::Postgres { url, store, control } => {
            let mut d = DatabaseConfig::new(url.clone());
            d.connections = 8;
            d.store_schema = store.clone();
            d.control_schema = control.clone();
            config.database = Some(d);
            Code::open(config, objects).unwrap()
        }
    };
    let router = code_control::api::router(code.clone());
    // A server of our own (not axum::serve) so a kill can drop every open connection.
    let conns: Arc<Mutex<Vec<AbortHandle>>> = Arc::default();
    let c2 = conns.clone();
    let accept = tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else { continue };
            let svc = router.clone().layer(axum::Extension(axum::extract::ConnectInfo(peer)));
            let svc = hyper_util::service::TowerToHyperService::new(svc);
            let io = hyper_util::rt::TokioIo::new(stream);
            let t = tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).with_upgrades().await;
            });
            let mut c = c2.lock();
            c.retain(|h| !h.is_finished());
            c.push(t.abort_handle());
        }
    })
    .abort_handle();
    Node { id: id.into(), code, url, _home: home, accept, conns, dead: AtomicBool::new(false) }
}

struct Net {
    env: Env,
    nodes: Vec<Node>,
    http: reqwest::Client,
    config: Arc<dyn Fn(&mut Config) + Send + Sync>,
}

const IDS: [&str; 4] = ["n-a", "n-b", "n-c", "n-d"];

async fn net(name: &str, n: usize) -> Net {
    net_with(name, n, Arc::new(|_: &mut Config| {})).await
}

async fn net_with(name: &str, n: usize, config: Arc<dyn Fn(&mut Config) + Send + Sync>) -> Net {
    let env = Env::new(name);
    let mut nodes = Vec::new();
    for id in &IDS[..n] {
        nodes.push(start_node(&env, id, &*config).await);
    }
    let http = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let net = Net { env, nodes, http, config };
    net.converge().await;
    net
}

impl Net {
    fn alive(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|n| !n.dead.load(Ordering::SeqCst))
    }

    /// Wait until every live node sees every live node.
    async fn converge(&self) {
        let n = self.alive().count();
        until("the nodes see each other", Duration::from_secs(20), || self.alive().all(|x| x.cluster().nodes().len() == n)).await;
    }

    async fn add_node(&mut self) {
        let id = IDS[self.nodes.len()];
        let node = start_node(&self.env, id, &*self.config).await;
        self.nodes.push(node);
        self.converge().await;
    }

    /// Stop node `i` (it leaves the cluster) and start a new process in its place, with the
    /// same id and its own fresh state directory.
    async fn restart(&mut self, i: usize) {
        let old = &self.nodes[i];
        old.dead.store(true, Ordering::SeqCst);
        old.accept.abort();
        for c in old.conns.lock().drain(..) {
            c.abort();
        }
        let c = old.cluster().clone();
        tokio::task::spawn_blocking(move || c.leave()).await.unwrap();
        let id = old.id.clone();
        self.nodes[i] = start_node(&self.env, &id, &*self.config).await;
        self.converge().await;
    }

    /// The node that owns `repo_id` now.
    fn owner(&self, repo_id: &str) -> Option<usize> {
        let owners: Vec<usize> = (0..self.nodes.len()).filter(|i| self.nodes[*i].owns(repo_id)).collect();
        assert!(owners.len() <= 1, "{repo_id} is owned by {owners:?}");
        owners.first().copied()
    }

    async fn req(&self, node: usize, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        req(&self.http, &self.nodes[node].url, method, path, body).await
    }

    async fn ok(&self, node: usize, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.req(node, method, path, body).await;
        assert!(s.is_success(), "{} {method} {path}: {s} {v}", self.nodes[node].id);
        v
    }

    /// `acme/{name}` with one landing (through `node`); its id.
    async fn repo(&self, node: usize, name: &str) -> String {
        let (s, v) = self.req(node, "POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
        assert!(s.is_success() || s == StatusCode::CONFLICT, "{s} {v}");
        let r = self.ok(node, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": name }))).await;
        let files = json!({ "README.md": { "content": format!("# {name}\n") } });
        self.ok(node, "POST", &format!("/v1/code/repos/acme/{name}/land"), Some(json!({ "message": "init", "files": files }))).await;
        r["id"].as_str().unwrap().to_string()
    }

    /// Subjects on `repo`'s main, oldest first.
    async fn log(&self, node: usize, repo: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let q = cursor.as_ref().map(|c| format!("&cursor={c}")).unwrap_or_default();
            let v = self.ok(node, "GET", &format!("/v1/code/repos/acme/{repo}/log?rev=main&limit=500{q}"), None).await;
            for item in v["items"].as_array().unwrap() {
                out.push(item["message"].as_str().unwrap().lines().next().unwrap().to_string());
            }
            match v["cursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }
        out.reverse();
        out
    }
}

async fn req(http: &reqwest::Client, base: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = http.request(method.parse().unwrap(), format!("{base}{path}")).timeout(Duration::from_secs(60));
    if let Some(v) = body {
        b = b.header("content-type", "application/json").body(v.to_string());
    }
    match b.send().await {
        Ok(r) => {
            let s = StatusCode::from_u16(r.status().as_u16()).unwrap();
            let bytes = r.bytes().await.unwrap_or_default();
            let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())) };
            (s, v)
        }
        Err(e) => (StatusCode::BAD_GATEWAY, json!({ "error": format!("client: {e}") })),
    }
}

async fn until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn land_body(message: &str, path: &str) -> Value {
    json!({ "message": message, "files": { path: { "content": format!("{message}\n") } } })
}

/// Every acknowledged subject is on main exactly once (and nothing is on it twice).
fn assert_exactly_once(log: &[String], acked: &[String]) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for m in log {
        *counts.entry(m.as_str()).or_default() += 1;
    }
    for (m, n) in &counts {
        assert!(*n == 1 || *m == "init", "{m} is on main {n} times");
    }
    for m in acked {
        assert_eq!(counts.get(m.as_str()).copied(), Some(1), "acknowledged landing {m:?} isn't on main once: {log:?}");
    }
}

// ── routing ──

/// Every kind of request for one repo, sent to every node, runs on the repo's owner: API reads
/// and landings, changes, threads and work items by id, git push and clone through other
/// nodes, an event stream through a node that doesn't own the repo (with an event emitted on yet
/// another node), and MCP tool calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn every_request_for_a_repo_runs_on_its_owner() {
    let net = net("routing", 3).await;
    let repo_id = net.repo(0, "api").await;
    let o = net.owner(&repo_id).expect("someone owns it after a landing");
    let others: Vec<usize> = (0..3).filter(|i| *i != o).collect();
    let (p, q) = (others[0], others[1]);
    let before: Vec<u64> = net.nodes.iter().map(Node::served).collect();
    let base = "/v1/code/repos/acme/api";

    for i in 0..3 {
        net.ok(i, "POST", &format!("{base}/land"), Some(land_body(&format!("via {i}"), &format!("n{i}.txt")))).await;
        assert_eq!(net.ok(i, "GET", base, None).await["name"], "api");
        let tree = net.ok(i, "GET", &format!("{base}/tree/main/"), None).await;
        assert!(tree["entries"].as_array().unwrap().len() >= 2, "{tree}");
        let (s, _) = net.req(i, "GET", &format!("{base}/blob/main/n0.txt"), None).await;
        assert_eq!(s, StatusCode::OK);
    }
    // Changes, threads and work items by id, each step through another node.
    let c = net.ok(p, "POST", &format!("{base}/changes"), Some(json!({ "title": "c", "message": "c", "files": { "c.txt": { "content": "c\n" } } }))).await;
    let cid = c["id"].as_str().unwrap();
    assert_eq!(net.ok(q, "GET", &format!("/v1/code/changes/{cid}"), None).await["id"], cid);
    let th = net.ok(q, "POST", &format!("/v1/code/changes/{cid}/threads"), Some(json!({ "path": "c.txt", "line": 1, "body": "looks fine" }))).await;
    let tid = th["id"].as_str().unwrap();
    assert_eq!(net.ok(p, "GET", &format!("/v1/code/threads/{tid}"), None).await["id"], tid);
    let w = net.ok(q, "POST", &format!("{base}/work"), Some(json!({ "title": "w" }))).await;
    let wid = w["id"].as_str().unwrap();
    assert_eq!(net.ok(p, "GET", &format!("/v1/code/work/{wid}"), None).await["id"], wid);
    let landed = net.ok(p, "POST", &format!("/v1/code/changes/{cid}/land"), None).await;
    assert!(landed.to_string().contains("landed"), "{landed}");

    // git: push through one node that doesn't own the repo, clone through the other.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let out = git_raw(home, home, &["clone", &format!("{}/git/acme/api.git", net.nodes[p].url), "a"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let wa = home.join("a");
    write(&wa, "pushed.txt", "from git\n");
    git(home, &wa, &["add", "-A"]);
    git(home, &wa, &["commit", "-qm", "pushed through a node that doesn't own the repo"]);
    git(home, &wa, &["push", "-q", "origin", "HEAD:main"]);
    let head = git(home, &wa, &["rev-parse", "HEAD"]).trim().to_string();
    git(home, home, &["clone", "-q", &format!("{}/git/acme/api.git", net.nodes[q].url), "b"]);
    assert_eq!(std::fs::read_to_string(home.join("b/pushed.txt")).unwrap(), "from git\n");
    assert_eq!(git(home, &home.join("b"), &["rev-parse", "HEAD"]).trim(), head, "the same commit through every node");
    assert_eq!(net.ok(o, "GET", base, None).await["head_git"], head);

    // An event stream through a node that doesn't own the repo.
    let mut stream = net.http.get(format!("{}{base}/events", net.nodes[p].url)).send().await.unwrap();
    if stream.status().as_u16() != 200 {
        let s = stream.status();
        panic!("{s}: {}", stream.text().await.unwrap_or_default());
    }
    let seen = Arc::new(Mutex::new(String::new()));
    let s2 = seen.clone();
    let reader = tokio::spawn(async move {
        while let Ok(Some(chunk)) = stream.chunk().await {
            s2.lock().push_str(&String::from_utf8_lossy(&chunk));
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    net.ok(q, "POST", &format!("{base}/land"), Some(land_body("while streaming", "s.txt"))).await;
    // Emitted on a node that doesn't own the repo: it reaches the owner's feed.
    net.nodes[q].code.emit(&repo_id, "work", json!({ "id": "wi_from_elsewhere" }));
    until("the stream sees the landing and the forwarded event", Duration::from_secs(10), || {
        let s = seen.lock();
        s.contains("while streaming") && s.contains("wi_from_elsewhere")
    })
    .await;
    reader.abort();

    // MCP on a node that doesn't own the repo: its tool calls run on the owner.
    let mcp = |sid: Option<String>, body: Value| {
        let mut b = net.http.post(format!("{}/v1/code/mcp", net.nodes[q].url)).header("content-type", "application/json").header("accept", "application/json");
        if let Some(s) = sid {
            b = b.header("mcp-session-id", s);
        }
        b.body(body.to_string()).send()
    };
    let init = mcp(None, json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } }))
        .await
        .unwrap();
    let sid = init.headers()["mcp-session-id"].to_str().unwrap().to_string();
    mcp(Some(sid.clone()), json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await.unwrap();
    let call = |id: u64, name: &str, args: Value| json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } });
    let r: Value = serde_json::from_slice(&mcp(Some(sid.clone()), call(1, "read_file", json!({ "repo": "acme/api", "path": "pushed.txt" }))).await.unwrap().bytes().await.unwrap()).unwrap();
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("from git"), "{r}");
    let r: Value = serde_json::from_slice(
        &mcp(Some(sid), call(2, "land", json!({ "repo": "acme/api", "message": "via mcp", "files": { "m.txt": { "content": "m\n" } } }))).await.unwrap().bytes().await.unwrap(),
    )
    .unwrap();
    assert!(r["result"]["isError"] != true, "{r}");

    // Everything ran on the owner; the other nodes only forwarded.
    let after: Vec<u64> = net.nodes.iter().map(Node::served).collect();
    for i in &others {
        assert_eq!(after[*i], before[*i], "{} ran repo requests it doesn't own", net.nodes[*i].id);
    }
    assert!(after[o] >= before[o] + 20, "the owner ran them: {before:?} → {after:?}");
    assert_eq!(net.owner(&repo_id), Some(o), "the owner never changed");
    let log = net.log(p, "api").await;
    assert_exactly_once(&log, &["via 0".into(), "via 1".into(), "via 2".into(), "while streaming".into(), "via mcp".into()]);
}

/// 64 landings at once, spread over every node: each acknowledged one is on main exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_landings_through_every_node_land_exactly_once() {
    let net = net("landings", 3).await;
    net.repo(0, "api").await;
    let runs = (0..64).map(|i| {
        let (http, url) = (net.http.clone(), net.nodes[i % 3].url.clone());
        async move {
            let m = format!("land {i}");
            let (s, v) = req(&http, &url, "POST", "/v1/code/repos/acme/api/land", Some(land_body(&m, &format!("f{i}.txt")))).await;
            (m, s, v)
        }
    });
    let results = futures::future::join_all(runs).await;
    let mut acked = Vec::new();
    for (m, s, v) in results {
        assert_eq!(s, StatusCode::OK, "{m}: {v}");
        acked.push(m);
    }
    let log = net.log(1, "api").await;
    assert_eq!(log.len(), 65);
    assert_exactly_once(&log, &acked);
    let paths = net.ok(2, "GET", "/v1/code/repos/acme/api/paths/main", None).await;
    assert_eq!(paths["paths"].as_array().unwrap().len(), 65);
}

// ── failures ──

/// Kill the owner while landings keep coming through the other nodes: after its lease expires
/// another node takes the repo and serves it, and every acknowledged landing is on main once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_dead_owners_repo_moves_and_keeps_every_acknowledged_landing() {
    let net = net("kill", 3).await;
    let repo_id = net.repo(0, "api").await;
    let o = net.owner(&repo_id).unwrap();
    let survivors: Vec<usize> = (0..3).filter(|i| *i != o).collect();
    let acked = Arc::new(Mutex::new(Vec::<String>::new()));
    let failed = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    for w in 0..4 {
        let (http, url) = (net.http.clone(), net.nodes[survivors[w % 2]].url.clone());
        let (acked, failed, stop) = (acked.clone(), failed.clone(), stop.clone());
        workers.push(tokio::spawn(async move {
            let mut i = 0;
            while !stop.load(Ordering::SeqCst) {
                let m = format!("w{w} {i}");
                let (s, _) = req(&http, &url, "POST", "/v1/code/repos/acme/api/land", Some(land_body(&m, &format!("w{w}/{i}.txt")))).await;
                if s == StatusCode::OK {
                    acked.lock().push(m);
                } else {
                    failed.fetch_add(1, Ordering::SeqCst);
                }
                i += 1;
            }
        }));
    }
    until("some landings", Duration::from_secs(20), || acked.lock().len() >= 20).await;
    let killed_at = Instant::now();
    net.nodes[o].kill();
    let at_kill = acked.lock().len();
    until("a survivor takes the repo", net.env.takeover(), || survivors.iter().any(|s| net.nodes[*s].owns(&repo_id))).await;
    let took = killed_at.elapsed();
    until("landings after the takeover", Duration::from_secs(30), || acked.lock().len() >= at_kill + 20).await;
    stop.store(true, Ordering::SeqCst);
    for w in workers {
        w.await.unwrap();
    }
    let n = net.owner(&repo_id).unwrap();
    assert!(survivors.contains(&n));
    // The lease had to expire first: no early takeover.
    assert!(took >= Duration::from_millis(500 * u64::from(net.env.scale())), "taken after {took:?}");
    let acked = acked.lock().clone();
    // Only requests in flight on the killed node at the moment it died may fail.
    assert!(failed.load(Ordering::SeqCst) <= 4, "{} landings failed", failed.load(Ordering::SeqCst));
    let log = net.log(survivors[0], "api").await;
    assert_exactly_once(&log, &acked);
}

/// Checks that pass, or (on the node that's told to) never finish.
#[derive(Default)]
struct GateRunner {
    hang: AtomicBool,
    started: AtomicUsize,
}

#[async_trait]
impl Runner for GateRunner {
    async fn plan(&self, _repo: &RepoRow, dir: &Path) -> anyhow::Result<Vec<Check>> {
        let text = std::fs::read_to_string(dir.join("roll.toml")).unwrap_or_default();
        Ok(text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("[checks.").and_then(|r| r.strip_suffix(']')))
            .map(|name| Check { name: name.into(), status: CheckStatus::Queued, source: "runner".into(), url: None, summary: None, started_at: None, finished_at: None })
            .collect())
    }

    async fn run(&self, _repo: &RepoRow, _change: Option<&Change>, _dir: &Path, check: &Check) -> anyhow::Result<Check> {
        self.started.fetch_add(1, Ordering::SeqCst);
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(Check { status: CheckStatus::Success, summary: Some("ok".into()), ..check.clone() })
    }
}

/// The owner dies while its merge queue tests a batch: the node that takes the repo over picks
/// the batch up from the database, tests it again and lands every change once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_merge_queue_continues_a_dead_owners_batch() {
    let net = net("queue", 3).await;
    let repo_id = net.repo(0, "api").await;
    let base = "/v1/code/repos/acme/api";
    let toml = "[checks.ci]\nimage = \"busybox\"\nrun = \"true\"\n";
    net.ok(1, "POST", &format!("{base}/land"), Some(json!({ "message": "checks", "files": { "roll.toml": { "content": toml } } }))).await;
    net.ok(2, "PATCH", base, Some(json!({ "land_policy": "queue" }))).await;
    let o = net.owner(&repo_id).unwrap();
    let runners: Vec<Arc<GateRunner>> = (0..3).map(|_| Arc::new(GateRunner::default())).collect();
    runners[o].hang.store(true, Ordering::SeqCst);
    for (n, r) in net.nodes.iter().zip(&runners) {
        n.code.set_runner(r.clone());
        n.code.start_queues().unwrap();
    }
    let p = (o + 1) % 3;
    let mut ids = Vec::new();
    for i in 0..3 {
        let c = net.ok(p, "POST", &format!("{base}/changes"), Some(json!({ "title": format!("queued {i}"), "message": format!("queued {i}"), "files": { format!("q{i}.txt"): { "content": "q\n" } } }))).await;
        let id = c["id"].as_str().unwrap().to_string();
        let (s, v) = net.req(p, "POST", &format!("/v1/code/changes/{id}/enqueue"), Some(json!({ "lane": "normal" }))).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{v}");
        ids.push(id);
    }
    until("the owner tests a batch", Duration::from_secs(20), || runners[o].started.load(Ordering::SeqCst) > 0).await;
    let q = net.ok(p, "GET", &format!("{base}/queue"), None).await;
    assert!(!q["batch"].is_null(), "a batch is in flight: {q}");
    net.nodes[o].kill();
    let deadline = Instant::now() + net.env.takeover() + Duration::from_secs(20);
    for id in &ids {
        loop {
            let (s, c) = net.req(p, "GET", &format!("/v1/code/changes/{id}"), None).await;
            if s == StatusCode::OK && c["state"] == "landed" {
                break;
            }
            assert!(Instant::now() < deadline, "{id} never landed: {c}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let n = net.owner(&repo_id).unwrap();
    assert_ne!(n, o);
    assert!(runners[n].started.load(Ordering::SeqCst) > 0, "the new owner tested the batch again");
    let log = net.log(p, "api").await;
    assert_exactly_once(&log, &["queued 0".into(), "queued 1".into(), "queued 2".into()]);
}

/// A mock Anthropic API whose first review doesn't answer until `release` is set (never, without
/// one); later ones answer at once, finding nothing wrong.
async fn mock_provider(release: Option<Arc<AtomicBool>>) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = calls.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, _h: HeaderMap, _b: Bytes| {
        let (calls, release) = (c2.clone(), release.clone());
        async move {
            if uri.path().ends_with("/count_tokens") {
                return axum::Json(json!({ "input_tokens": 500 })).into_response();
            }
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                match release {
                    Some(r) => {
                        while !r.load(Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    }
                    None => std::future::pending::<()>().await,
                }
            }
            let review = json!({ "walkthrough": "Adds a file.", "files": [], "diagram": null, "findings": [], "notes": [] });
            axum::Json(json!({
                "model": "claude-opus-5-5",
                "stop_reason": "end_turn",
                "content": [{ "type": "text", "text": review.to_string() }],
                "usage": { "input_tokens": 100, "output_tokens": 20 },
            }))
            .into_response()
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, calls)
}

/// Nodes whose hosted reviewer is the mock at `url`.
fn reviewer_config(url: String) -> Arc<dyn Fn(&mut Config) + Send + Sync> {
    Arc::new(move |c: &mut Config| {
        c.review.hosted = Some(code_control::ai::Hosted { provider: "anthropic".into(), api_key: "sk-hosted".into(), base_url: Some(url.clone()), model: None, secondary_model: None });
        c.review.retry = code_review::provider::RetryPolicy { attempts: 1, base: Duration::from_millis(5), max_delay: Duration::from_millis(50), timeout: Duration::from_secs(3600) };
        c.review.retry_ms = 50;
    })
}

/// The owner dies in the middle of a review (the model never answers it): the node that takes
/// the repo over runs the job again, and the change gets its review.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_running_review_runs_again_after_its_owner_dies() {
    let (url, calls) = mock_provider(None).await;
    let net = net_with("review", 3, reviewer_config(url)).await;
    for n in &net.nodes {
        n.code.start_reviewer();
    }
    let repo_id = net.repo(0, "api").await;
    let base = "/v1/code/repos/acme/api";
    net.ok(1, "PATCH", &format!("{base}/reviewer"), Some(json!({ "enabled": true, "mode": "standard" }))).await;
    let o = net.owner(&repo_id).unwrap();
    let p = (o + 1) % 3;
    let c = net.ok(p, "POST", &format!("{base}/changes"), Some(json!({ "title": "t", "message": "t", "base": "main", "files": { "a.rs": { "content": "fn a() {}\n" } } }))).await;
    let id = c["id"].as_str().unwrap().to_string();
    until("the owner asks the model", Duration::from_secs(20), || calls.load(Ordering::SeqCst) >= 1).await;
    let jobs = net.ok(p, "GET", &format!("/v1/code/changes/{id}/ai-jobs"), None).await;
    assert_eq!(jobs[0]["state"], "running", "{jobs}");
    net.nodes[o].kill();
    let deadline = Instant::now() + net.env.takeover() + Duration::from_secs(20);
    loop {
        let (s, v) = net.req(p, "GET", &format!("/v1/code/changes/{id}/reviews"), None).await;
        if s == StatusCode::OK && v.as_array().is_some_and(|a| a.iter().any(|r| r["reviewer"]["kind"] == "roll" && !r["ai"].is_null())) {
            break;
        }
        assert!(Instant::now() < deadline, "no roll review: {v}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The review is posted before its job is marked done: wait for that too.
    let mut jobs = Value::Null;
    while Instant::now() < deadline {
        jobs = net.ok(p, "GET", &format!("/v1/code/changes/{id}/ai-jobs"), None).await;
        if jobs[0]["state"] == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!((jobs[0]["state"].as_str(), jobs[0]["attempts"].as_u64()), (Some("done"), Some(2)), "{jobs}");
    assert_ne!(net.owner(&repo_id), Some(o));
}

/// A node restarting (its reviewer starting again) leaves the reviews running on the other
/// nodes alone: each job runs once, on its repo's owner, and is billed once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn restarting_a_node_doesnt_run_another_nodes_review_again() {
    let release = Arc::new(AtomicBool::new(false));
    let (url, calls) = mock_provider(Some(release.clone())).await;
    let mut net = net_with("restart", 2, reviewer_config(url)).await;
    for n in &net.nodes {
        n.code.start_reviewer();
    }
    let repo_id = net.repo(0, "api").await;
    let base = "/v1/code/repos/acme/api";
    net.ok(1, "PATCH", &format!("{base}/reviewer"), Some(json!({ "enabled": true, "mode": "standard" }))).await;
    let o = net.owner(&repo_id).unwrap();
    let other = 1 - o;
    let c = net.ok(other, "POST", &format!("{base}/changes"), Some(json!({ "title": "t", "message": "t", "base": "main", "files": { "a.rs": { "content": "fn a() {}\n" } } }))).await;
    let id = c["id"].as_str().unwrap().to_string();
    until("the owner asks the model", Duration::from_secs(20), || calls.load(Ordering::SeqCst) >= 1).await;
    // The other node restarts (same id) and starts its reviewer while the review is running.
    net.restart(other).await;
    net.nodes[other].code.start_reviewer();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the restarted node left the running review alone");
    let jobs = net.ok(other, "GET", &format!("/v1/code/changes/{id}/ai-jobs"), None).await;
    assert_eq!(jobs[0]["state"], "running", "{jobs}");
    release.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let jobs = net.ok(other, "GET", &format!("/v1/code/changes/{id}/ai-jobs"), None).await;
        if jobs[0]["state"] == "done" {
            assert_eq!(jobs[0]["attempts"], 1, "{jobs}");
            break;
        }
        assert!(Instant::now() < deadline, "the review never finished: {jobs}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one model call for one review");
    let usage = net.ok(other, "GET", "/v1/code/owners/acme/usage", None).await;
    assert_eq!(usage["totals"]["reviews"], 1, "metered once: {usage}");
}

// ── work that isn't per repo ──

/// A webhook receiver: each `landed` delivery's id and changeset.
async fn hook_receiver() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    let got: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let g = got.clone();
    let app = axum::Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
        let got = g.clone();
        async move {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let d = headers.get("x-roll-delivery").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            if v["event"] == "landed" {
                got.lock().push((d, v["data"]["changeset"].as_str().unwrap_or("").to_string()));
            }
            StatusCode::OK
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, got)
}

/// Landings reach webhooks through one forwarder at a time: the cluster's `land-forwarder`
/// singleton. Kill the node running it while landings keep coming: it stops forwarding (it no
/// longer holds the singleton), another node takes the singleton over and carries on from the
/// cursor, and every landing is delivered exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_land_forwarder_moves_with_its_singleton() {
    let (url, got) = hook_receiver().await;
    let net = net_with("forwarder", 3, Arc::new(|c: &mut Config| c.review_app_retry_ms = 50)).await;
    for n in &net.nodes {
        n.code.start_webhooks();
    }
    net.repo(0, "api").await;
    let (s, v) = net.req(1, "POST", "/v1/code/repos/acme/api/webhooks", Some(json!({ "url": url, "events": ["landed"] }))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let mut landed = Vec::new();
    let land = |i: usize, node: usize| {
        let (http, base) = (net.http.clone(), net.nodes[node].url.clone());
        async move {
            let (s, v) = req(&http, &base, "POST", "/v1/code/repos/acme/api/land", Some(land_body(&format!("l{i}"), &format!("l{i}.txt")))).await;
            assert_eq!(s, StatusCode::OK, "{v}");
            v["changeset"].as_str().unwrap().to_string()
        }
    };
    for i in 0..10 {
        landed.push(land(i, i % 3).await);
    }
    let delivered = |want: &[String]| {
        let g = got.lock();
        want.iter().all(|cs| g.iter().any(|(_, c)| c == cs))
    };
    let wait = Duration::from_secs(20) + net.env.takeover();
    let deadline = Instant::now() + wait;
    while !delivered(&landed) {
        assert!(Instant::now() < deadline, "not every landing was delivered: {:?}", got.lock());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let holder = |n: &Node| n.owns(&format!("singleton:{}", code_control::webhooks::LAND_FORWARDER));
    let f = net.nodes.iter().position(holder).expect("a node runs the forwarder");
    net.nodes[f].kill();
    let survivors: Vec<usize> = (0..3).filter(|i| *i != f).collect();
    for i in 10..20 {
        landed.push(land(i, survivors[i % 2]).await);
    }
    // The dead node's forwarder doesn't hold the singleton any more: it forwards nothing.
    let dead = net.nodes[f].code.clone();
    let done = tokio::task::spawn_blocking(move || dead.forward_landings()).await.unwrap().unwrap();
    assert_eq!((done.landings, done.deliveries), (0, 0));
    let deadline = Instant::now() + wait;
    while !delivered(&landed) {
        assert!(Instant::now() < deadline, "landings after the forwarder died weren't all delivered: {:?}", got.lock());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(survivors.iter().any(|s| holder(&net.nodes[*s])), "a survivor runs the forwarder now");
    // Nothing more arrives late: each landing went out in exactly one delivery.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let g = got.lock().clone();
    for cs in &landed {
        let mut ids: Vec<&String> = g.iter().filter(|(_, c)| c == cs).map(|(d, _)| d).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 1, "landing {cs} went out in {} deliveries", ids.len());
    }
}

/// A landing on one node reaches a webhook within well under the forwarder's 5 s poll when the
/// forwarder runs on another node: the landing's transaction wakes every node's forwarder
/// (Postgres `NOTIFY`; in-process on SQLite).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn landings_on_one_node_wake_the_forwarder_on_another() {
    let got: Arc<Mutex<HashMap<String, Instant>>> = Arc::default();
    let g = got.clone();
    let app = axum::Router::new().fallback(move |body: Bytes| {
        let got = g.clone();
        async move {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if v["event"] == "landed" {
                got.lock().entry(v["data"]["changeset"].as_str().unwrap_or("").to_string()).or_insert_with(Instant::now);
            }
            StatusCode::OK
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let net = net_with("wake", 3, Arc::new(|c: &mut Config| c.review_app_retry_ms = 50)).await;
    for n in &net.nodes {
        n.code.start_webhooks();
    }
    let holder = |n: &Node| n.owns(&format!("singleton:{}", code_control::webhooks::LAND_FORWARDER));
    until("a node runs the forwarder", Duration::from_secs(30), || net.nodes.iter().any(holder)).await;
    let f = net.nodes.iter().position(holder).unwrap();
    // A repo whose owner (where its landings run) isn't the forwarder's node.
    let mut name = String::new();
    for i in 0..20 {
        let n = format!("r{i}");
        let id = net.repo(i % 3, &n).await;
        if net.owner(&id).is_some_and(|o| o != f) {
            name = n;
            break;
        }
    }
    assert!(!name.is_empty(), "some repo lives off the forwarder's node");
    let (s, v) = net.req(f, "POST", &format!("/v1/code/repos/acme/{name}/webhooks"), Some(json!({ "url": url, "events": ["landed"] }))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let mut took = Vec::new();
    for i in 0..5 {
        // Spaced out so the 5 s poll can't explain them all.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let at = Instant::now();
        let v = net.ok((f + 1 + i % 2) % 3, "POST", &format!("/v1/code/repos/acme/{name}/land"), Some(land_body(&format!("w{i}"), &format!("w{i}.txt")))).await;
        let cs = v["changeset"].as_str().unwrap().to_string();
        until("the landing is delivered", Duration::from_secs(20), || got.lock().contains_key(&cs)).await;
        took.push(got.lock()[&cs].duration_since(at));
    }
    assert_eq!(net.nodes.iter().position(holder), Some(f), "the forwarder stayed put");
    // Without the wake-up each one waits for the next 5 s poll (about 4 s here); with it, the
    // typical one takes a fraction of a second (a loaded machine may slow one or two down).
    let mut sorted = took.clone();
    sorted.sort();
    assert!(sorted[2] < Duration::from_millis(1500), "median delivery {:?}: {took:?}", sorted[2]);
    assert!(sorted[1] < Duration::from_millis(1000), "deliveries took {took:?}");
}

// ── leases and indexes ──

/// The lease row of a repo (from any node's control database).
fn lease_row(n: &Node, repo_id: &str) -> Option<String> {
    use code_sql::OptionalExtension;
    n.code.db.sql().query_row("SELECT node_id FROM code_leases WHERE key = ?", [repo_id], |r| r.get(0)).optional().unwrap()
}

fn index_file(n: &Node, repo_id: &str) -> std::path::PathBuf {
    n._home.path().join("code").join("index").join(format!("{repo_id}.db"))
}

/// Search a repo through `node` until its owner has built the index.
async fn search_ready(net: &Net, node: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (s, v) = net.req(node, "GET", "/v1/code/repos/acme/api/search?q=api", None).await;
        if s == StatusCode::OK {
            return;
        }
        assert!(Instant::now() < deadline, "search never worked: {s} {v}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A node that loses a repo removes its code index; deleting a repo (through a node that
/// doesn't own it) stops its work on the owner, removes the owner's index and deletes its lease
/// row, which no node takes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn moving_or_deleting_a_repo_leaves_no_index_or_lease_behind() {
    let net = net_with("delete", 3, Arc::new(|c: &mut Config| c.review.index = true)).await;
    let repo_id = net.repo(0, "api").await;
    until("someone owns it", Duration::from_secs(20), || net.owner(&repo_id).is_some()).await;
    let o = net.owner(&repo_id).unwrap();
    search_ready(&net, (o + 1) % 3).await;
    assert!(index_file(&net.nodes[o], &repo_id).exists(), "the owner built the index");
    // Moving away (the owner drains): the old owner drops its copy once the repo has stayed
    // away for a while, and the new owner builds its own.
    net.nodes[o].code.drain(Duration::from_secs(30)).await;
    net.nodes[o].dead.store(true, Ordering::SeqCst);
    until("the old owner's index is gone", Duration::from_secs(30), || !index_file(&net.nodes[o], &repo_id).exists()).await;
    let first = (0..3).find(|i| *i != o).unwrap();
    search_ready(&net, first).await;
    until("someone owns it", Duration::from_secs(20), || net.owner(&repo_id).is_some()).await;
    let n = net.owner(&repo_id).unwrap();
    assert!(n != o && index_file(&net.nodes[n], &repo_id).exists());
    // Deleted through another node: the owner runs the delete.
    let other = (0..3).find(|i| *i != n && *i != o).unwrap();
    let (s, v) = net.req(other, "DELETE", "/v1/code/repos/acme/api", None).await;
    assert!(s.is_success(), "{s} {v}");
    until("the lease row is deleted", Duration::from_secs(20), || lease_row(&net.nodes[0], &repo_id).is_none()).await;
    until("nobody owns it", Duration::from_secs(10), || net.owner(&repo_id).is_none()).await;
    assert!(!index_file(&net.nodes[n], &repo_id).exists(), "the owner's index is gone");
    // Several sweeps later the row hasn't come back, and the name is free.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(lease_row(&net.nodes[0], &repo_id).is_none() && net.owner(&repo_id).is_none());
    let (s, _) = net.req(other, "GET", "/v1/code/repos/acme/api", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(lease_row(&net.nodes[0], &repo_id).is_none(), "a 404 takes no lease");
}

/// An owner gives up a repo it hasn't used for `repo_idle_release` once nothing is left to do
/// (an open event stream or a promised webhook event keeps it); the lease row goes, and the
/// next request takes the repo again, on any node.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn idle_repos_are_released_and_taken_again_on_demand() {
    let idle = Duration::from_millis(1500);
    let net = net_with("idle", 3, Arc::new(move |c: &mut Config| c.repo_idle_release = idle)).await;
    let repo_id = net.repo(0, "api").await;
    until("someone owns it", Duration::from_secs(20), || net.owner(&repo_id).is_some()).await;
    let o = net.owner(&repo_id).unwrap();
    let p = (o + 1) % 3;
    // An open event stream is use: the repo stays.
    let mut stream = net.http.get(format!("{}/v1/code/repos/acme/api/events", net.nodes[p].url)).send().await.unwrap();
    assert_eq!(stream.status().as_u16(), 200);
    let reader = tokio::spawn(async move { while let Ok(Some(_)) = stream.chunk().await {} });
    tokio::time::sleep(idle * 3).await;
    assert_eq!(lease_row(&net.nodes[0], &repo_id), Some(net.nodes[o].id.clone()), "kept while a stream is open");
    reader.abort();
    // Work left to do keeps it too (a promised webhook event nobody delivered yet).
    let sql = net.nodes[o].code.db.sql().clone();
    sql.execute("INSERT INTO webhook_outbox (id, repo_id, kind, subject, data, created_at) VALUES ('ob_test', ?, 'change', 'x', NULL, ?)", code_sql::params![repo_id.as_str(), i64::MAX / 2])
        .unwrap();
    tokio::time::sleep(idle * 3).await;
    assert_eq!(lease_row(&net.nodes[0], &repo_id), Some(net.nodes[o].id.clone()), "kept while work is waiting");
    sql.execute("DELETE FROM webhook_outbox WHERE id = 'ob_test'", ()).unwrap();
    until("the idle repo is given up", idle * 4 + Duration::from_secs(10), || lease_row(&net.nodes[0], &repo_id).is_none()).await;
    until("nobody holds it", Duration::from_secs(10), || net.owner(&repo_id).is_none()).await;
    // Nobody takes it back on their own.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(net.owner(&repo_id).is_none() && lease_row(&net.nodes[0], &repo_id).is_none());
    // The next request does, wherever it arrives.
    net.ok(p, "POST", "/v1/code/repos/acme/api/land", Some(land_body("after idle", "i.txt"))).await;
    assert!(lease_row(&net.nodes[0], &repo_id).is_some(), "taken again");
    assert_exactly_once(&net.log(p, "api").await, &["after idle".into()]);
}

/// Indexes left on disk by an earlier process: a single node removes those of repos that no
/// longer exist; a cluster node, which starts owning nothing, removes them all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leftover_indexes_are_removed_at_startup() {
    let env = Env::new("indexes");
    let home = tempfile::tempdir().unwrap();
    let objects = || Objects::new(&StorageConfig::Local(env.dir.path().join("objects"))).unwrap();
    let config = || {
        let mut c = Config::local(home.path().to_path_buf());
        c.review.index = true;
        c
    };
    let code = Code::open(config(), objects()).unwrap();
    code.create_owner(&code_control::Caller::Admin, "acme", code_control::model::OwnerKind::Org, None).unwrap();
    let id = code.create_repo(&code_control::Caller::Admin, &json!({ "owner": "acme", "name": "api" })).await.unwrap().id;
    let dir = home.path().join("code").join("index");
    std::fs::create_dir_all(&dir).unwrap();
    for f in [format!("{id}.db"), "repo_gone.db".into(), "repo_gone.db-wal".into()] {
        std::fs::write(dir.join(f), b"").unwrap();
    }
    drop(code);
    let code = Code::open(config(), objects()).unwrap();
    assert!(dir.join(format!("{id}.db")).exists(), "a single node keeps the index of a repo that exists");
    assert!(!dir.join("repo_gone.db").exists() && !dir.join("repo_gone.db-wal").exists(), "and drops the one of a repo that's gone");
    drop(code);
    // The same home as a cluster node: it owns nothing yet.
    let node = start_node(&env, "n-a", &|c: &mut Config| {
        c.home = home.path().to_path_buf();
        c.review.index = true;
    })
    .await;
    assert!(!dir.join(format!("{id}.db")).exists(), "a cluster node starts with no index");
    node.cluster().leave();
}

// ── GitHub ──

const GH_SECRET: &str = "github webhook secret";

/// Every node runs the GitHub App (its API unreachable: the events used here need no calls), and
/// `acme/app` is connected to GitHub's acme/app.
async fn github_net(name: &str) -> (Net, String) {
    let net = net(name, 3).await;
    let repo_id = net.repo(0, "app").await;
    let link = json!({
        "installation_id": 7, "owner": "acme", "repo": "app", "url": "https://github.com/acme/app",
        "default_branch_protected": null, "mirror": { "state": "ok", "last_push_at": null, "behind": 0, "error": null },
        "remote_head": null, "connected": true,
    });
    net.nodes[0]
        .code
        .db
        .sql()
        .execute("UPDATE repos SET source = 'github', github = ?, import = ? WHERE id = ?", code_sql::params![link.to_string(), json!({ "state": "done" }).to_string(), repo_id.as_str()])
        .unwrap();
    for n in &net.nodes {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap().to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string();
        let mut config = code_github::AppConfig::new(1, key, GH_SECRET, "roll-test");
        config.api_base = format!("http://{}:9/api", [127, 0, 0, 1].map(|b: u8| b.to_string()).join("."));
        config.web_base = "https://github.com".into();
        let client = code_github::ClientOptions { max_retries: 0, timeout: Duration::from_secs(2), ..Default::default() };
        let mirror = code_github::MirrorConfig { debounce: Duration::from_secs(600), max_delay: Duration::from_secs(600), ..Default::default() };
        code_control::github::enable(&n.code, config, code_control::github::Options { mirror, client }).unwrap();
        n.code.set_github_delivery_claim(Duration::from_secs(2));
    }
    (net, repo_id)
}

async fn github_hook(http: &reqwest::Client, base: &str, event: &str, delivery: &str, body: &Value) -> StatusCode {
    let body = body.to_string();
    let r = http
        .post(format!("{base}/v1/code/github/webhook"))
        .header("x-github-event", event)
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", code_github::webhook::sign(GH_SECRET.as_bytes(), body.as_bytes()))
        .header("content-type", "application/json")
        .body(body)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    StatusCode::from_u16(r.status().as_u16()).unwrap()
}

fn issue_opened(n: u64) -> Value {
    json!({
        "action": "opened",
        "issue": { "number": n, "title": format!("Issue {n}"), "body": "b", "state": "open", "labels": [], "html_url": format!("https://github.com/acme/app/issues/{n}"), "user": { "login": "octo", "id": 9 } },
        "repository": { "id": 1, "name": "app", "full_name": "acme/app", "private": false, "default_branch": "main", "owner": { "login": "acme", "id": 1, "type": "Organization" } },
        "installation": { "id": 7 },
    })
}

/// Work items made from GitHub issue `n` (through any live node).
async fn issue_items(net: &Net, node: usize, n: u64) -> usize {
    let (s, v) = net.req(node, "GET", "/v1/code/repos/acme/app/work", None).await;
    if s != StatusCode::OK {
        return 0;
    }
    v.as_array().map_or(0, |a| a.iter().filter(|w| w["source"] == format!("github#{n}")).count())
}

/// A GitHub delivery for a repo another node owns is recorded before GitHub is answered: the
/// owner dying while it handles it (GitHub gets 202, not an error it would have to redeliver)
/// loses nothing: the node that takes the repo over handles it, once. A delivery that arrives
/// while the owner can't be reached is recorded and handled the same way, and redeliveries are
/// answered without running twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn github_deliveries_survive_their_owner_dying() {
    let (net, repo_id) = github_net("github").await;
    let o = net.owner(&repo_id).unwrap();
    let p = (o + 1) % 3;
    let q = (o + 2) % 3;
    // Through a node that doesn't own the repo, handled on the owner.
    assert_eq!(github_hook(&net.http, &net.nodes[p].url, "issues", "d-1", &issue_opened(1)).await, StatusCode::OK);
    assert_eq!(issue_items(&net, q, 1).await, 1);
    assert_eq!(github_hook(&net.http, &net.nodes[q].url, "issues", "d-1", &issue_opened(1)).await, StatusCode::OK, "a redelivery");
    // The owner gets stuck handling the next one, then dies.
    net.nodes[o].code.github_hang_deliveries(true);
    let started = net.nodes[o].code.github_deliveries_started();
    let send = {
        let (http, url) = (net.http.clone(), net.nodes[p].url.clone());
        tokio::spawn(async move { github_hook(&http, &url, "issues", "d-2", &issue_opened(2)).await })
    };
    until("the owner starts on it", Duration::from_secs(20), || net.nodes[o].code.github_deliveries_started() > started).await;
    net.nodes[o].kill();
    assert_eq!(send.await.unwrap(), StatusCode::ACCEPTED, "recorded, so GitHub is told it's taken");
    // While nobody can serve the repo (the dead owner's lease hasn't expired), another one.
    assert_eq!(github_hook(&net.http, &net.nodes[q].url, "issues", "d-3", &issue_opened(3)).await, StatusCode::ACCEPTED);
    let deadline = Instant::now() + net.env.takeover() + Duration::from_secs(30);
    loop {
        if issue_items(&net, p, 2).await == 1 && issue_items(&net, p, 3).await == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "the recorded deliveries were never handled");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let n = net.owner(&repo_id).expect("a survivor owns it");
    assert_ne!(n, o);
    // A part's row goes right after its handler is done.
    let pending = || net.nodes[n].code.db.sql().query_row("SELECT COUNT(*) FROM github_inbox", (), |r| r.get::<_, i64>(0)).unwrap();
    until("every recorded part is handled and removed", Duration::from_secs(20), || pending() == 0).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    for i in 1..=3 {
        assert_eq!(issue_items(&net, p, i).await, 1, "issue {i} became one work item");
    }
}

// ── membership ──

/// Landings on many repos through every node never stop while a fourth node joins and the
/// others hand it its share of the repos: no request fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_joining_node_takes_repos_over_without_failing_requests() {
    let mut net = net("join", 2).await;
    let mut repos = Vec::new();
    // Enough repos that the joining node is the preferred one for at least three.
    let joining = |id: &str| code_cluster::placement::rendezvous(id, IDS[..3].iter().copied()) == Some(IDS[2]);
    while repos.len() < 12 || repos.iter().filter(|(_, id): &&(String, String)| joining(id)).count() < 3 {
        let i = repos.len();
        repos.push((format!("r{i}"), net.repo(i % 2, &format!("r{i}")).await));
    }
    let urls = Arc::new(Mutex::new(net.nodes.iter().map(|n| n.url.clone()).collect::<Vec<_>>()));
    let acked: Arc<Mutex<BTreeMap<String, Vec<String>>>> = Arc::default();
    let failures: Arc<Mutex<Vec<String>>> = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    for w in 0..6usize {
        let (http, urls, acked, failures, stop) = (net.http.clone(), urls.clone(), acked.clone(), failures.clone(), stop.clone());
        let names: Vec<String> = repos.iter().map(|(n, _)| n.clone()).collect();
        workers.push(tokio::spawn(async move {
            let mut i = 0usize;
            while !stop.load(Ordering::SeqCst) {
                let repo = &names[(w + i) % names.len()];
                let url = {
                    let u = urls.lock();
                    u[(w + i) % u.len()].clone()
                };
                let m = format!("w{w} {i}");
                let (s, v) = req(&http, &url, "POST", &format!("/v1/code/repos/acme/{repo}/land"), Some(land_body(&m, &format!("w{w}-{i}.txt")))).await;
                if s == StatusCode::OK {
                    acked.lock().entry(repo.clone()).or_default().push(m);
                } else {
                    failures.lock().push(format!("{repo} via {url}: {s} {v}"));
                }
                let (s, v) = req(&http, &url, "GET", &format!("/v1/code/repos/acme/{repo}/tree/main/"), None).await;
                if s != StatusCode::OK {
                    failures.lock().push(format!("read {repo} via {url}: {s} {v}"));
                }
                i += 1;
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    net.add_node().await;
    urls.lock().push(net.nodes[2].url.clone());
    until("the new node owns some repos", Duration::from_secs(30), || repos.iter().any(|(_, id)| net.nodes[2].owns(id))).await;
    // Rebalancing moves every repo to its preferred node while the load goes on.
    until("every repo is on its preferred node", Duration::from_secs(30), || {
        repos.iter().all(|(_, id)| {
            let want = code_cluster::placement::rendezvous(id, IDS[..3].iter().copied()).unwrap();
            net.nodes.iter().any(|n| n.id == want && n.owns(id))
        })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    stop.store(true, Ordering::SeqCst);
    for w in workers {
        w.await.unwrap();
    }
    let failures = failures.lock().clone();
    assert!(failures.is_empty(), "{} requests failed: {failures:#?}", failures.len());
    let acked = acked.lock().clone();
    for (name, _) in &repos {
        let log = net.log(2, name).await;
        assert_exactly_once(&log, acked.get(name).map(|v| v.as_slice()).unwrap_or_default());
    }
}

/// Draining a node (what SIGTERM does) moves every repo it owns to the others before it leaves,
/// while requests through the other nodes keep succeeding; its health check fails meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn draining_moves_every_repo_off_a_node_before_it_leaves() {
    let net = net("drain", 3).await;
    let mut repos = Vec::new();
    for i in 0..9 {
        repos.push((format!("r{i}"), net.repo(i % 3, &format!("r{i}")).await));
    }
    let owned = |n: usize| repos.iter().filter(|(_, id)| net.nodes[n].owns(id)).count();
    let d = (0..3).max_by_key(|n| owned(*n)).unwrap();
    assert!(owned(d) > 0);
    let others: Vec<usize> = (0..3).filter(|i| *i != d).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let failures: Arc<Mutex<Vec<String>>> = Arc::default();
    let acked: Arc<Mutex<BTreeMap<String, Vec<String>>>> = Arc::default();
    let mut workers = Vec::new();
    for w in 0..4usize {
        let (http, url) = (net.http.clone(), net.nodes[others[w % 2]].url.clone());
        let (stop, failures, acked) = (stop.clone(), failures.clone(), acked.clone());
        let names: Vec<String> = repos.iter().map(|(n, _)| n.clone()).collect();
        workers.push(tokio::spawn(async move {
            let mut i = 0usize;
            while !stop.load(Ordering::SeqCst) {
                let repo = &names[(w + i) % names.len()];
                let m = format!("w{w} {i}");
                let (s, v) = req(&http, &url, "POST", &format!("/v1/code/repos/acme/{repo}/land"), Some(land_body(&m, &format!("w{w}-{i}.txt")))).await;
                if s == StatusCode::OK {
                    acked.lock().entry(repo.clone()).or_default().push(m);
                } else {
                    failures.lock().push(format!("{repo}: {s} {v}"));
                }
                i += 1;
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let drained = net.nodes[d].code.clone();
    let health = tokio::spawn({
        let (http, url) = (net.http.clone(), net.nodes[d].url.clone());
        async move {
            // While it drains the node says it's unavailable, so a load balancer moves on.
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let (s, v) = req(&http, &url, "GET", "/v1/code/health", None).await;
                if s == StatusCode::SERVICE_UNAVAILABLE {
                    return v;
                }
                assert!(Instant::now() < deadline, "health stayed {s}: {v}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    });
    drained.drain(Duration::from_secs(30)).await;
    let h = health.await.unwrap();
    assert_eq!(h["cluster"]["draining"], true, "{h}");
    assert!(drained.cluster().unwrap().owned().is_empty(), "nothing is left on the drained node");
    until("the others see it gone", Duration::from_secs(10), || others.iter().all(|o| net.nodes[*o].cluster().nodes().len() == 2)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop.store(true, Ordering::SeqCst);
    for w in workers {
        w.await.unwrap();
    }
    let failures = failures.lock().clone();
    assert!(failures.is_empty(), "{failures:#?}");
    for (name, id) in &repos {
        assert!(others.iter().any(|o| net.nodes[*o].owns(id)), "{name} has an owner among the others");
        assert!(!net.nodes[d].cluster().owns(id));
        let log = net.log(others[0], name).await;
        assert_exactly_once(&log, acked.lock().get(name).map(|v| v.as_slice()).unwrap_or_default());
    }
}

/// A rename keeps the repo on the same owner (the lease is the repo id), and the old name keeps
/// working through any node: 301 on the API, followed by git.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn renaming_a_repo_keeps_its_owner() {
    let net = net("rename", 3).await;
    let repo_id = net.repo(0, "api").await;
    let o = net.owner(&repo_id).unwrap();
    let (p, q) = ((o + 1) % 3, (o + 2) % 3);
    let before: Vec<u64> = net.nodes.iter().map(Node::served).collect();
    assert_eq!(net.ok(p, "PATCH", "/v1/code/repos/acme/api", Some(json!({ "name": "web" }))).await["name"], "web");
    net.ok(q, "POST", "/v1/code/repos/acme/web/land", Some(land_body("after the rename", "r.txt"))).await;
    let (s, v) = net.req(q, "GET", "/v1/code/repos/acme/api/refs", None).await;
    assert_eq!((s, v["redirect"].as_str()), (StatusCode::MOVED_PERMANENTLY, Some("acme/web")));
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), tmp.path(), &["clone", "-q", &format!("{}/git/acme/api.git", net.nodes[p].url), "old"]);
    assert_eq!(std::fs::read_to_string(tmp.path().join("old/r.txt")).unwrap(), "after the rename\n");
    assert_eq!(net.owner(&repo_id), Some(o));
    let after: Vec<u64> = net.nodes.iter().map(Node::served).collect();
    assert_eq!((after[p], after[q]), (before[p], before[q]), "only the owner ran them");
    assert!(after[o] >= before[o] + 4);
}

/// An event stream holds its repo's guard; when the repo starts moving the stream ends (its
/// client reconnects and reaches the new owner), and the move doesn't wait for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn an_event_stream_ends_when_its_repo_moves() {
    let net = net("stream", 3).await;
    let repo_id = net.repo(0, "api").await;
    let o = net.owner(&repo_id).unwrap();
    let p = (o + 1) % 3;
    let mut stream = net.http.get(format!("{}/v1/code/repos/acme/api/events", net.nodes[p].url)).send().await.unwrap();
    if stream.status().as_u16() != 200 {
        let s = stream.status();
        panic!("{s}: {}", stream.text().await.unwrap_or_default());
    }
    let ended = tokio::spawn(async move {
        loop {
            match stream.chunk().await {
                Ok(Some(_)) => continue,
                _ => return Instant::now(),
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t = Instant::now();
    assert!(net.nodes[o].cluster().handoff(&repo_id));
    let at = tokio::time::timeout(Duration::from_secs(10), ended).await.expect("the stream ended").unwrap();
    assert!(at.duration_since(t) < Duration::from_secs(2), "ended {:?} after the handoff started", at.duration_since(t));
    // The repo is served again (by its preferred node), and a new stream works.
    net.ok(p, "POST", "/v1/code/repos/acme/api/land", Some(land_body("after the move", "m.txt"))).await;
    let s = net.http.get(format!("{}/v1/code/repos/acme/api/events", net.nodes[p].url)).send().await.unwrap();
    assert_eq!(s.status().as_u16(), 200);
}

/// `GET /v1/code/health` on every node: 200 with the node's id while it holds its leases; 503
/// once it can't reach the database long enough to trust them, and 200 again after.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn health_follows_each_node() {
    let net = net("health", 3).await;
    for (i, n) in net.nodes.iter().enumerate() {
        // Under load a heartbeat can come late enough for a peer to drop out of the view for a
        // moment: each node's view comes back to all three.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (s, v) = net.req(i, "GET", "/v1/code/health", None).await;
            assert_eq!(s, StatusCode::OK, "{v}");
            assert_eq!((v["status"].as_str(), v["database"].as_bool(), v["cluster"]["node"].as_str()), (Some("ok"), Some(true), Some(n.id.as_str())));
            if v["cluster"]["nodes"] == 3 {
                break;
            }
            assert!(Instant::now() < deadline, "{} never saw all three nodes: {v}", n.id);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    net.nodes[1].cluster().fault_db(true);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (s, v) = net.req(1, "GET", "/v1/code/health", None).await;
        if s == StatusCode::SERVICE_UNAVAILABLE {
            assert_eq!(v["cluster"]["healthy"], false, "{v}");
            break;
        }
        assert!(Instant::now() < deadline, "still {s}: {v}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    net.nodes[1].cluster().fault_db(false);
    let deadline = Instant::now() + Duration::from_secs(10);
    while net.req(1, "GET", "/v1/code/health", None).await.0 != StatusCode::OK {
        assert!(Instant::now() < deadline, "never healthy again");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
