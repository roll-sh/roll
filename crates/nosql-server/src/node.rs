//! A multi-tenant node: many databases in one process, loaded on first use and unloaded when
//! idle.
//!
//! - **Routing.** A connection names its database by TLS SNI (`<db>.<domain>`) or by its SASL
//!   PLAIN user (`role@db`; the role inside the database is the part before the last `@`).
//!   HTTP requests use a token `<db>_<secret>`. With [`NodeConfig::default_db`] set, anything
//!   else goes to that database (single mode).
//! - **Scale to zero.** A database without connections or HTTP requests for its idle timeout is
//!   unloaded: memtables are flushed, the manifest written, memory released. The next connection
//!   loads it again from object storage.
//! - **Live changes.** [`Node::set_idle_timeout`] and [`Node::set_read_only`] apply to a loaded
//!   database at once (the engine's settings: `Database::set_settings`).
//! - **Evict** disconnects every client first (they get a `STATUS_CHANGE DOWN` if registered)
//!   and then unloads: for deletes, in-place restores and moves.
//! - **Catalog.** Which databases exist and their settings come from a [`Catalog`]: the control
//!   plane in production, a [`StaticCatalog`] locally.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use nosql_engine as eng;
use nosql_storage::log::Durability;
use nosql_storage::store::Store;
use parking_lot::Mutex;
use tokio::sync::OnceCell;

use crate::directory::{Directory, Lease, Loaded, Metrics, MetricsSnapshot, OpenError, ServerSettings, ct_eq};

/// A credential the platform manages (the initial superuser, rotated by the control plane).
/// [`DbSpec::with_password`] also makes it the engine's `DbConfig::superuser`, so the engine's
/// `PasswordAuthenticator` checks it against the role it created; one the engine doesn't know
/// authenticates as `role` without consulting the auth tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    pub role: String,
    pub password: String,
}

/// What a node needs to know to serve a database.
#[derive(Clone, Debug)]
pub struct DbSpec {
    pub id: String,
    /// Dialect, presented node, and the `cassandra.yaml` / `scylla.yaml` settings
    /// (`settings.authenticator` selects `AllowAllAuthenticator` or `PasswordAuthenticator`).
    pub config: eng::DbConfig,
    /// Platform credentials (the first is the HTTP data API's role). One that is also
    /// `config.superuser` is checked by the engine.
    pub credentials: Vec<Credential>,
    /// HTTP data API secrets (`Authorization: Bearer <db>_<secret>`).
    pub tokens: Vec<String>,
    pub durability: Durability,
    /// Point-in-time restore window.
    pub history_retention: Duration,
    /// Unload after this long without clients (`None`: the node's default; zero: never).
    pub idle_timeout: Option<Duration>,
    /// Reject writes, schema, role and permission changes (read-only branches;
    /// `Database::set_read_only`).
    pub read_only: bool,
    /// Cap on the database's memtables (its plan; `None`: the node's per-database default).
    pub memtable_space: Option<u64>,
}

impl DbSpec {
    /// A database with the reference image's defaults for `dialect` (AllowAll auth).
    pub fn new(id: impl Into<String>, dialect: nosql_types::Dialect) -> Self {
        let id = id.into();
        DbSpec {
            config: eng::DbConfig::new(id.clone(), dialect),
            id,
            credentials: Vec::new(),
            tokens: Vec::new(),
            durability: Durability::Async,
            history_retention: Duration::from_secs(7 * 86_400),
            idle_timeout: None,
            read_only: false,
            memtable_space: None,
        }
    }

    /// The storage configuration of the database.
    pub fn storage_config(&self) -> nosql_storage::db::DbConfig {
        nosql_storage::db::DbConfig {
            durability: self.durability,
            history_retention_ms: self.history_retention.as_millis() as i64,
            memtable_space_bytes: self.memtable_space,
        }
    }

    /// With `PasswordAuthenticator` and a platform credential, which is also the engine's
    /// initial superuser (created at open when missing).
    pub fn with_password(mut self, role: impl Into<String>, password: impl Into<String>) -> Self {
        let (role, password) = (role.into(), password.into());
        self.config.settings.authenticator = "PasswordAuthenticator".into();
        self.config.superuser = Some(eng::config::Superuser { name: role.clone(), password: password.clone() });
        self.credentials.push(Credential { role, password });
        self
    }

    pub fn with_token(mut self, secret: impl Into<String>) -> Self {
        self.tokens.push(secret.into());
        self
    }

    /// Whether clients must authenticate (anything but `AllowAllAuthenticator`).
    pub fn requires_auth(&self) -> bool {
        let a = &self.config.settings.authenticator;
        !(a.is_empty() || a.ends_with("AllowAllAuthenticator"))
    }

    /// A platform credential matching `role` and `password`.
    pub fn platform_login(&self, role: &str, password: &[u8]) -> bool {
        // Check them all (constant time in the number matched).
        self.credentials.iter().fold(false, |ok, c| ok | (ct_eq(c.role.as_bytes(), role.as_bytes()) & ct_eq(c.password.as_bytes(), password)))
    }
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

    pub fn remove(&self, id: &str) {
        self.0.lock().remove(id);
    }
}

impl Catalog for StaticCatalog {
    fn lookup(&self, db: &str) -> BoxFuture<'_, Option<DbSpec>> {
        let spec = self.0.lock().get(db).cloned();
        Box::pin(async move { spec })
    }
}

#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// Unload databases idle (no connections, no requests) this long. `None`: never.
    pub idle_timeout: Option<Duration>,
    /// DNS suffix of database hostnames for SNI routing (`nosql.example.com`).
    pub domain: Option<String>,
    /// Single mode: connections that name no database go here.
    pub default_db: Option<String>,
    pub server: ServerSettings,
    /// DNS suffix of Alternator hostnames (`alternator.example.com`: `<db>.alternator.example.com`).
    pub alternator_domain: Option<String>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig { idle_timeout: Some(Duration::from_secs(300)), domain: None, default_db: None, server: ServerSettings::default(), alternator_domain: None }
    }
}

type Slot = Arc<OnceCell<Arc<Loaded>>>;

pub struct Node {
    pub catalog: Arc<dyn Catalog>,
    pub store: Arc<Store>,
    pub cfg: NodeConfig,
    dbs: Mutex<HashMap<String, Slot>>,
    metrics: Mutex<HashMap<String, Arc<Metrics>>>,
    /// Serialises loads and unloads of the same database.
    transitions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Idle timeouts set while loaded ([`Node::set_idle_timeout`]; reset from the spec at load).
    idle_timeouts: Mutex<HashMap<String, Option<Duration>>>,
}

/// The database label of a TLS server name: `<db>.<domain>` (any first label without a domain).
pub fn sni_label<'a>(sni: &'a str, domain: Option<&str>) -> Option<&'a str> {
    let label = match domain {
        Some(d) => sni.strip_suffix(d).and_then(|s| s.strip_suffix('.')),
        None => sni.split('.').next(),
    };
    label.filter(|l| !l.is_empty() && !l.contains('.'))
}

impl Node {
    pub fn new(catalog: Arc<dyn Catalog>, store: Arc<Store>, cfg: NodeConfig) -> Arc<Self> {
        let node = Arc::new(Node {
            catalog,
            store,
            cfg,
            dbs: Mutex::new(HashMap::new()),
            metrics: Mutex::new(HashMap::new()),
            transitions: Mutex::new(HashMap::new()),
            idle_timeouts: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&node);
        let tick = node.cfg.idle_timeout.map_or(Duration::from_secs(1), |i| (i / 4).clamp(Duration::from_millis(50), Duration::from_secs(15)));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                let Some(node) = weak.upgrade() else { return };
                for id in node.idle() {
                    if let Err(e) = node.unload(&id).await {
                        tracing::warn!(db = id, error = %e, "nosql unload failed");
                    }
                }
            }
        });
        node
    }

    fn transition_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.transitions.lock().entry(id.to_string()).or_default().clone()
    }

    pub fn loaded(&self) -> Vec<Arc<Loaded>> {
        self.dbs.lock().values().filter_map(|s| s.get().cloned()).collect()
    }

    pub fn get_loaded(&self, id: &str) -> Option<Arc<Loaded>> {
        self.dbs.lock().get(id).and_then(|s| s.get().cloned())
    }

    /// The counters of a database this node has served.
    pub fn metrics(&self, id: &str) -> Option<MetricsSnapshot> {
        self.metrics.lock().get(id).map(|m| m.snapshot())
    }

    pub fn all_metrics(&self) -> Vec<(String, MetricsSnapshot)> {
        let mut v: Vec<_> = self.metrics.lock().iter().map(|(k, m)| (k.clone(), m.snapshot())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    fn metrics_of(&self, id: &str) -> Arc<Metrics> {
        self.metrics.lock().entry(id.to_string()).or_default().clone()
    }

    /// Changes a database's idle timeout (`None`: the node's default; zero: never unload). Applies
    /// to the loaded database at once; the next load takes the spec's again.
    pub fn set_idle_timeout(&self, id: &str, idle: Option<Duration>) {
        self.idle_timeouts.lock().insert(id.to_string(), idle);
    }

    /// The idle timeout a loaded database is unloaded after (`None`: never).
    pub fn idle_timeout(&self, id: &str) -> Option<Duration> {
        let own = match self.idle_timeouts.lock().get(id) {
            Some(t) => *t,
            None => self.get_loaded(id).and_then(|l| l.spec.idle_timeout),
        };
        own.or(self.cfg.idle_timeout).filter(|t| !t.is_zero())
    }

    /// Makes a loaded database read-only, or writable again, at once (`Database::set_read_only`;
    /// the next load takes the spec's flag). `false` when it isn't loaded.
    pub fn set_read_only(&self, id: &str, on: bool) -> bool {
        match self.get_loaded(id) {
            Some(l) => {
                l.db.set_read_only(on);
                true
            }
            None => false,
        }
    }

    fn idle(&self) -> Vec<String> {
        self.loaded()
            .into_iter()
            .filter(|l| {
                let Some(idle) = self.idle_timeout(&l.spec.id) else { return false };
                !idle.is_zero() && l.users() == 0 && l.idle_ms() >= idle.as_millis() as u64
            })
            .map(|l| l.spec.id.clone())
            .collect()
    }

    /// The database, loading it from object storage if it isn't in memory.
    pub async fn open(&self, spec: &Arc<DbSpec>) -> anyhow::Result<Arc<Loaded>> {
        loop {
            let slot = self.dbs.lock().entry(spec.id.clone()).or_default().clone();
            let loaded = slot
                .get_or_try_init(|| async {
                    let lock = self.transition_lock(&spec.id);
                    let _g = lock.lock().await;
                    let started = std::time::Instant::now();
                    let storage = self.store.open(&spec.id, spec.storage_config()).await?;
                    let db = match eng::Database::open(spec.config.clone(), storage).await {
                        Ok(db) => db,
                        Err(e) => {
                            let _ = self.store.unload(&spec.id).await;
                            return Err(e);
                        }
                    };
                    db.set_read_only(spec.read_only);
                    self.set_idle_timeout(&spec.id, spec.idle_timeout);
                    let metrics = self.metrics_of(&spec.id);
                    Metrics::add(&metrics.loads, 1);
                    tracing::info!(db = spec.id, ms = started.elapsed().as_millis() as u64, "nosql database ready");
                    anyhow::Ok(Loaded::new(spec.clone(), db, metrics))
                })
                .await?
                .clone();
            if !loaded.is_closed() {
                return Ok(loaded);
            }
            // Unloading right now: wait for it to finish, then load again.
            // (A failed unload reopens it; a finished one removes the slot.)
            let lock = self.transition_lock(&spec.id);
            drop(lock.lock().await);
        }
    }

    /// Flush a database to object storage and drop it from memory, unless it is in use.
    /// Returns whether it was unloaded.
    pub async fn unload(&self, id: &str) -> anyhow::Result<bool> {
        self.unload_inner(id, false).await
    }

    /// Unload even if clients are connected: they are disconnected first (registered ones get
    /// `STATUS_CHANGE DOWN`) and reconnect to the reloaded database. Used for deletes, in-place
    /// restores and moves.
    pub async fn evict(&self, id: &str) -> anyhow::Result<bool> {
        self.unload_inner(id, true).await
    }

    async fn unload_inner(&self, id: &str, force: bool) -> anyhow::Result<bool> {
        let lock = self.transition_lock(id);
        let _g = lock.lock().await;
        let Some(l) = self.get_loaded(id) else { return Ok(false) };
        if force {
            l.announce(eng::Event::StatusChange { change: "DOWN", address: l.address() });
            // Let the event reach the connections before they close.
            tokio::task::yield_now().await;
            l.kick();
            for _ in 0..500 {
                if l.users() == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } else {
            l.set_closed(true);
        }
        if l.users() > 0 {
            l.reopen();
            if force {
                anyhow::bail!("database {id} still has clients after disconnecting them");
            }
            return Ok(false);
        }
        if let Err(e) = self.store.unload(id).await {
            l.reopen();
            return Err(e);
        }
        self.dbs.lock().remove(id);
        self.idle_timeouts.lock().remove(id);
        // The engine's database (schema, caches, the storage tables' indexes) goes with the
        // last reference; then freed memory goes back to the OS.
        drop(l);
        nosql_storage::memory::release_to_os();
        tracing::info!(db = id, "nosql database unloaded");
        Ok(true)
    }

    /// Sends a topology or status event to a loaded database's registered connections.
    pub fn announce(&self, id: &str, e: eng::Event) -> bool {
        match self.get_loaded(id) {
            Some(l) => {
                l.announce(e);
                true
            }
            None => false,
        }
    }

    async fn lookup(&self, id: &str) -> Option<Arc<DbSpec>> {
        self.catalog.lookup(id).await.map(Arc::new)
    }

    async fn default_spec(&self) -> Option<Arc<DbSpec>> {
        match &self.cfg.default_db {
            Some(d) => self.lookup(d).await,
            None => None,
        }
    }
}

impl Directory for Node {
    fn settings(&self) -> &ServerSettings {
        &self.cfg.server
    }

    fn route_sni<'a>(&'a self, sni: Option<&'a str>) -> BoxFuture<'a, Option<Arc<DbSpec>>> {
        Box::pin(async move {
            if let Some(label) = sni.and_then(|s| sni_label(s, self.cfg.domain.as_deref()))
                && let Some(spec) = self.lookup(label).await
            {
                return Some(spec);
            }
            self.default_spec().await
        })
    }

    fn route_user<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Option<(Arc<DbSpec>, String)>> {
        Box::pin(async move {
            if let Some((role, db)) = user.rsplit_once('@')
                && let Some(spec) = self.lookup(db).await
            {
                return Some((spec, role.to_string()));
            }
            self.default_spec().await.map(|s| (s, user.to_string()))
        })
    }

    fn route_token<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<Arc<DbSpec>>> {
        Box::pin(async move {
            let Some(token) = token else {
                // Single mode without tokens: the default database is open.
                return self.default_spec().await.filter(|s| s.tokens.is_empty());
            };
            if let Some((db, secret)) = token.rsplit_once('_')
                && let Some(spec) = self.lookup(db).await
                && spec.tokens.iter().any(|t| ct_eq(t.as_bytes(), secret.as_bytes()))
            {
                return Some(spec);
            }
            self.default_spec().await.filter(|s| s.tokens.iter().any(|t| ct_eq(t.as_bytes(), token.as_bytes())))
        })
    }

    fn route_alternator<'a>(&'a self, host: Option<&'a str>, access_key: Option<&'a str>) -> BoxFuture<'a, Option<Arc<DbSpec>>> {
        Box::pin(async move {
            if let Some((_, db)) = access_key.and_then(|k| k.rsplit_once('@'))
                && let Some(spec) = self.lookup(db).await
            {
                return Some(spec);
            }
            // `Host` without its port.
            let host = host.map(|h| match h.rsplit_once(':') {
                Some((name, port)) if !name.ends_with(']') && port.bytes().all(|b| b.is_ascii_digit()) && !name.contains(':') => name,
                _ => h,
            });
            if let (Some(h), Some(domain)) = (host, self.cfg.alternator_domain.as_deref())
                && let Some(label) = sni_label(h, Some(domain))
                && let Some(spec) = self.lookup(label).await
            {
                return Some(spec);
            }
            self.default_spec().await
        })
    }

    fn open<'a>(&'a self, spec: &'a Arc<DbSpec>) -> BoxFuture<'a, Result<Lease, OpenError>> {
        Box::pin(async move {
            for _ in 0..100 {
                let l = Node::open(self, spec).await.map_err(|e| OpenError::Unavailable(e.to_string()))?;
                if let Some(lease) = Lease::acquire(&l) {
                    return Ok(lease);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(OpenError::Closing)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sni_labels() {
        assert_eq!(sni_label("n1.nosql.example.com", Some("nosql.example.com")), Some("n1"));
        assert_eq!(sni_label("nosql.example.com", Some("nosql.example.com")), None);
        assert_eq!(sni_label("a.b.nosql.example.com", Some("nosql.example.com")), None);
        assert_eq!(sni_label("n1.other.com", Some("nosql.example.com")), None);
        assert_eq!(sni_label("n1.anything", None), Some("n1"));
        assert_eq!(sni_label("", None), None);
    }

    #[test]
    fn specs() {
        let s = DbSpec::new("n1", nosql_types::Dialect::Scylla);
        assert!(!s.requires_auth());
        let s = s.with_password("cassandra", "pw");
        assert!(s.requires_auth());
        assert!(s.platform_login("cassandra", b"pw"));
        assert!(!s.platform_login("cassandra", b"px"));
        assert!(!s.platform_login("other", b"pw"));
        let mut q = DbSpec::new("n1", nosql_types::Dialect::Cassandra);
        q.config.settings.authenticator = "org.apache.cassandra.auth.AllowAllAuthenticator".into();
        assert!(!q.requires_auth());
        q.config.settings.authenticator = "org.apache.cassandra.auth.PasswordAuthenticator".into();
        assert!(q.requires_auth());
    }
}
