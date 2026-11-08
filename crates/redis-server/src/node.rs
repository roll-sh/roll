//! A multi-tenant node: many databases in one process, loaded on demand and unloaded when idle.
//!
//! - **Routing.** A connection names its database by TLS SNI (`<db>.redis.example.com`), by
//!   username (`user@<db>`), or by a self-identifying password/token (`<db>_<secret>`, split at
//!   the last `_`), so any
//!   Redis client or HTTP SDK works with a plain URL.
//! - **Scale to zero.** A database with no connections for `idle_timeout` is flushed to object
//!   storage and dropped from memory. The next AUTH (or HTTP request) restores it.
//! - **Snapshots.** A database snapshots once its log since the last snapshot outgrows the
//!   data (or spans many segments), and when it unloads with more than a few segments of log,
//!   so loads replay little. Branches snapshot early, which also ends their dependency on the
//!   parent's history.
//! - **Catalog.** Which databases exist and their credentials come from a [`Catalog`]: the
//!   control plane in production, a static map for local use.
//! - **Owners.** In a cluster an [`Owner`] says which databases are this node's: a database is
//!   loaded only under a [`Tenure`], and when the tenure ends (a handoff, a lapsed lease) it moves
//!   away: connections close over `handoff_spread`, everything is flushed, the final manifest is
//!   written, and only then is the tenure let go. A database another node took over regardless
//!   (storage fenced this node, see [`persist`]) is dropped at once.

use crate::database::Database;
use crate::directory::{AuthError, AuthRequest, Directory, Session};
use crate::persist::{self, Shipper};
use crate::repl::Follower;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use redis_engine::Config;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::OnceCell;

/// What a node needs to know to serve a database.
#[derive(Clone, Debug)]
pub struct DbSpec {
    pub id: String,
    pub config: Config,
    pub shards: usize,
    /// Accepted secrets (password, or the part after `<db>_` of a token).
    pub secrets: Vec<Secret>,
    /// A read replica of this database: restored from its history, then follows its log.
    pub replica_of: Option<String>,
    /// Acknowledge writes only once they are in object storage.
    pub sync_writes: bool,
    /// Unload after this long without clients (`None`: the node's default).
    pub idle_timeout: Option<Duration>,
    /// Point-in-time restore window: older snapshots and log are dropped.
    pub history_retention: Duration,
}

impl DbSpec {
    pub fn new(id: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: Config::default(),
            shards: 1,
            secrets: vec![Secret { value: secret.into(), read_only: false }],
            replica_of: None,
            sync_writes: false,
            idle_timeout: None,
            history_retention: Duration::from_secs(7 * 86_400),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Secret {
    pub value: String,
    /// Read-only credential (replica endpoints, analytics, browser tokens).
    pub read_only: bool,
}

pub trait Catalog: Send + Sync + 'static {
    fn lookup(&self, db: &str) -> BoxFuture<'_, Option<DbSpec>>;
}

/// A fixed set of databases (local development, tests, single-tenant deployments).
#[derive(Default)]
pub struct StaticCatalog(pub Mutex<HashMap<String, DbSpec>>);

impl StaticCatalog {
    pub fn insert(&self, spec: DbSpec) {
        self.0.lock().insert(spec.id.clone(), spec);
    }
}

impl Catalog for StaticCatalog {
    fn lookup(&self, db: &str) -> BoxFuture<'_, Option<DbSpec>> {
        let spec = self.0.lock().get(db).cloned();
        Box::pin(async move { spec })
    }
}

pub struct NodeConfig {
    /// Unload databases idle (no connections, no commands) this long. `None`: never.
    pub idle_timeout: Option<Duration>,
    /// DNS suffix of database hostnames, for SNI routing (`redis.example.com`).
    pub domain: Option<String>,
    pub snapshots: SnapshotPolicy,
    /// A database moving to another node closes its connections over this long, each at its own
    /// moment, so its clients don't all reconnect at once.
    pub handoff_spread: Duration,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self { idle_timeout: Some(Duration::from_secs(300)), domain: None, snapshots: SnapshotPolicy::default(), handoff_spread: Duration::from_secs(2) }
    }
}

/// When loaded databases snapshot (with a codec on the shipper).
#[derive(Clone, Debug)]
pub struct SnapshotPolicy {
    /// Snapshot once the log since the last one exceeds both this and the data's size (a
    /// snapshot costs about the data's size, so the log never costs more than twice it).
    pub min_log_bytes: u64,
    /// …or spans this many segments (each is a GET when loading).
    pub max_segments: usize,
    /// On unload, snapshot if the log spans more segments than this.
    pub unload_segments: usize,
    pub check_interval: Duration,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self { min_log_bytes: 64 << 20, max_segments: 1000, unload_segments: 8, check_interval: Duration::from_secs(10) }
    }
}

/// Which node serves a database: roll's cluster layer in a fleet (`redis-control`). A node
/// without one serves every database it's asked for.
pub trait Owner: Send + Sync + 'static {
    /// Hold `db` (a replica: of `primary`) on this node for as long as the tenure lives, waiting
    /// out a move in progress.
    fn claim<'a>(&'a self, db: &'a str, primary: Option<&'a str>) -> BoxFuture<'a, Result<Box<dyn Tenure>, Refused>>;
}

/// Why a database isn't served here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Another node serves it (this node, say, just handed it over): a connection closes so its
    /// client reconnects there.
    Elsewhere(String),
    /// It's still moving between nodes after waiting a lease's length.
    Moving(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Elsewhere(node) => write!(f, "the database is served by node {node}"),
            Refused::Moving(db) => write!(f, "database {db} is moving between nodes; retry"),
        }
    }
}

impl std::error::Error for Refused {}

/// A database's stay on this node.
pub trait Tenure: Send + Sync + 'static {
    /// Resolves when the database must leave this node: handed off, or no longer this node's.
    fn ending(&self) -> BoxFuture<'static, ()>;
    fn is_ending(&self) -> bool;
}

type Slot = Arc<OnceCell<Arc<Database>>>;

pub struct Node {
    pub catalog: Arc<dyn Catalog>,
    /// Durability; without it databases live only in memory and are never unloaded.
    pub shipper: Option<Arc<Shipper>>,
    pub cfg: NodeConfig,
    dbs: Mutex<HashMap<String, Slot>>,
    followers: Mutex<HashMap<String, Arc<Follower>>>,
    idle_overrides: Mutex<HashMap<String, Duration>>,
    retention: Mutex<HashMap<String, Duration>>,
    /// How often replicas poll object storage for new log segments.
    pub replica_poll: Duration,
    /// Serialises loads and unloads of the same database.
    transitions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    owner: std::sync::OnceLock<Arc<dyn Owner>>,
    /// The loaded databases' tenures (with an [`Owner`]).
    tenures: Mutex<HashMap<String, Box<dyn Tenure>>>,
    me: std::sync::Weak<Node>,
}

/// Which database a connection wants, and the secret to check.
pub fn route<'a>(req: &AuthRequest<'a>, domain: Option<&str>) -> Option<(String, Option<String>, &'a [u8])> {
    let user = req.user.map(|u| String::from_utf8_lossy(u).into_owned());
    // `user@db` names the database explicitly.
    if let Some(u) = &user
        && let Some((name, db)) = u.rsplit_once('@')
    {
        return Some((db.to_string(), Some(name.to_string()), req.password));
    }
    if let Some(db) = req.sni.and_then(|sni| sni_db(sni, domain)) {
        return Some((db, user, req.password));
    }
    // `<db>_<secret>` tokens (database ids may contain `_`, secrets don't).
    let pos = req.password.iter().rposition(|&b| b == b'_')?;
    let db = std::str::from_utf8(&req.password[..pos]).ok()?;
    Some((db.to_string(), user, &req.password[pos + 1..]))
}

/// The database a TLS server name names: its first label (under `domain`, when given).
pub fn sni_db(sni: &str, domain: Option<&str>) -> Option<String> {
    let label = match domain {
        Some(d) => sni.strip_suffix(d).and_then(|s| s.strip_suffix('.')),
        None => sni.split('.').next(),
    };
    label.filter(|l| !l.is_empty() && !l.contains('.')).map(str::to_string)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Attempts at loading a database whose manifest keeps changing under the load (a previous
/// owner still running, moving to a new log).
const LOAD_ATTEMPTS: usize = 3;

impl Node {
    pub fn new(catalog: Arc<dyn Catalog>, shipper: Option<Arc<Shipper>>, cfg: NodeConfig) -> Arc<Self> {
        let node = Arc::new_cyclic(|me| Self {
            catalog,
            shipper,
            cfg,
            dbs: Mutex::new(HashMap::new()),
            followers: Mutex::new(HashMap::new()),
            idle_overrides: Mutex::new(HashMap::new()),
            retention: Mutex::new(HashMap::new()),
            replica_poll: Duration::from_millis(100),
            transitions: Mutex::new(HashMap::new()),
            owner: std::sync::OnceLock::new(),
            tenures: Mutex::new(HashMap::new()),
            me: me.clone(),
        });
        if let Some(shipper) = &node.shipper {
            // Another node took a database over: it stops here at once.
            let weak = Arc::downgrade(&node);
            shipper.on_fenced(move |id| {
                if let Some(node) = weak.upgrade() {
                    let id = id.to_string();
                    tokio::spawn(async move { node.drop_fenced(&id).await });
                }
            });
        }
        if let (Some(idle), Some(_)) = (node.cfg.idle_timeout, &node.shipper) {
            let weak = Arc::downgrade(&node);
            tokio::spawn(async move {
                let tick = (idle / 4).clamp(Duration::from_millis(50), Duration::from_secs(15));
                loop {
                    tokio::time::sleep(tick).await;
                    let Some(node) = weak.upgrade() else { return };
                    for id in node.idle(idle) {
                        if let Err(e) = node.unload(&id).await {
                            tracing::warn!(db = id, error = %e, "unload failed");
                        }
                    }
                }
            });
        }
        if node.shipper.as_ref().is_some_and(|s| s.codec().is_some()) {
            let weak = Arc::downgrade(&node);
            let every = node.cfg.snapshots.check_interval;
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    let Some(node) = weak.upgrade() else { return };
                    node.snapshot_due().await;
                }
            });
        }
        node
    }

    /// Serve databases only while `owner` says they're this node's (a cluster). Set once, before
    /// anything is loaded.
    pub fn set_owner(&self, owner: Arc<dyn Owner>) {
        let _ = self.owner.set(owner);
    }

    /// Snapshot the loaded primaries whose log outgrew the policy, one at a time.
    async fn snapshot_due(&self) {
        let Some(shipper) = &self.shipper else { return };
        let p = &self.cfg.snapshots;
        for db in self.loaded() {
            if self.followers.lock().contains_key(&db.id) || db.closed.load(Ordering::SeqCst) {
                continue;
            }
            let Some(st) = shipper.log_stats(&db.id) else { continue };
            let due = st.based || st.segments >= p.max_segments || st.bytes >= p.min_log_bytes.max(db.used_memory() as u64);
            if due && let Err(e) = shipper.snapshot(&db, self.retention_ms(&db.id)).await {
                tracing::warn!(db = db.id, error = %e, "snapshot failed");
            }
        }
    }

    fn retention_ms(&self, id: &str) -> i64 {
        self.retention.lock().get(id).map_or(7 * 86_400_000, |d| d.as_millis() as i64)
    }

    fn transition_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.transitions.lock().entry(id.to_string()).or_default().clone()
    }

    /// Loaded databases.
    pub fn loaded(&self) -> Vec<Arc<Database>> {
        self.dbs.lock().values().filter_map(|s| s.get().cloned()).collect()
    }

    pub fn get_loaded(&self, id: &str) -> Option<Arc<Database>> {
        self.dbs.lock().get(id).and_then(|s| s.get().cloned())
    }

    /// Whether `id` is loaded here but on its way to another node (it takes no new sessions).
    fn leaving(&self, id: &str) -> bool {
        self.tenures.lock().get(id).is_some_and(|t| t.is_ending())
    }

    fn idle(&self, default: Duration) -> Vec<String> {
        let overrides = self.idle_overrides.lock().clone();
        self.loaded()
            .into_iter()
            .filter(|d| {
                let idle = overrides.get(&d.id).copied().unwrap_or(default);
                d.connections.load(Ordering::SeqCst) == 0 && !idle.is_zero() && d.idle_ms() >= idle.as_millis() as u64
            })
            .map(|d| d.id.clone())
            .collect()
    }

    /// Change a database's idle timeout (zero: never unload).
    pub fn set_idle_timeout(&self, id: &str, idle: Option<Duration>) {
        match idle {
            Some(d) => self.idle_overrides.lock().insert(id.to_string(), d),
            None => self.idle_overrides.lock().remove(id),
        };
    }

    /// The database, restoring it from object storage if it isn't in memory. With an
    /// [`Owner`], only while it's this node's.
    pub async fn open(&self, spec: &DbSpec) -> anyhow::Result<Arc<Database>> {
        loop {
            if self.leaving(&spec.id) {
                // Moving away: no new sessions here. Once it's gone, ask where it is now.
                drop(self.transition_lock(&spec.id).lock().await);
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            let slot = self.dbs.lock().entry(spec.id.clone()).or_default().clone();
            // The tenure a fresh load took, held once the database is in its slot.
            let fresh: Mutex<Option<Box<dyn Tenure>>> = Mutex::new(None);
            let db = slot
                .get_or_try_init(|| async {
                    let lock = self.transition_lock(&spec.id);
                    let _g = lock.lock().await;
                    if let Some(o) = self.owner.get() {
                        let t = o.claim(&spec.id, spec.replica_of.as_deref()).await.map_err(anyhow::Error::from)?;
                        *fresh.lock() = Some(t);
                    }
                    self.load(spec).await
                })
                .await?
                .clone();
            if let Some(t) = fresh.lock().take() {
                self.hold(&db, t);
            }
            if !db.closed.load(Ordering::SeqCst) {
                return Ok(db);
            }
            // Unloading right now: wait for it to finish, then load again.
            let lock = self.transition_lock(&spec.id);
            drop(lock.lock().await);
        }
    }

    /// Build a database from object storage and start shipping (or following) it.
    async fn load(&self, spec: &DbSpec) -> anyhow::Result<Arc<Database>> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let db = Database::new(spec.id.clone(), spec.config.clone(), spec.shards);
            db.sync_writes.store(spec.sync_writes, Ordering::Relaxed);
            self.set_idle_timeout(&spec.id, spec.idle_timeout);
            self.retention.lock().insert(spec.id.clone(), spec.history_retention);
            if let Some(primary) = &spec.replica_of {
                let Some(shipper) = &self.shipper else { anyhow::bail!("replicas need object storage") };
                persist::restore_at(&shipper.objects, primary, &db, persist::Until::LATEST, shipper.codec().as_deref()).await?;
                let f = Follower::with_codec(db.clone(), shipper.codec());
                tokio::spawn(f.clone().tail_storage(shipper.objects.clone(), primary.clone(), self.replica_poll));
                self.followers.lock().insert(spec.id.clone(), f);
                db.start_cron();
                return Ok(db);
            }
            if let Some(shipper) = &self.shipper {
                let started = std::time::Instant::now();
                let m = persist::restore(&shipper.objects, &db, shipper.codec().as_deref()).await?;
                if let Err(e) = shipper.attach(&db, m).await {
                    if attempt < LOAD_ATTEMPTS {
                        tracing::info!(db = spec.id, error = %e, "loading again");
                        continue;
                    }
                    return Err(e);
                }
                tracing::info!(db = spec.id, keys = db.total_keys(), ms = started.elapsed().as_millis() as u64, "database loaded");
            }
            db.start_cron();
            return Ok(db);
        }
    }

    /// Keep `db` here while `tenure` lasts; when it ends, the database moves away.
    fn hold(&self, db: &Arc<Database>, tenure: Box<dyn Tenure>) {
        let ending = tenure.ending();
        self.tenures.lock().insert(db.id.clone(), tenure);
        let (me, weak_db) = (self.me.clone(), Arc::downgrade(db));
        tokio::spawn(async move {
            ending.await;
            if let (Some(node), Some(db)) = (me.upgrade(), weak_db.upgrade()) {
                node.move_away(&db).await;
            }
        });
    }

    /// Forget a database that is no longer loaded here (its tenure ends with it).
    fn forget(&self, id: &str) {
        self.dbs.lock().remove(id);
        self.retention.lock().remove(id);
        self.tenures.lock().remove(id);
    }

    /// Hand a database over to its next node: its connections close over `handoff_spread`, each
    /// at its own moment (commands in flight finish and are answered first), then everything it
    /// wrote is flushed and its final manifest written, and only then does its tenure end, so the
    /// next owner loads every acknowledged write.
    async fn move_away(&self, db: &Arc<Database>) {
        let lock = self.transition_lock(&db.id);
        let _g = lock.lock().await;
        if !self.get_loaded(&db.id).is_some_and(|d| Arc::ptr_eq(&d, db)) {
            return;
        }
        let spread = self.cfg.handoff_spread;
        tracing::info!(db = db.id, clients = db.connections.load(Ordering::SeqCst), "handing the database to another node");
        db.kick_gradually(spread);
        let until = std::time::Instant::now() + spread + Duration::from_secs(1);
        while db.connections.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Whatever is left (a long script, a slow socket) goes now.
        db.close_and_kick();
        for _ in 0..500 {
            if db.connections.load(Ordering::SeqCst) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if let Some(f) = self.followers.lock().remove(&db.id) {
            f.stop();
        } else if let Some(shipper) = &self.shipper {
            let mut tries: u64 = 0;
            while let Err(e) = shipper.detach(db).await {
                tries += 1;
                if tries >= 3 {
                    // The next owner fences this node's log and replays what reached storage.
                    tracing::warn!(db = db.id, error = %e, "couldn't flush the database before handing it over");
                    shipper.abandon(db);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200 * tries)).await;
            }
        }
        self.forget(&db.id);
        tracing::info!(db = db.id, "database handed over");
    }

    /// Another node took `id` over (its manifest moved on): disconnect everyone at once. Nothing
    /// is flushed; what wasn't durable here was never this node's to keep.
    async fn drop_fenced(&self, id: &str) {
        let lock = self.transition_lock(id);
        let _g = lock.lock().await;
        let Some(db) = self.get_loaded(id) else { return };
        db.close_and_kick();
        for _ in 0..500 {
            if db.connections.load(Ordering::SeqCst) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if let Some(shipper) = &self.shipper {
            shipper.abandon(&db);
        }
        self.followers.lock().remove(id);
        self.forget(id);
    }

    /// Flush a database to object storage and drop it from memory, unless it is in use.
    /// Returns whether it was unloaded.
    pub async fn unload(&self, id: &str) -> anyhow::Result<bool> {
        self.unload_inner(id, false).await
    }

    /// Unload even if clients are connected: they are disconnected first (and reconnect to the
    /// reloaded database). Used for deletes, in-place restores and moves.
    pub async fn evict(&self, id: &str) -> anyhow::Result<bool> {
        self.unload_inner(id, true).await
    }

    async fn unload_inner(&self, id: &str, force: bool) -> anyhow::Result<bool> {
        let Some(shipper) = &self.shipper else { return Ok(false) };
        let lock = self.transition_lock(id);
        let _g = lock.lock().await;
        let Some(db) = self.get_loaded(id) else { return Ok(false) };
        if force {
            db.close_and_kick();
            // In-flight HTTP requests and connections finish their current batch.
            for _ in 0..500 {
                if db.connections.load(Ordering::SeqCst) == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } else {
            db.closed.store(true, Ordering::SeqCst);
        }
        if db.connections.load(Ordering::SeqCst) > 0 {
            db.closed.store(false, Ordering::SeqCst);
            db.evicting.store(false, Ordering::SeqCst);
            if force {
                anyhow::bail!("database {id} still has clients after disconnecting them");
            }
            return Ok(false);
        }
        let follower = self.followers.lock().remove(id);
        if let Some(f) = follower {
            // Replicas have nothing to flush: they're rebuilt from the primary's log.
            f.stop();
        } else if let Err(e) = async {
            // Nobody is waiting on the database now: a good time to make the next load cheap.
            let st = shipper.log_stats(id).unwrap_or_default();
            if (st.based || st.segments > self.cfg.snapshots.unload_segments)
                && let Err(e) = shipper.snapshot(&db, self.retention_ms(id)).await
            {
                tracing::warn!(db = id, error = %e, "snapshot on unload failed");
            }
            shipper.detach(&db).await
        }
        .await
        {
            db.closed.store(false, Ordering::SeqCst);
            return Err(e);
        }
        self.forget(id);
        tracing::info!(db = id, "database unloaded");
        Ok(true)
    }

    /// Create database `child` from `parent`'s state as of `until` (branching / point-in-time
    /// restore into a new database). Copies nothing: the child's manifest points at the parent's
    /// history. Returns the stream offset of the branch point.
    pub async fn branch(&self, parent: &str, child: &str, until: persist::Until) -> anyhow::Result<u64> {
        let Some(shipper) = &self.shipper else { anyhow::bail!("branching needs object storage") };
        if persist::read_manifest(&shipper.objects, child).await?.is_some() {
            anyhow::bail!("database {child} already exists");
        }
        // Make the parent's latest writes visible in storage.
        shipper.flush().await?;
        shipper.write_manifests(false).await?;
        let scratch = Database::new(child, Config::default(), 1);
        let m = persist::restore_at(&shipper.objects, parent, &scratch, until, shipper.codec().as_deref()).await?;
        let manifest = persist::Manifest {
            base: Some(persist::Base {
                db: parent.to_string(),
                offset: m.end,
                at_ms: until.time_ms.unwrap_or_else(redis_engine::util::now_ms),
                history: Some(Box::new(persist::history(&m, parent, m.end))),
            }),
            end: m.end,
            ..Default::default()
        };
        if !persist::create_manifest(&shipper.objects, child, &manifest).await? {
            anyhow::bail!("database {child} already exists");
        }
        Ok(m.end)
    }

    /// Restore database `id` in place to `until`. Clients are disconnected; the current history
    /// is kept under a new id (returned) so the restore itself can be undone by branching from it.
    pub async fn restore_in_place(&self, id: &str, until: persist::Until) -> anyhow::Result<String> {
        let Some(shipper) = &self.shipper else { anyhow::bail!("restores need object storage") };
        // Disconnect clients and flush (no-op when not loaded).
        self.evict(id).await?;
        let lock = self.transition_lock(id);
        let _g = lock.lock().await;
        if self.get_loaded(id).is_some() {
            anyhow::bail!("database {id} was reopened by a client during the restore; retry");
        }
        let Some(current) = persist::read_manifest(&shipper.objects, id).await? else {
            anyhow::bail!("database {id} has no history yet");
        };
        // A manifest that still names a log (its owner crashed): nothing more goes into that
        // log, so the archive's view of it is final.
        if let Some(log) = &current.node {
            persist::fence_log(&shipper.objects, log, current.after_seq).await?;
        }
        let archive = format!("{id}.{}", redis_engine::util::now_ms());
        let frozen = persist::Manifest { stream: Some(current.stream.clone().unwrap_or_else(|| id.to_string())), version: None, ..current.clone() };
        if !persist::create_manifest(&shipper.objects, &archive, &frozen).await? {
            anyhow::bail!("restore archive {archive} exists already; retry");
        }
        let scratch = Database::new(&archive, Config::default(), 1);
        let m = persist::restore_at(&shipper.objects, &archive, &scratch, until, shipper.codec().as_deref()).await?;
        let manifest = persist::Manifest {
            base: Some(persist::Base {
                db: archive.clone(),
                offset: m.end,
                at_ms: until.time_ms.unwrap_or_else(redis_engine::util::now_ms),
                history: Some(Box::new(persist::history(&m, &archive, m.end))),
            }),
            end: m.end,
            version: current.version.clone(),
            ..Default::default()
        };
        if !persist::replace_manifest(&shipper.objects, id, &manifest).await? {
            anyhow::bail!("database {id} changed during the restore; retry");
        }
        Ok(archive)
    }

    pub fn follower(&self, id: &str) -> Option<Arc<Follower>> {
        self.followers.lock().get(id).cloned()
    }

}

impl Directory for Node {
    fn authenticate<'a>(&'a self, req: AuthRequest<'a>) -> BoxFuture<'a, Result<Session, AuthError>> {
        Box::pin(async move {
            let (db, user, secret) = route(&req, self.cfg.domain.as_deref()).ok_or(AuthError::WrongPass)?;
            let spec = self.catalog.lookup(&db).await.ok_or(AuthError::WrongPass)?;
            let full = req.password;
            let platform = spec.secrets.iter().any(|s| ct_eq(s.value.as_bytes(), secret) || ct_eq(s.value.as_bytes(), full));
            let is_default = user.as_deref().is_none_or(|u| u == "default");
            // Only known databases are loaded: a wrong token for an existing database still
            // wakes it, but that is needed to check tenant-defined ACL users anyway.
            loop {
                let db = self.open(&spec).await.map_err(|e| match e.downcast_ref::<Refused>() {
                    Some(Refused::Elsewhere(_)) => AuthError::Moved,
                    _ => AuthError::Unavailable(e.to_string()),
                })?;
                let user = {
                    let acl = db.acl.read();
                    if platform && is_default {
                        // The platform credential is the default user, as long as the tenant
                        // hasn't disabled it.
                        let u = acl.default_user();
                        if !u.enabled() {
                            return Err(AuthError::WrongPass);
                        }
                        u
                    } else {
                        match acl.authenticate(user.as_deref().map(str::as_bytes), secret) {
                            Ok(u) => u,
                            // Including `AUTH <token>` against a nopass default user.
                            Err(_) => {
                                if let Some(c) = req.client {
                                    acl.log_auth_failure(c, req.command, user.as_deref().unwrap_or("default").as_bytes());
                                }
                                return Err(AuthError::WrongPass);
                            }
                        }
                    }
                };
                if let Some(s) = Session::new(db, user) {
                    return Ok(s);
                }
            }
        })
    }

    fn anonymous(&self) -> BoxFuture<'_, Option<Session>> {
        Box::pin(async { None })
    }

    /// Unload the database (flushed to object storage): the next connection loads it afresh, like
    /// a restarted server. Without object storage its clients are just disconnected.
    fn shutdown(&self, db: Arc<Database>) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            match self.evict(&db.id).await {
                Ok(true) => {}
                Ok(false) => crate::directory::kick_all(&db).await,
                Err(e) => tracing::warn!(db = db.id, error = %e, "shutdown failed"),
            }
        })
    }
}
