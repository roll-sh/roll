//! A database: one tenant's keyspace, split into shards that each run their own engine.
//!
//! Shards give a database multi-core parallelism (keys are routed by hash slot, like Redis
//! Cluster) and are the unit that moves between nodes when a database scales out.

use parking_lot::{Mutex, RwLock};
use redis_engine::acl::Acl;
use redis_engine::{Config, Engine, Hub};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Shard {
    pub engine: Mutex<Engine>,
}

pub struct Database {
    pub id: String,
    pub shards: Vec<Shard>,
    pub hub: Arc<Hub>,
    /// ACL users and log, shared by every shard's engine.
    pub acl: Arc<RwLock<Acl>>,
    /// Connections currently attached (for scale-to-zero decisions and metering).
    pub connections: AtomicU64,
    /// Unix ms of the last command (idle detection).
    pub last_active: AtomicU64,
    pub commands: AtomicU64,
}

impl Database {
    pub fn new(id: impl Into<String>, config: Config, shards: usize) -> Arc<Self> {
        let hub = Hub::new();
        let acl = Arc::new(RwLock::new(Acl::new()));
        let shards = (0..shards.max(1))
            .map(|i| {
                let mut e = Engine::new(config.clone(), hub.clone());
                e.shard = i as u16;
                e.acl = acl.clone();
                Shard { engine: Mutex::new(e) }
            })
            .collect();
        Arc::new(Self {
            id: id.into(),
            shards,
            hub,
            acl,
            connections: AtomicU64::new(0),
            last_active: AtomicU64::new(redis_engine::util::now_ms() as u64),
            commands: AtomicU64::new(0),
        })
    }

    pub fn shard(&self, i: usize) -> &Shard {
        &self.shards[i]
    }

    pub fn touch(&self, n: u64) {
        self.last_active.store(redis_engine::util::now_ms() as u64, Ordering::Relaxed);
        self.commands.fetch_add(n, Ordering::Relaxed);
    }
}
