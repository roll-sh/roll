//! Shared test helpers: a cluster under test on one of the backends.
//!
//! - `Kind::Memory`: one `MemoryStore` the nodes share.
//! - `Kind::Postgres`: the `code_nodes`/`code_leases` tables on SQLite, or on Postgres when
//!   `ROLL_CODE_TEST_DATABASE` names a server (each node then gets a pool of its own, as separate
//!   processes would).
//! - `Kind::DynamoDb`: a fresh table on DynamoDB Local: `ROLL_CLUSTER_TEST_DYNAMODB` names its
//!   endpoint (the gates start one), else the tests start `amazon/dynamodb-local` in Docker as
//!   `roll-cloud-cluster-ddb` and remove it when the test binary exits. Without either, the
//!   DynamoDB tests skip with a message (`ROLL_CLUSTER_DYNAMODB_REQUIRED=1` makes that a failure).

#![allow(dead_code)]

use roll_cluster::{Backend, Cluster, ClusterConfig, DynamoDb, DynamoDbConfig, Memory, MemoryStore, Postgres, placement};
use code_sql::{Db, PgOptions, Target};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub const SECRET: &[u8] = b"test cluster secret, 32 bytes!!";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Memory,
    Postgres,
    DynamoDb,
}

enum Store {
    Memory(MemoryStore),
    Sql(Db, tempfile::TempDir),
    Dynamo { endpoint: String, table: String },
}

pub struct Env {
    pub kind: Kind,
    store: Store,
}

impl Env {
    /// The environment for `kind`, or `None` (with a message) when DynamoDB Local isn't there.
    pub fn new(kind: Kind, name: &str) -> Option<Env> {
        // Logs show up for failing tests only (`cargo test` captures them).
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "roll_cluster=info".into()))
            .with_test_writer()
            .try_init();
        let store = match kind {
            Kind::Memory => Store::Memory(MemoryStore::new()),
            Kind::Postgres => {
                let dir = tempfile::tempdir().unwrap();
                let db = Db::open_or_test(&Target::Sqlite(dir.path().join("cluster.db")), name).unwrap();
                Store::Sql(db, dir)
            }
            Kind::DynamoDb => {
                let endpoint = dynamodb_endpoint()?;
                Store::Dynamo { endpoint, table: format!("t-{name}-{:016x}", random()) }
            }
        };
        Some(Env { kind, store })
    }

    /// A backend for one node: its own handle (and pool, or client) on the shared store.
    pub fn backend(&self) -> Box<dyn Backend> {
        match &self.store {
            Store::Memory(m) => Box::new(Memory::new(m.clone())),
            Store::Sql(db, _) => {
                let db = match (db.pg_schema(), std::env::var(code_sql::TEST_DATABASE_ENV)) {
                    (Some(schema), Ok(url)) => Db::connect(&url, schema, PgOptions { max_connections: 4, ..PgOptions::default() }).unwrap(),
                    _ => db.clone(),
                };
                Box::new(Postgres::new(db))
            }
            Store::Dynamo { endpoint, table } => {
                let mut c = DynamoDbConfig::new(table.clone());
                c.endpoint = Some(endpoint.clone());
                c.create_table = true;
                Box::new(DynamoDb::connect(c).unwrap())
            }
        }
    }

    /// Test timings; twice as long on a real Postgres or DynamoDB Local, where a call on a busy
    /// machine (or a pool's first connection) can take a good part of a second.
    pub fn config(&self, id: &str) -> ClusterConfig {
        self.scaled(ClusterConfig::for_tests(id, "http://127.0.0.1:9", SECRET))
    }

    pub fn scale(&self) -> u32 {
        match &self.store {
            Store::Memory(_) => 1,
            Store::Sql(db, _) => {
                if db.pg_schema().is_some() {
                    2
                } else {
                    1
                }
            }
            Store::Dynamo { .. } => 2,
        }
    }

    pub fn scaled(&self, mut c: ClusterConfig) -> ClusterConfig {
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

    pub fn join(&self, id: &str) -> Arc<Cluster> {
        self.join_with(self.config(id))
    }

    pub fn join_with(&self, cfg: ClusterConfig) -> Arc<Cluster> {
        Cluster::join(self.backend(), cfg).unwrap()
    }
}

/// The lease record of `key` as `node` reads it: `(holder, epoch)`.
pub fn record(node: &Cluster, key: &str) -> Option<(String, i64)> {
    node.lease_record(key).unwrap()
}

fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    h.finish()
}

const CONTAINER: &str = "roll-cloud-cluster-ddb";

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker").args(args).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

extern "C" fn remove_container() {
    let _ = Command::new("docker").args(["rm", "-f", CONTAINER]).output();
}

/// DynamoDB Local's endpoint for this test run (see the module docs).
pub fn dynamodb_endpoint() -> Option<String> {
    static EP: OnceLock<Option<String>> = OnceLock::new();
    let ep = EP.get_or_init(|| {
        if let Ok(e) = std::env::var("ROLL_CLUSTER_TEST_DYNAMODB")
            && !e.is_empty()
        {
            return Some(e);
        }
        let port = |s: &str| s.lines().next().and_then(|l| l.rsplit(':').next()).map(str::to_string);
        let existing = docker(&["port", CONTAINER, "8000/tcp"]).ok().and_then(|s| port(&s));
        let p = match existing {
            Some(p) => p,
            None => {
                if let Err(e) = docker(&["run", "-d", "--rm", "--name", CONTAINER, "-p", "127.0.0.1::8000", "amazon/dynamodb-local"]) {
                    eprintln!("skipping the DynamoDB backend's tests: Docker can't start amazon/dynamodb-local ({e})");
                    return None;
                }
                // SAFETY: registering a plain extern "C" function to run at exit.
                unsafe { libc::atexit(remove_container) };
                port(&docker(&["port", CONTAINER, "8000/tcp"]).ok()?)?
            }
        };
        let addr = format!("127.0.0.1:{p}");
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(&addr).is_err() {
            if Instant::now() > deadline {
                eprintln!("skipping the DynamoDB backend's tests: DynamoDB Local never listened on {addr}");
                return None;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Some(format!("http://{addr}"))
    });
    if ep.is_none() && std::env::var("ROLL_CLUSTER_DYNAMODB_REQUIRED").is_ok_and(|v| v == "1") {
        panic!("DynamoDB Local is required (ROLL_CLUSTER_DYNAMODB_REQUIRED=1) but isn't available");
    }
    ep.clone()
}

/// A key (`{prefix}{n}`) whose preferred node among `nodes` is `want`.
pub fn key_for(nodes: &[&str], want: &str, prefix: &str) -> String {
    keys_for(nodes, want, prefix, 1).remove(0)
}

pub fn keys_for(nodes: &[&str], want: &str, prefix: &str, n: usize) -> Vec<String> {
    (0..).map(|i| format!("{prefix}{i}")).filter(|k| placement::rendezvous(k, nodes.iter().copied()) == Some(want)).take(n).collect()
}

/// Poll `f` until it's true or `timeout` passes; whether it became true.
pub fn eventually(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Everything received so far.
pub fn drain_events<T>(rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> Vec<T> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
}

/// Wait until every node sees all of them as live (views converge within a heartbeat).
pub fn converge(nodes: &[Arc<Cluster>]) {
    let n = nodes.len();
    assert!(eventually(Duration::from_secs(10), || nodes.iter().all(|c| c.nodes().len() == n)), "views never converged");
}

/// Records, from a thread of its own, when each node starts and stops owning `key` (on this
/// process's clock): the spans never overlap if fencing works.
pub struct Spans {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<Vec<(String, Instant, Instant)>>>,
}

impl Spans {
    pub fn watch(nodes: &[Arc<Cluster>], key: &str) -> Spans {
        let (nodes, key) = (nodes.to_vec(), key.to_string());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = stop.clone();
        let handle = std::thread::spawn(move || {
            // Per node: (first seen owning, last seen owning) of the current span.
            let mut open: Vec<Option<(Instant, Instant)>> = vec![None; nodes.len()];
            let mut spans = Vec::new();
            loop {
                let stopping = s.load(std::sync::atomic::Ordering::SeqCst);
                for (i, n) in nodes.iter().enumerate() {
                    let owns = n.owns(&key);
                    let t = Instant::now();
                    match (open[i], owns) {
                        (None, true) => open[i] = Some((t, t)),
                        (Some((from, _)), true) => open[i] = Some((from, t)),
                        (Some((from, last)), false) => {
                            spans.push((n.node_id().to_string(), from, last));
                            open[i] = None;
                        }
                        _ => {}
                    }
                }
                if stopping {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            for (i, o) in open.iter().enumerate() {
                if let Some((from, last)) = o {
                    spans.push((nodes[i].node_id().to_string(), *from, *last));
                }
            }
            spans
        });
        Spans { stop, handle: Some(handle) }
    }

    /// Every span `(node, from, to)`; panics if two nodes' spans overlap.
    pub fn stop(mut self) -> Vec<(String, Instant, Instant)> {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let spans = self.handle.take().unwrap().join().unwrap();
        for a in &spans {
            for b in &spans {
                if a.0 != b.0 {
                    assert!(a.2 <= b.1 || b.2 <= a.1, "{} and {} owned the key at once: {spans:?}", a.0, b.0);
                }
            }
        }
        spans
    }
}
