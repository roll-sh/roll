//! Shared test helpers. Every test runs on SQLite, or on Postgres when
//! `ROLL_CODE_TEST_DATABASE` names a server (each node then gets a pool of its own on the same
//! schema, as separate processes would).

#![allow(dead_code)]

use code_cluster::{Cluster, ClusterConfig, placement};
use code_sql::{Db, OptionalExtension, PgOptions, Target};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SECRET: &[u8] = b"test cluster secret, 32 bytes!!";

pub struct Env {
    pub db: Db,
    _dir: tempfile::TempDir,
}

impl Env {
    pub fn new(name: &str) -> Env {
        // Logs show up for failing tests only (`cargo test` captures them).
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "code_cluster=info".into()))
            .with_test_writer()
            .try_init();
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_or_test(&Target::Sqlite(dir.path().join("cluster.db")), name).unwrap();
        Env { db, _dir: dir }
    }

    /// A database handle for one node: its own pool on Postgres, a clone on SQLite.
    pub fn node_db(&self) -> Db {
        match (self.db.pg_schema(), std::env::var(code_sql::TEST_DATABASE_ENV)) {
            (Some(schema), Ok(url)) => Db::connect(&url, schema, PgOptions { max_connections: 4, ..PgOptions::default() }).unwrap(),
            _ => self.db.clone(),
        }
    }

    /// Test timings; twice as long on Postgres, where a statement on a busy shared server (or
    /// opening a pool's first connection) can take a good part of a second.
    pub fn config(&self, id: &str) -> ClusterConfig {
        self.scaled(ClusterConfig::for_tests(id, "http://127.0.0.1:9", SECRET))
    }

    pub fn scale(&self) -> u32 {
        if self.db.pg_schema().is_some() { 2 } else { 1 }
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

    /// `lease_ttl` in ms.
    pub fn ttl(&self) -> i64 {
        self.config("x").lease_ttl.as_millis() as i64
    }

    pub fn join(&self, id: &str) -> Arc<Cluster> {
        self.join_with(self.config(id))
    }

    pub fn join_with(&self, cfg: ClusterConfig) -> Arc<Cluster> {
        Cluster::join(self.node_db(), cfg).unwrap()
    }

    /// The database's clock.
    pub fn now(&self) -> i64 {
        self.db.now_ms().unwrap()
    }

    pub fn lease(&self, key: &str) -> Option<Lease> {
        self.db
            .query_row("SELECT node_id, epoch, expires_at FROM code_leases WHERE key = ?1", [key], |r| {
                Ok(Lease { node: r.get(0)?, epoch: r.get(1)?, expires_at: r.get(2)? })
            })
            .optional()
            .unwrap()
    }

    pub fn leases(&self) -> Vec<(String, Lease)> {
        self.db
            .query("SELECT key, node_id, epoch, expires_at FROM code_leases ORDER BY key", (), |r| {
                Ok((r.get(0)?, Lease { node: r.get(1)?, epoch: r.get(2)?, expires_at: r.get(3)? }))
            })
            .unwrap()
    }

    pub fn node_rows(&self) -> Vec<String> {
        self.db.query("SELECT id FROM code_nodes ORDER BY id", (), |r| r.get(0)).unwrap()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub node: String,
    pub epoch: i64,
    pub expires_at: i64,
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
    assert!(eventually(Duration::from_secs(5), || nodes.iter().all(|c| c.nodes().len() == n)), "views never converged");
}

/// Watches one key's lease row from its own thread and records every distinct state with the
/// database time it was first seen.
pub struct Monitor {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<Seen>>,
}

/// Distinct states of a lease row, with the database time each was first seen.
pub type Seen = Vec<(i64, Option<Lease>)>;

impl Monitor {
    pub fn start(env: &Env, key: &str) -> Monitor {
        let db = env.node_db();
        let key = key.to_string();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut seen: Seen = Vec::new();
            loop {
                // One last look after being told to stop, so nothing before `stop` is missed.
                let stopping = s.load(std::sync::atomic::Ordering::SeqCst);
                let now = db.now_ms().unwrap();
                let row = db
                    .query_row("SELECT node_id, epoch, expires_at FROM code_leases WHERE key = ?1", [&key], |r| {
                        Ok(Lease { node: r.get(0)?, epoch: r.get(1)?, expires_at: r.get(2)? })
                    })
                    .optional()
                    .unwrap();
                if seen.last().is_none_or(|(_, l)| *l != row) {
                    seen.push((now, row));
                }
                if stopping {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            seen
        });
        Monitor { stop, handle: Some(handle) }
    }

    pub fn stop(mut self) -> Seen {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.handle.take().unwrap().join().unwrap()
    }
}
