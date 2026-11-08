//! roll tasks on a roll-cluster fleet: several rolld-like nodes in one process
//! (`tasks::fleet`). Requests through any node reach the cell's leader; a standby takes over
//! from a killed leader with every acknowledged write; a drain hands the cell off without failing
//! a request or delivering anything twice; a node that took the log behind the leader's back
//! fences it and is fenced in turn; a node alone imports an old SQLite database. The multi-node
//! tests run on roll-cluster's memory backend and on DynamoDB Local
//! (`ROLL_CLUSTER_TEST_DYNAMODB`; skipped without it), with the log in memory or under
//! `ROLL_TASKS_E2E_LOG` (MinIO in `scripts/test-tasks.sh`).

mod common;

use common::*;
use object_store::ObjectStore;
use object_store::memory::InMemory;
use reqwest::{Method, StatusCode};
use roll_cluster::{Backend as ClusterBackend, Cluster, ClusterConfig, DynamoDb, DynamoDbConfig, Memory, MemoryStore};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tasks::fleet::{KEY, NodeConfig, TasksNode};
use tasks::store::log::LogConfig;

const SECRET: &[u8] = b"tasks cluster test secret 012345";
/// rolld's operator token (`ROLL_ADMIN_TOKEN`): every call below uses it unless it names a key.
const OPERATOR: &str = "operator-token-for-the-tasks-fleet-tests";

macro_rules! on_backends {
    ($($name:ident),* $(,)?) => {
        mod memory {
            $(#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() {
                super::$name(super::Kind::Memory).await
            })*
        }
        mod dynamodb {
            $(#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() {
                super::$name(super::Kind::DynamoDb).await
            })*
        }
    };
}

on_backends!(
    requests_through_any_node_reach_the_leader,
    the_leader_killed_a_standby_takes_over_with_every_acknowledged_write,
    a_drain_hands_the_cell_off_without_failing_a_request,
    a_node_that_takes_the_log_behind_the_leaders_back_is_fenced,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Memory,
    DynamoDb,
}

enum Leases {
    Memory(MemoryStore),
    Dynamo { endpoint: String, table: String },
}

/// An empty log location: in memory, or under a fresh prefix of `ROLL_TASKS_E2E_LOG`.
fn bucket() -> (Arc<dyn ObjectStore>, String) {
    match std::env::var("ROLL_TASKS_E2E_LOG") {
        Ok(url) => {
            let (objects, prefix) = tasks::store::log::objects_from_url(&url).unwrap();
            (objects, format!("{prefix}/fleet-{}", tasks::ids::ulid()))
        }
        Err(_) => (Arc::new(InMemory::new()), "tasks".into()),
    }
}

struct Node {
    cluster: Arc<Cluster>,
    tasks: Arc<TasksNode>,
    /// The node's API, as rolld serves it: tasks' routes, the internal ones, the cluster's.
    base: String,
    serving: tokio::task::JoinHandle<()>,
}

impl Node {
    fn leads(&self) -> bool {
        self.tasks.status().leader
    }

    /// Die as a killed process: no handoff, nothing more committed. The lease runs out.
    fn kill(&self) {
        self.cluster.fault_kill();
        self.tasks.fault_kill();
        self.serving.abort();
    }
}

struct Fleet {
    nodes: Vec<Node>,
    leases: Leases,
    objects: Arc<dyn ObjectStore>,
    prefix: String,
    dir: tempfile::TempDir,
}

impl Fleet {
    async fn start(kind: Kind, name: &str, n: usize) -> Option<Fleet> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "tasks=info,roll_cluster=info".into()))
            .with_test_writer()
            .try_init();
        let leases = match kind {
            Kind::Memory => Leases::Memory(MemoryStore::new()),
            Kind::DynamoDb => {
                let Some(endpoint) = std::env::var("ROLL_CLUSTER_TEST_DYNAMODB").ok().filter(|e| !e.is_empty()) else {
                    eprintln!("skipping: ROLL_CLUSTER_TEST_DYNAMODB isn't set (DynamoDB Local's endpoint)");
                    return None;
                };
                Leases::Dynamo { endpoint, table: format!("tasks-{name}-{}", tasks::ids::ulid().to_lowercase()) }
            }
        };
        let (objects, prefix) = bucket();
        let mut f = Fleet { nodes: Vec::new(), leases, objects, prefix, dir: tempfile::tempdir().unwrap() };
        for _ in 0..n {
            f.add().await;
        }
        f.settle().await;
        Some(f)
    }

    fn log_config(&self, dir: std::path::PathBuf) -> LogConfig {
        let mut log = LogConfig::new(self.objects.clone(), self.prefix.clone(), dir);
        log.shards = 4;
        log.commit_ms = 10;
        log.relaxed_ms = 100;
        log
    }

    async fn add(&mut self) {
        let id = format!("node-{}", self.nodes.len());
        let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", api.local_addr().unwrap());
        let backend: Box<dyn ClusterBackend> = match &self.leases {
            Leases::Memory(m) => Box::new(Memory::new(m.clone())),
            Leases::Dynamo { endpoint, table } => {
                let mut c = DynamoDbConfig::new(table.clone());
                c.endpoint = Some(endpoint.clone());
                c.create_table = true;
                Box::new(tokio::task::block_in_place(|| DynamoDb::connect(c)).unwrap())
            }
        };
        let cluster = tokio::task::block_in_place(|| Cluster::join(backend, ClusterConfig::for_tests(id.clone(), base.clone(), SECRET))).unwrap();
        let mut engine = fast_config();
        // Well under the test cluster's handoff timeout (1.5 s).
        engine.drain_ms = 300;
        // As rolld runs it with the operator's token required.
        engine.require_auth = true;
        let mut cfg = NodeConfig::new(self.log_config(self.dir.path().join(&id)), engine);
        cfg.operator = Some(roll_auth::AdminToken::new(OPERATOR).unwrap());
        cfg.follow_every = Duration::from_millis(50);
        cfg.check_every = Duration::from_millis(500);
        cfg.open_wait = Duration::from_secs(3);
        cfg.ride = Duration::from_secs(10);
        let tasks = TasksNode::start(cluster.clone(), cfg).await.unwrap();
        let router = tasks
            .api_routes()
            .merge(cluster.router())
            .layer(axum::middleware::from_fn_with_state(cluster.clone(), roll_cluster::accept_forwarded));
        let serving = tokio::spawn(async move { axum::serve(api, router).await.unwrap() });
        self.nodes.push(Node { cluster, tasks, base, serving });
    }

    /// Wait until the node placement prefers leads, and every node sees every other.
    async fn settle(&self) {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let preferred = self.nodes[0].cluster.preferred(KEY).map(|n| n.id);
            if let Some(l) = self.leader()
                && preferred.as_deref() == Some(self.nodes[l].cluster.node_id())
                && self.nodes.iter().all(|n| n.cluster.nodes().len() == self.nodes.len())
            {
                return;
            }
            assert!(Instant::now() < until, "the cell didn't settle on its preferred node");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn leader(&self) -> Option<usize> {
        let leaders: Vec<usize> = (0..self.nodes.len()).filter(|i| self.nodes[*i].leads()).collect();
        (leaders.len() == 1).then(|| leaders[0])
    }

    async fn wait_leader_not(&self, old: Option<usize>) -> usize {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(l) = self.leader().filter(|l| Some(*l) != old) {
                return l;
            }
            assert!(Instant::now() < until, "no node leads the cell");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A node that doesn't lead, other than `except`.
    fn non_leader(&self, except: &[usize]) -> usize {
        let l = self.leader();
        (0..self.nodes.len()).find(|i| Some(*i) != l && !except.contains(i)).expect("a node that doesn't lead")
    }

    fn tenures(&self) -> Vec<u64> {
        self.nodes.iter().map(|n| n.tasks.status().tenures).collect()
    }

    /// Wait until a standby's copy holds everything the leader committed so far.
    async fn standby_caught_up(&self) {
        let seq = self.nodes[self.leader().unwrap()].tasks.log_stats().unwrap().seq;
        let until = Instant::now() + Duration::from_secs(10);
        while !self.nodes.iter().any(|n| n.tasks.status().following.is_some_and(|s| s >= seq)) {
            assert!(Instant::now() < until, "no standby followed the log to entry {seq}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn call(base: &str, method: Method, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let mut r = reqwest::Client::new().request(method, format!("{base}{path}"));
    r = r.bearer_auth(token.unwrap_or(OPERATOR));
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

async fn publish(base: &str, body: Value) -> String {
    let (s, v) = call(base, Method::POST, "/v1/tasks/publish", None, Some(body)).await;
    assert!(s.is_success(), "publish: {s} {v}");
    v["message_id"].as_str().unwrap().to_string()
}

fn delivered(recv: &Receiver, path: &str) -> HashMap<String, usize> {
    let mut seen = HashMap::new();
    for h in recv.hits(path) {
        *seen.entry(h.header("roll-message-id").unwrap()).or_insert(0) += 1;
    }
    seen
}

async fn requests_through_any_node_reach_the_leader(kind: Kind) {
    let Some(f) = Fleet::start(kind, "route", 3).await else { return };
    let tenures = f.tenures();
    let recv = Receiver::start().await;
    let via = &f.nodes[f.non_leader(&[])].base;
    let mut ids = Vec::new();
    for i in 0..10 {
        ids.push(publish(via, json!({ "url": recv.url("/m"), "body": format!("{i}") })).await);
    }
    // Hosted tenants and their management go the same way.
    let (s, t) = call(via, Method::POST, "/_roll/tenants", None, Some(json!({ "id": "acme" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{t}");
    let token = t["token"].as_str().unwrap().to_string();
    let other = &f.nodes[f.non_leader(&[f.non_leader(&[])])].base;
    let (s, v) = call(other, Method::POST, "/t/acme/v1/tasks/publish", Some(&token), Some(json!({ "url": recv.url("/acme") }))).await;
    assert!(s.is_success(), "{s} {v}");
    recv.wait("/m", 10, 10_000).await;
    recv.wait("/acme", 1, 10_000).await;
    // The operator's token works on the leader wherever it came in; nothing else does.
    let (s, _) = call(other, Method::GET, "/v1/tasks/queues", Some("not-the-token"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(other, Method::GET, "/v1/tasks/queues", Some(&token), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "a tenant's key isn't the default tenant's");
    // Every node answers reads, and errors come back as they are.
    for n in &f.nodes {
        let (s, m) = call(&n.base, Method::GET, &format!("/v1/tasks/messages/{}", ids[0]), None, None).await;
        assert_eq!(s, StatusCode::OK, "{m}");
        let (s, _) = call(&n.base, Method::GET, "/v1/tasks/messages/msg_nope", None, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }
    // The cell never moved meanwhile, and a standby's copy keeps up with the log.
    assert_eq!(f.tenures(), tenures);
    f.standby_caught_up().await;
}

async fn the_leader_killed_a_standby_takes_over_with_every_acknowledged_write(kind: Kind) {
    let Some(f) = Fleet::start(kind, "kill", 3).await else { return };
    let old = f.leader().unwrap();
    let recv = Receiver::start().await;
    let via = f.nodes[f.non_leader(&[])].base.clone();
    // Due after the crash: only the next leader can deliver these.
    let mut ids = Vec::new();
    for i in 0..20 {
        ids.push(publish(&via, json!({ "url": recv.url("/later"), "body": format!("{i}"), "delay": "3s" })).await);
    }
    let (s, q) = call(&via, Method::PUT, "/v1/tasks/queues/kept", None, Some(json!({ "parallelism": 2 }))).await;
    assert!(s.is_success(), "{q}");
    f.standby_caught_up().await;
    let tenures = f.tenures();

    let killed = Instant::now();
    f.nodes[old].kill();
    // A request sent meanwhile waits for the next leader instead of failing.
    let during = publish(&via, json!({ "url": recv.url("/during"), "dedup_id": "during-failover" })).await;
    let new = f.wait_leader_not(Some(old)).await;
    assert!(killed.elapsed() < Duration::from_secs(10), "took over in {:?}", killed.elapsed());
    assert_eq!(f.nodes[new].tasks.status().tenures, tenures[new] + 1);

    recv.wait("/later", 20, 15_000).await;
    recv.wait("/during", 1, 10_000).await;
    let seen = delivered(&recv, "/later");
    for id in &ids {
        assert!(seen.contains_key(id), "lost {id}");
    }
    let (s, q) = call(&via, Method::GET, "/v1/tasks/queues/kept", None, None).await;
    assert!(s.is_success(), "{q}");
    assert_eq!(q["parallelism"], 2);
    let (s, m) = call(&via, Method::GET, &format!("/v1/tasks/messages/{during}"), None, None).await;
    assert_eq!(s, StatusCode::OK, "{m}");
}

async fn a_drain_hands_the_cell_off_without_failing_a_request(kind: Kind) {
    let Some(f) = Fleet::start(kind, "drain", 3).await else { return };
    let old = f.leader().unwrap();
    let via = f.nodes[f.non_leader(&[])].base.clone();
    let recv = Receiver::start().await;
    let mut ids = Vec::new();
    for i in 0..20 {
        ids.push(publish(&via, json!({ "url": recv.url("/m"), "body": format!("{i}"), "delay": format!("{}ms", i * 40) })).await);
    }
    // A long poll waiting through the handoff.
    let (s, _) = call(&via, Method::PUT, "/v1/tasks/queues/jobs", None, Some(json!({}))).await;
    assert!(s.is_success());
    let poll = {
        let via = via.clone();
        tokio::spawn(async move { call(&via, Method::POST, "/v1/tasks/queues/jobs/pull", None, Some(json!({ "wait": "3s" }))).await })
    };
    // Publishers keep going through the handoff; every publish must succeed.
    let busy = Arc::new(AtomicBool::new(true));
    let publishers: Vec<_> = (0..4)
        .map(|p| {
            let (via, url, busy) = (via.clone(), recv.url("/m"), busy.clone());
            tokio::spawn(async move {
                let mut ids = Vec::new();
                let mut i = 0;
                while busy.load(Ordering::SeqCst) || i < 5 {
                    ids.push(publish(&via, json!({ "url": url, "body": format!("p{p}-{i}") })).await);
                    i += 1;
                }
                ids
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    f.nodes[old].cluster.drain(Duration::from_secs(10)).await;
    let new = f.wait_leader_not(Some(old)).await;
    assert!(started.elapsed() < Duration::from_secs(8), "handoff took {:?}", started.elapsed());
    tokio::time::sleep(Duration::from_millis(300)).await;
    busy.store(false, Ordering::SeqCst);
    for p in publishers {
        ids.extend(p.await.unwrap());
    }
    let (s, v) = poll.await.unwrap();
    assert_eq!(s, StatusCode::OK, "the long poll across the handoff: {v}");
    assert_ne!(new, old);

    recv.wait("/m", ids.len(), 15_000).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = delivered(&recv, "/m");
    assert_eq!(seen.len(), ids.len());
    assert!(seen.values().all(|n| *n == 1), "a planned handoff shouldn't redeliver: {seen:?}");
}

async fn a_node_that_takes_the_log_behind_the_leaders_back_is_fenced(kind: Kind) {
    let Some(f) = Fleet::start(kind, "fence", 2).await else { return };
    let leader = f.leader().unwrap();
    let via = f.nodes[f.non_leader(&[])].base.clone();
    let first = publish(&via, json!({ "url": "http://example.test/x", "delay": "1h" })).await;
    let tenures = f.tenures();

    // Something opens the log without the lease (a partition, a confused operator): it has
    // everything acknowledged, and the leader can't commit any more.
    let dir = tempfile::tempdir().unwrap();
    let usurper = tasks::store::log::LogStore::open(f.log_config(dir.path().to_path_buf())).await.unwrap();
    assert!(tasks::store::Store::msg_get(&*usurper, &first).await.unwrap().is_some());

    // The leader still holds the lease: its next write fails, it takes the log back, and the
    // request that hit the fence runs again there. The client sees only success.
    let second = publish(&via, json!({ "url": "http://example.test/x", "delay": "1h" })).await;
    assert_eq!(f.nodes[leader].tasks.status().tenures, tenures[leader] + 1);
    let err = tasks::store::Store::kv_put(&*usurper, "k", &json!(1), tasks::store::Expect::Any, None).await.unwrap_err();
    assert!(err.to_string().contains("unavailable"), "the usurper can't commit: {err}");
    for id in [first, second] {
        let (s, m) = call(&via, Method::GET, &format!("/v1/tasks/messages/{id}"), None, None).await;
        assert_eq!(s, StatusCode::OK, "{m}");
    }
}

/// A node alone upgrading from SQLite: its old database becomes the cell's default tenant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_alone_imports_its_old_database() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("tasks.db");
    let id = {
        let store = Arc::new(tasks::store::sqlite::SqliteStore::open(&old, 4).unwrap());
        let t = tasks::Tasks::new(fast_config(), store).await.unwrap();
        let p = t.publish(serde_json::from_value(json!({ "url": "http://example.test/x", "delay": "1h" })).unwrap()).await.unwrap();
        p[0].message_id.clone()
    };
    let (objects, prefix) = bucket();
    let mut log = LogConfig::new(objects, prefix, dir.path().join("node"));
    log.shards = 4;
    assert!(tasks::store::log::import_sqlite(&log, "default", &old).await.unwrap());
    let cluster = tokio::task::block_in_place(|| Cluster::join(Box::new(Memory::solo()), ClusterConfig::for_tests("solo", "http://127.0.0.1:9", SECRET))).unwrap();
    let mut cfg = NodeConfig::new(log.clone(), fast_config());
    cfg.single = true;
    let node = TasksNode::start(cluster, cfg).await.unwrap();
    let cell = node.cell().unwrap();
    let router = cell.router();
    let resp = tower::ServiceExt::oneshot(router, axum::http::Request::get(format!("/v1/tasks/messages/{id}")).body(axum::body::Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    node.shutdown().await;
    assert!(!tasks::store::log::import_sqlite(&log, "default", &old).await.unwrap(), "only into a new log");
}
