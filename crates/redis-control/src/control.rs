//! The Redis control plane: databases and their settings, branches, point-in-time restores,
//! read replicas, credentials, metering and the cost estimate. Serving is done by a
//! [`redis_server::node::Node`] whose catalog is this control plane's store.

use crate::store::{RedisDb, Replica, Store, UsageSample, UsageTotals};
use futures::future::BoxFuture;
use redis_engine::Config as EngineConfig;
use redis_engine::config::Policy;
use redis_server::node::{Catalog, DbSpec, Node, NodeConfig, Secret};
use redis_server::persist::{Objects, Shipper, ShipperConfig, Until};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum ControlError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, ControlError>;

#[derive(Clone, Debug)]
pub struct RedisConfig {
    pub home: PathBuf,
    /// This node's name in object storage (segments are per node).
    pub node_name: String,
    pub region: String,
    /// Host and ports written into connection strings.
    pub public_host: String,
    pub resp_port: u16,
    pub http_port: u16,
    pub tls: bool,
    /// Group-commit interval for the durable log.
    pub flush_interval: Duration,
    pub defaults: Defaults,
    pub pricing: Pricing,
}

#[derive(Clone, Debug, Serialize)]
pub struct Defaults {
    pub max_memory_mb: i64,
    pub eviction_policy: String,
    pub idle_timeout_s: i64,
    pub history_retention_s: i64,
}

/// AWS list prices behind the cost estimate (us-east-1, on-demand, Graviton), and the AWS
/// managed service it replaces for comparison.
#[derive(Clone, Debug, Serialize)]
pub struct Pricing {
    /// r7g.large is 2 vCPU / 16 GiB at $0.1071/h → $0.0067 per GiB-hour, billed only while a
    /// database is loaded (scale to zero bills nothing).
    pub usd_per_gib_hour: f64,
    /// CPU per million commands (~100k simple commands/s per vCPU at $0.0535/vCPU-hour).
    pub usd_per_million_commands: f64,
    /// Snapshots and log on S3 Standard.
    pub usd_per_gb_month_s3: f64,
    /// Internet egress.
    pub usd_per_gb_out: f64,
    /// ElastiCache Serverless for Redis OSS / Valkey: $0.125/GB-hour (Valkey $0.084), with a
    /// 1 GB minimum, and $0.0034 per million ECPUs.
    pub elasticache_usd_per_gb_hour: f64,
    pub elasticache_usd_per_million_ecpu: f64,
    pub elasticache_min_gb: f64,
}

impl RedisConfig {
    pub fn local(home: PathBuf) -> Self {
        Self {
            home,
            node_name: "local".into(),
            region: "local".into(),
            public_host: "127.0.0.1".into(),
            resp_port: 56380,
            http_port: 58080,
            tls: false,
            flush_interval: Duration::from_secs(1),
            defaults: Defaults {
                max_memory_mb: 256,
                eviction_policy: "noeviction".into(),
                idle_timeout_s: 300,
                history_retention_s: 7 * 86_400,
            },
            pricing: Pricing {
                usd_per_gib_hour: 0.0067,
                usd_per_million_commands: 0.00015,
                usd_per_gb_month_s3: 0.023,
                usd_per_gb_out: 0.09,
                elasticache_usd_per_gb_hour: 0.125,
                elasticache_usd_per_million_ecpu: 0.0034,
                elasticache_min_gb: 1.0,
            },
        }
    }
}

pub struct Control {
    pub cfg: RedisConfig,
    pub store: Arc<Store>,
    pub node: Arc<Node>,
    pub shipper: Arc<Shipper>,
}

/// Database settings a client may change.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct DbOptions {
    pub name: Option<String>,
    pub max_memory_mb: Option<i64>,
    pub eviction_policy: Option<String>,
    pub durability: Option<String>,
    pub idle_timeout_s: Option<i64>,
    pub history_retention_s: Option<i64>,
}

const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

fn random_string(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("os randomness");
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", random_string(12))
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 63 && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Serves the node's catalog from the store.
struct StoreCatalog(Arc<Store>);

impl Catalog for StoreCatalog {
    fn lookup(&self, id: &str) -> BoxFuture<'_, Option<DbSpec>> {
        let spec = spec_for(&self.0, id).ok().flatten();
        Box::pin(async move { spec })
    }
}

fn engine_config(d: &RedisDb) -> EngineConfig {
    let mut c = EngineConfig::default();
    let bytes = (d.max_memory_mb.max(1) as u64) << 20;
    c.maxmemory = bytes;
    c.maxmemory_cap = bytes;
    c.maxmemory_policy = Policy::parse(&d.eviction_policy).unwrap_or(Policy::NoEviction);
    c
}

fn spec_for(store: &Store, id: &str) -> anyhow::Result<Option<DbSpec>> {
    if let Some(d) = store.db(id)? {
        return Ok(Some(DbSpec {
            config: engine_config(&d),
            sync_writes: d.durability == "sync",
            idle_timeout: Some(Duration::from_secs(d.idle_timeout_s.max(0) as u64)),
            ..DbSpec::new(d.id.clone(), d.password.clone())
        }));
    }
    if let Some(r) = store.replica(id)?
        && let Some(d) = store.db(&r.db_id)?
    {
        return Ok(Some(DbSpec {
            config: engine_config(&d),
            replica_of: Some(d.id.clone()),
            idle_timeout: Some(Duration::from_secs(d.idle_timeout_s.max(0) as u64)),
            secrets: vec![Secret { value: r.password.clone(), read_only: true }],
            ..DbSpec::new(r.id.clone(), r.password.clone())
        }));
    }
    Ok(None)
}

#[derive(Serialize)]
pub struct Cost {
    pub memory_usd: f64,
    pub compute_usd: f64,
    pub storage_usd: f64,
    pub egress_usd: f64,
    pub total_usd: f64,
    /// The same workload on ElastiCache Serverless (always on, 1 GB minimum).
    pub elasticache_serverless_usd: f64,
}

#[derive(Serialize)]
pub struct Usage {
    pub db_id: String,
    pub days: i64,
    #[serde(flatten)]
    pub totals: UsageTotals,
    /// Durable state in object storage (approximate: the in-memory dataset size).
    pub storage_bytes: i64,
    pub cost: Cost,
}

impl Control {
    pub async fn new(cfg: RedisConfig, objects: Objects) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.home)?;
        let store = Arc::new(Store::open(&cfg.home.join("redis.db"))?);
        let shipper = Shipper::start(&cfg.node_name, objects, ShipperConfig { flush_interval: cfg.flush_interval, ..Default::default() }).await?;
        let node = Node::new(
            Arc::new(StoreCatalog(store.clone())),
            Some(shipper.clone()),
            NodeConfig { idle_timeout: Some(Duration::from_secs(cfg.defaults.idle_timeout_s as u64)), domain: None },
        );
        let control = Arc::new(Self { cfg, store, node, shipper });
        control.clone().start_metering();
        Ok(control)
    }

    pub fn objects(&self) -> &Objects {
        &self.shipper.objects
    }

    /// `redis://` and HTTP connection details for a database or replica.
    pub fn connection(&self, id: &str, password: &str) -> serde_json::Value {
        let scheme = if self.cfg.tls { "rediss" } else { "redis" };
        let http = if self.cfg.tls { "https" } else { "http" };
        let token = format!("{id}_{password}");
        serde_json::json!({
            "redis_url": format!("{scheme}://default:{token}@{}:{}", self.cfg.public_host, self.cfg.resp_port),
            "rest_url": format!("{http}://{}:{}", self.cfg.public_host, self.cfg.http_port),
            "rest_token": token,
            "host": self.cfg.public_host,
            "port": self.cfg.resp_port,
        })
    }

    pub fn get(&self, id_or_name: &str) -> Result<RedisDb> {
        if let Some(d) = self.store.db(id_or_name)? {
            return Ok(d);
        }
        self.store.db_by_name(id_or_name)?.ok_or_else(|| ControlError::NotFound(format!("no Redis database {id_or_name}")))
    }

    pub fn list(&self) -> Result<Vec<RedisDb>> {
        Ok(self.store.dbs()?)
    }

    fn check(&self, d: &RedisDb) -> Result<()> {
        if !valid_name(&d.name) {
            return Err(ControlError::Invalid("name: 1-63 letters, digits, '-' or '_'".into()));
        }
        if !(1..=1 << 20).contains(&d.max_memory_mb) {
            return Err(ControlError::Invalid("max_memory_mb must be between 1 and 1048576".into()));
        }
        if Policy::parse(&d.eviction_policy).is_none() {
            return Err(ControlError::Invalid(format!("unknown eviction policy {}", d.eviction_policy)));
        }
        if d.durability != "async" && d.durability != "sync" {
            return Err(ControlError::Invalid("durability must be async or sync".into()));
        }
        if d.idle_timeout_s < 0 || d.history_retention_s < 0 {
            return Err(ControlError::Invalid("timeouts can't be negative".into()));
        }
        Ok(())
    }

    fn apply(mut d: RedisDb, o: DbOptions) -> RedisDb {
        if let Some(v) = o.name {
            d.name = v;
        }
        if let Some(v) = o.max_memory_mb {
            d.max_memory_mb = v;
        }
        if let Some(v) = o.eviction_policy {
            d.eviction_policy = v.to_ascii_lowercase();
        }
        if let Some(v) = o.durability {
            d.durability = v.to_ascii_lowercase();
        }
        if let Some(v) = o.idle_timeout_s {
            d.idle_timeout_s = v;
        }
        if let Some(v) = o.history_retention_s {
            d.history_retention_s = v;
        }
        d
    }

    fn insert(&self, d: &RedisDb) -> Result<()> {
        self.check(d)?;
        if self.store.db_by_name(&d.name)?.is_some() {
            return Err(ControlError::Conflict(format!("a database named {} already exists", d.name)));
        }
        self.store.insert_db(d)?;
        Ok(())
    }

    pub fn create(&self, name: &str, o: DbOptions) -> Result<RedisDb> {
        let df = &self.cfg.defaults;
        let d = Self::apply(
            RedisDb {
                id: new_id("rdb"),
                name: name.to_string(),
                region: self.cfg.region.clone(),
                password: random_string(32),
                max_memory_mb: df.max_memory_mb,
                eviction_policy: df.eviction_policy.clone(),
                durability: "async".into(),
                idle_timeout_s: df.idle_timeout_s,
                history_retention_s: df.history_retention_s,
                parent_id: None,
                parent_at: None,
                created_at: now(),
            },
            DbOptions { name: None, ..o },
        );
        self.insert(&d)?;
        Ok(d)
    }

    /// Change settings; loaded databases pick them up immediately (no restart).
    pub fn update(&self, id: &str, o: DbOptions) -> Result<RedisDb> {
        let old = self.get(id)?;
        let new_name = o.name.clone();
        let d = Self::apply(old.clone(), o);
        self.check(&d)?;
        if let Some(n) = new_name
            && n != old.name
            && self.store.db_by_name(&n)?.is_some()
        {
            return Err(ControlError::Conflict(format!("a database named {n} already exists")));
        }
        self.store.update_db(&d)?;
        self.node.set_idle_timeout(&d.id, Some(Duration::from_secs(d.idle_timeout_s as u64)));
        let mut targets: Vec<String> = vec![d.id.clone()];
        targets.extend(self.store.replicas(&d.id)?.into_iter().map(|r| r.id));
        let cfg = engine_config(&d);
        for t in targets {
            if let Some(db) = self.node.get_loaded(&t) {
                for s in &db.shards {
                    let mut e = s.engine.lock();
                    e.cfg.maxmemory = cfg.maxmemory;
                    e.cfg.maxmemory_cap = cfg.maxmemory_cap;
                    e.cfg.maxmemory_policy = cfg.maxmemory_policy;
                }
                if t == d.id {
                    db.sync_writes.store(d.durability == "sync", Ordering::Relaxed);
                }
            }
        }
        Ok(d)
    }

    /// New password; current clients stay connected, new connections need the new one.
    pub fn rotate_password(&self, id: &str) -> Result<RedisDb> {
        let mut d = self.get(id)?;
        d.password = random_string(32);
        self.store.update_db(&d)?;
        Ok(d)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        let d = self.get(id)?;
        if self.store.dbs()?.iter().any(|c| c.parent_id.as_deref() == Some(d.id.as_str())) {
            // Branches read their parent's history.
            return Err(ControlError::Conflict(format!("{} has branches; delete them first", d.name)));
        }
        for r in self.store.replicas(&d.id)? {
            self.node.evict(&r.id).await?;
        }
        self.node.evict(&d.id).await?;
        self.store.delete_db(&d.id)?;
        self.objects().delete_prefix(&format!("redis/db/{}/", d.id)).await?;
        Ok(())
    }

    /// A new database with `parent`'s data as of `at` (Unix seconds, default now). Instant: the
    /// branch reads its parent's history until it writes its own.
    pub async fn branch(&self, parent: &str, name: &str, at: Option<i64>) -> Result<RedisDb> {
        let p = self.get(parent)?;
        if let Some(t) = at {
            self.check_window(&p, t)?;
        }
        let d = RedisDb {
            id: new_id("rdb"),
            name: name.to_string(),
            password: random_string(32),
            parent_id: Some(p.id.clone()),
            parent_at: Some(at.unwrap_or_else(now)),
            created_at: now(),
            ..p.clone()
        };
        self.check(&d)?;
        if self.store.db_by_name(name)?.is_some() {
            return Err(ControlError::Conflict(format!("a database named {name} already exists")));
        }
        let until = at.map_or(Until::LATEST, |t| Until::time(t * 1000));
        self.node.branch(&p.id, &d.id, until).await?;
        self.store.insert_db(&d)?;
        Ok(d)
    }

    fn check_window(&self, d: &RedisDb, at: i64) -> Result<()> {
        if at > now() + 1 {
            return Err(ControlError::Invalid("that point in time is in the future".into()));
        }
        let oldest = (now() - d.history_retention_s).max(d.created_at);
        if at < oldest {
            return Err(ControlError::Invalid(format!("outside the restore window (earliest is {oldest})")));
        }
        Ok(())
    }

    /// Restore a database in place to `at` (Unix seconds). Clients are disconnected and
    /// reconnect to the restored state. The pre-restore state stays available: branch from the
    /// returned archive id to get it back.
    pub async fn restore(&self, id: &str, at: i64) -> Result<String> {
        let d = self.get(id)?;
        self.check_window(&d, at)?;
        for r in self.store.replicas(&d.id)? {
            self.node.evict(&r.id).await?;
        }
        Ok(self.node.restore_in_place(&d.id, Until::time(at * 1000)).await?)
    }

    pub fn replicas(&self, id: &str) -> Result<Vec<Replica>> {
        Ok(self.store.replicas(&self.get(id)?.id)?)
    }

    pub fn create_replica(&self, id: &str) -> Result<Replica> {
        let d = self.get(id)?;
        let r = Replica { id: new_id("rre"), db_id: d.id, region: self.cfg.region.clone(), password: random_string(32), created_at: now() };
        self.store.insert_replica(&r)?;
        Ok(r)
    }

    pub async fn delete_replica(&self, id: &str) -> Result<()> {
        let r = self.store.replica(id)?.ok_or_else(|| ControlError::NotFound(format!("no replica {id}")))?;
        self.node.evict(&r.id).await?;
        self.store.delete_replica(&r.id)?;
        Ok(())
    }

    /// Scale to zero now.
    pub async fn unload(&self, id: &str) -> Result<bool> {
        let d = self.get(id)?;
        Ok(self.node.evict(&d.id).await?)
    }

    /// Live state of a database: loaded or not, memory, keys, clients.
    pub fn status(&self, id: &str) -> Result<serde_json::Value> {
        let d = self.get(id)?;
        Ok(match self.node.get_loaded(&d.id) {
            Some(db) => serde_json::json!({
                "state": "active",
                "used_memory": db.used_memory(),
                "keys": db.total_keys(),
                "connections": db.connections.load(Ordering::Relaxed),
                "commands": db.commands.load(Ordering::Relaxed),
                "idle_ms": db.idle_ms(),
                "durable_offset": db.durability.lock().as_ref().map(|d| *d.durable.borrow()),
                "offset": db.repl_offset(),
            }),
            None => serde_json::json!({ "state": "idle" }),
        })
    }

    /// Run one command as the default user (dashboard console).
    pub async fn command(&self, id: &str, args: Vec<bytes::Bytes>) -> Result<redis_engine::Reply> {
        let d = self.get(id)?;
        if args.is_empty() {
            return Err(ControlError::Invalid("empty command".into()));
        }
        let spec = spec_for(&self.store, &d.id)?.ok_or_else(|| ControlError::NotFound(d.id.clone()))?;
        let db = self.node.open(&spec).await?;
        let session = redis_server::Session::new(db, redis_engine::acl::User::default_user()).ok_or_else(|| ControlError::Conflict("database is restarting; retry".into()))?;
        let mut x = redis_server::exec::Detached::new(&session, "console");
        Ok(x.run(vec![args]).await.pop().unwrap_or(redis_engine::Reply::Nil))
    }

    pub fn usage(&self, id: &str, days: i64) -> Result<Usage> {
        let d = self.get(id)?;
        let since = now() - days * 86_400;
        let totals = self.store.usage_since(&d.id, since)?;
        let p = &self.cfg.pricing;
        let storage_bytes = self.node.get_loaded(&d.id).map_or(totals.peak_memory_bytes, |db| db.used_memory() as i64);
        let gb = |b: i64| b as f64 / 1e9;
        let months = days as f64 / 30.0;
        let memory_usd = totals.gib_hours * p.usd_per_gib_hour;
        let compute_usd = totals.commands as f64 / 1e6 * p.usd_per_million_commands;
        let storage_usd = gb(storage_bytes) * p.usd_per_gb_month_s3 * months;
        let egress_usd = gb(totals.net_out_bytes) * p.usd_per_gb_out;
        let hours = days as f64 * 24.0;
        let ec_gb = (totals.peak_memory_bytes as f64 / 1e9).max(p.elasticache_min_gb);
        let elasticache = ec_gb * hours * p.elasticache_usd_per_gb_hour + totals.commands as f64 / 1e6 * p.elasticache_usd_per_million_ecpu + egress_usd;
        let total_usd = memory_usd + compute_usd + storage_usd + egress_usd;
        Ok(Usage {
            db_id: d.id,
            days,
            totals,
            storage_bytes,
            cost: Cost { memory_usd, compute_usd, storage_usd, egress_usd, total_usd, elasticache_serverless_usd: elasticache },
        })
    }

    /// Sample loaded databases every minute: loaded time, memory, commands, traffic.
    fn start_metering(self: Arc<Self>) {
        let weak = Arc::downgrade(&self);
        tokio::spawn(async move {
            let mut last: std::collections::HashMap<String, (u64, u64, u64)> = Default::default();
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(c) = weak.upgrade() else { return };
                if let Err(e) = c.sample(&mut last) {
                    tracing::warn!(error = %e, "redis metering failed");
                }
            }
        });
    }

    /// One metering sample (also called by tests).
    pub fn sample(&self, last: &mut std::collections::HashMap<String, (u64, u64, u64)>) -> anyhow::Result<()> {
        let at = now();
        for db in self.node.loaded() {
            let cur = (db.commands.load(Ordering::Relaxed), db.net_in.load(Ordering::Relaxed), db.net_out.load(Ordering::Relaxed));
            let prev = last.insert(db.id.clone(), cur).unwrap_or_default();
            // A reload resets counters.
            let delta = |c: u64, p: u64| if c >= p { c - p } else { c } as i64;
            // Replicas are metered against their database.
            let owner = match self.store.replica(&db.id)? {
                Some(r) => r.db_id,
                None => db.id.clone(),
            };
            let sample = UsageSample {
                at,
                seconds: 60,
                mem_bytes: db.used_memory() as i64,
                commands: delta(cur.0, prev.0),
                net_in: delta(cur.1, prev.1),
                net_out: delta(cur.2, prev.2),
            };
            self.store.record_usage(&owner, &sample)?;
        }
        Ok(())
    }
}
