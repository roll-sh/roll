//! Test harness: in-process keepers on `127.0.0.1:0` with tempdirs.
#![allow(dead_code)]

use bytes::Bytes;
use mq_core::transport::{Transport, TransportConfig};
use mq_storage::client::KeeperClient;
use mq_storage::proto::{LogState, Reply, Request};
use mq_storage::{Keeper, KeeperConfig, QuorumConfig};
use std::net::SocketAddr;
use std::time::Duration;
use tempfile::TempDir;

pub struct Cluster {
    pub dirs: Vec<TempDir>,
    pub addrs: Vec<SocketAddr>,
    pub keepers: Vec<Option<Keeper>>,
    pub fsync: bool,
    /// Keepers and writers share a secret, so every test goes through the handshake.
    pub transport: std::sync::Arc<Transport>,
}

pub fn secret(s: &str) -> std::sync::Arc<Transport> {
    Transport::new("keeper", &TransportConfig { secret: Some(s.into()), ..Default::default() }).unwrap()
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt().with_env_filter("mq_storage=info").with_test_writer().try_init();
}

impl Cluster {
    pub async fn start(n: usize) -> Self {
        Self::start_with(n, false).await
    }

    pub async fn start_with(n: usize, fsync: bool) -> Self {
        init_tracing();
        let mut c = Cluster { dirs: Vec::new(), addrs: Vec::new(), keepers: Vec::new(), fsync, transport: secret("test-secret") };
        for _ in 0..n {
            let dir = tempfile::tempdir().unwrap();
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            c.addrs.push(l.local_addr().unwrap());
            c.keepers.push(Some(Keeper::start(l, c.keeper_config(dir.path())).await.unwrap()));
            c.dirs.push(dir);
        }
        c
    }

    fn keeper_config(&self, dir: &std::path::Path) -> KeeperConfig {
        KeeperConfig { dir: dir.to_path_buf(), az: String::new(), fsync: self.fsync, shards: 2, transport: self.transport.clone() }
    }

    pub async fn kill(&mut self, i: usize) {
        if let Some(k) = self.keepers[i].take() {
            k.stop().await;
        }
    }

    pub async fn restart(&mut self, i: usize) {
        let mut tries = 0;
        let l = loop {
            match tokio::net::TcpListener::bind(self.addrs[i]).await {
                Ok(l) => break l,
                Err(e) if tries < 50 => {
                    tries += 1;
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => panic!("rebind {}: {e}", self.addrs[i]),
            }
        };
        let cfg = self.keeper_config(self.dirs[i].path());
        self.keepers[i] = Some(Keeper::start(l, cfg).await.unwrap());
    }

    pub fn config(&self) -> QuorumConfig {
        QuorumConfig {
            keepers: self.addrs.iter().enumerate().map(|(i, a)| (*a, format!("az{i}"))).collect(),
            write_quorum: 2,
            az: "az0".into(),
            append_timeout: Duration::from_secs(5),
            rpc_timeout: Duration::from_secs(2),
            transport: self.transport.clone(),
            ..Default::default()
        }
    }

    pub fn client(&self, i: usize) -> std::sync::Arc<KeeperClient> {
        KeeperClient::with_transport(self.addrs[i], "", Duration::from_secs(2), self.transport.clone())
    }

    /// Everything keeper `i` holds for `log` from `from`.
    pub async fn contents(&self, i: usize, log: &str, from: u64) -> Vec<Bytes> {
        let c = self.client(i);
        let mut out = Vec::new();
        let mut at = from;
        loop {
            match c.call(&Request::Read { log: log.into(), from: at, max: 10_000, max_bytes: 1 << 24 }).await.unwrap() {
                Reply::Entries { first, entries } => {
                    assert_eq!(first, at, "keeper {i} skipped entries");
                    if entries.is_empty() {
                        return out;
                    }
                    at += entries.len() as u64;
                    out.extend(entries);
                }
                other => panic!("keeper {i}: {other:?}"),
            }
        }
    }

    pub async fn state(&self, i: usize, log: &str) -> LogState {
        match self.client(i).call(&Request::State { log: log.into() }).await.unwrap() {
            Reply::State(s) => s,
            other => panic!("{other:?}"),
        }
    }
}

pub fn entries(prefix: &str, r: std::ops::Range<usize>) -> Vec<Bytes> {
    r.map(|i| Bytes::from(format!("{prefix}{i}"))).collect()
}

/// Poll until `f` is true (or fail after `secs`).
pub async fn eventually(secs: u64, what: &str, mut f: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !f().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
