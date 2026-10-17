//! A multi-tenant node: many databases in one process, loaded on demand and unloaded when idle.
//!
//! - **Routing.** A connection names its database by TLS SNI (`<db>.redis.example.com`), by
//!   username (`user@<db>`), or by a self-identifying password/token (`<db>_<secret>`), so any
//!   Redis client or HTTP SDK works with a plain URL.
//! - **Scale to zero.** A database with no connections for `idle_timeout` is flushed to object
//!   storage and dropped from memory. The next AUTH (or HTTP request) restores it.
//! - **Catalog.** Which databases exist and their credentials come from a [`Catalog`]: the
//!   control plane in production, a static map for local use.

use crate::database::Database;
use crate::directory::{AuthError, AuthRequest, Directory, Session};
use crate::persist::{self, Shipper};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use redis_engine::Config;
use redis_engine::acl::User;
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
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self { idle_timeout: Some(Duration::from_secs(300)), domain: None }
    }
}

type Slot = Arc<OnceCell<Arc<Database>>>;

pub struct Node {
    pub catalog: Arc<dyn Catalog>,
    /// Durability; without it databases live only in memory and are never unloaded.
    pub shipper: Option<Arc<Shipper>>,
    pub cfg: NodeConfig,
    dbs: Mutex<HashMap<String, Slot>>,
    /// Serialises loads and unloads of the same database.
    transitions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
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
    if let Some(sni) = req.sni {
        let label = match domain {
            Some(d) => sni.strip_suffix(d).and_then(|s| s.strip_suffix('.')),
            None => sni.split('.').next(),
        };
        if let Some(db) = label.filter(|l| !l.is_empty() && !l.contains('.')) {
            return Some((db.to_string(), user, req.password));
        }
    }
    // `<db>_<secret>` tokens.
    let pos = req.password.iter().position(|&b| b == b'_')?;
    let db = std::str::from_utf8(&req.password[..pos]).ok()?;
    Some((db.to_string(), user, &req.password[pos + 1..]))
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Node {
    pub fn new(catalog: Arc<dyn Catalog>, shipper: Option<Arc<Shipper>>, cfg: NodeConfig) -> Arc<Self> {
        let node = Arc::new(Self { catalog, shipper, cfg, dbs: Mutex::new(HashMap::new()), transitions: Mutex::new(HashMap::new()) });
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
        node
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

    fn idle(&self, idle: Duration) -> Vec<String> {
        self.loaded()
            .into_iter()
            .filter(|d| d.connections.load(Ordering::SeqCst) == 0 && d.idle_ms() >= idle.as_millis() as u64)
            .map(|d| d.id.clone())
            .collect()
    }

    /// The database, restoring it from object storage if it isn't in memory.
    pub async fn open(&self, spec: &DbSpec) -> anyhow::Result<Arc<Database>> {
        loop {
            let slot = self.dbs.lock().entry(spec.id.clone()).or_default().clone();
            let db = slot
                .get_or_try_init(|| async {
                    let lock = self.transition_lock(&spec.id);
                    let _g = lock.lock().await;
                    let db = Database::new(spec.id.clone(), spec.config.clone(), spec.shards);
                    if let Some(shipper) = &self.shipper {
                        let started = std::time::Instant::now();
                        let m = persist::restore(&shipper.objects, &db, None).await?;
                        shipper.attach(&db, m).await?;
                        tracing::info!(db = spec.id, keys = db.total_keys(), ms = started.elapsed().as_millis() as u64, "database loaded");
                    }
                    db.start_cron();
                    anyhow::Ok(db)
                })
                .await?
                .clone();
            if !db.closed.load(Ordering::SeqCst) {
                return Ok(db);
            }
            // Unloading right now: wait for it to finish, then load again.
            let lock = self.transition_lock(&spec.id);
            drop(lock.lock().await);
        }
    }

    /// Flush a database to object storage and drop it from memory, unless it is in use.
    /// Returns whether it was unloaded.
    pub async fn unload(&self, id: &str) -> anyhow::Result<bool> {
        let Some(shipper) = &self.shipper else { return Ok(false) };
        let lock = self.transition_lock(id);
        let _g = lock.lock().await;
        let Some(db) = self.get_loaded(id) else { return Ok(false) };
        db.closed.store(true, Ordering::SeqCst);
        if db.connections.load(Ordering::SeqCst) > 0 {
            db.closed.store(false, Ordering::SeqCst);
            return Ok(false);
        }
        if let Err(e) = shipper.detach(&db).await {
            db.closed.store(false, Ordering::SeqCst);
            return Err(e);
        }
        self.dbs.lock().remove(id);
        tracing::info!(db = id, "database unloaded");
        Ok(true)
    }

    async fn session_for(&self, spec: &DbSpec, user: Arc<User>) -> Result<Session, AuthError> {
        loop {
            let db = self.open(spec).await.map_err(|e| AuthError::Unavailable(e.to_string()))?;
            if let Some(s) = Session::new(db, user.clone()) {
                return Ok(s);
            }
        }
    }
}

impl Directory for Node {
    fn authenticate<'a>(&'a self, req: AuthRequest<'a>) -> BoxFuture<'a, Result<Session, AuthError>> {
        Box::pin(async move {
            let (db, _user, secret) = route(&req, self.cfg.domain.as_deref()).ok_or(AuthError::WrongPass)?;
            let spec = self.catalog.lookup(&db).await.ok_or(AuthError::WrongPass)?;
            let full = req.password;
            let ok = spec.secrets.iter().find(|s| ct_eq(s.value.as_bytes(), secret) || ct_eq(s.value.as_bytes(), full));
            let Some(_secret) = ok else { return Err(AuthError::WrongPass) };
            self.session_for(&spec, User::default_user()).await
        })
    }

    fn anonymous(&self) -> BoxFuture<'_, Option<Session>> {
        Box::pin(async { None })
    }
}
