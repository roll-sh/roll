//! Which database a connection or request is for, and what keeps a database loaded while it is
//! in use.
//!
//! A [`Directory`] routes a connection by its TLS server name, its SASL PLAIN user (`role@db`)
//! or an HTTP token, and opens (wakes) databases. [`crate::node::Node`] is the multi-tenant
//! implementation; [`Single`] serves one already opened database (tests, embedding).
//!
//! A [`Lease`] is one user of a [`Loaded`] database: a CQL connection or an in-flight HTTP
//! request. Databases are only unloaded when they have none.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use nosql_engine as eng;
use nosql_proto::Dialect;
use serde::Serialize;
use tokio::sync::watch;

use crate::node::DbSpec;

/// Connection-level settings shared by every listener of a directory.
#[derive(Clone, Debug)]
pub struct ServerSettings {
    /// How connections whose database is not known yet (no SNI, routed by SASL user) present
    /// themselves: their OPTIONS/STARTUP answers and error texts.
    pub dialect: Dialect,
    /// `SCYLLA_SHARD_AWARE_PORT` in SUPPORTED.
    pub shard_aware_port: Option<u16>,
    /// `SCYLLA_SHARD_AWARE_PORT_SSL` in SUPPORTED.
    pub shard_aware_port_ssl: Option<u16>,
    /// Requests executing at once per connection; further frames wait (backpressure).
    pub max_in_flight: usize,
    /// Responses queued per connection before executing requests wait for the client to read.
    pub write_queue: usize,
    /// Close connections that haven't finished STARTUP/authentication after this long.
    pub handshake_timeout: Duration,
}

impl Default for ServerSettings {
    fn default() -> Self {
        ServerSettings {
            dialect: Dialect::Scylla,
            shard_aware_port: None,
            shard_aware_port_ssl: None,
            max_in_flight: 256,
            write_queue: 256,
            handshake_timeout: Duration::from_secs(60),
        }
    }
}

/// Counters per database, for metering. Monotonic over the node's life (they survive unloads).
#[derive(Debug, Default)]
pub struct Metrics {
    /// CQL connections bound to the database right now.
    pub connections: AtomicU64,
    pub connections_total: AtomicU64,
    /// Milliseconds of closed connections (open ones are added when they close).
    pub connection_ms: AtomicU64,
    /// CQL requests (QUERY, PREPARE, EXECUTE, BATCH, REGISTER, OPTIONS…) and HTTP requests.
    pub requests: AtomicU64,
    /// Requests answered with an ERROR.
    pub errors: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub http_requests: AtomicU64,
    /// Alternator (DynamoDB API) requests, and those answered with an error.
    pub alternator_requests: AtomicU64,
    pub alternator_errors: AtomicU64,
    /// Times the database was loaded.
    pub loads: AtomicU64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MetricsSnapshot {
    pub connections: u64,
    pub connections_total: u64,
    pub connection_ms: u64,
    pub requests: u64,
    pub errors: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub http_requests: u64,
    pub alternator_requests: u64,
    pub alternator_errors: u64,
    pub loads: u64,
}

impl Metrics {
    pub fn snapshot(&self) -> MetricsSnapshot {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        MetricsSnapshot {
            connections: g(&self.connections),
            connections_total: g(&self.connections_total),
            connection_ms: g(&self.connection_ms),
            requests: g(&self.requests),
            errors: g(&self.errors),
            bytes_in: g(&self.bytes_in),
            bytes_out: g(&self.bytes_out),
            http_requests: g(&self.http_requests),
            alternator_requests: g(&self.alternator_requests),
            alternator_errors: g(&self.alternator_errors),
            loads: g(&self.loads),
        }
    }

    pub(crate) fn add(a: &AtomicU64, n: u64) {
        a.fetch_add(n, Ordering::Relaxed);
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A database in memory, with its users and the node's signals to them.
pub struct Loaded {
    pub spec: Arc<DbSpec>,
    pub db: Arc<eng::Database>,
    pub metrics: Arc<Metrics>,
    users: AtomicUsize,
    closed: AtomicBool,
    kick: watch::Sender<bool>,
    last_active_ms: AtomicU64,
    /// The database's Alternator (created by its first DynamoDB API request).
    pub(crate) alternator: std::sync::OnceLock<Arc<nosql_alternator::Alternator>>,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded").field("id", &self.spec.id).field("users", &self.users()).finish()
    }
}

impl Loaded {
    pub fn new(spec: Arc<DbSpec>, db: Arc<eng::Database>, metrics: Arc<Metrics>) -> Arc<Self> {
        Arc::new(Loaded {
            spec,
            db,
            metrics,
            users: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            kick: watch::channel(false).0,
            last_active_ms: AtomicU64::new(now_ms()),
            alternator: Default::default(),
        })
    }

    pub fn id(&self) -> &str {
        &self.spec.id
    }

    /// Connections and in-flight HTTP requests.
    pub fn users(&self) -> usize {
        self.users.load(Ordering::SeqCst)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub(crate) fn set_closed(&self, closed: bool) {
        self.closed.store(closed, Ordering::SeqCst);
    }

    /// Milliseconds since the last user left (0 while in use).
    pub fn idle_ms(&self) -> u64 {
        if self.users() > 0 {
            return 0;
        }
        now_ms().saturating_sub(self.last_active_ms.load(Ordering::Relaxed))
    }

    pub(crate) fn touch(&self) {
        self.last_active_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Closes the database to new users and disconnects the current ones.
    pub fn kick(&self) {
        self.set_closed(true);
        let _ = self.kick.send(true);
    }

    /// Becomes `true` when the database disconnects its users.
    pub fn kicked(&self) -> watch::Receiver<bool> {
        self.kick.subscribe()
    }

    /// Lets users in again after a [`Loaded::kick`] (a database that stays loaded).
    pub(crate) fn reopen(&self) {
        let _ = self.kick.send(false);
        self.set_closed(false);
    }

    /// Sends a topology or status event to the connections registered for it.
    pub fn announce(&self, e: eng::Event) {
        self.db.publish(e);
    }

    /// The address drivers know the database's node by (events name it).
    pub fn address(&self) -> std::net::SocketAddr {
        let c = self.db.config();
        std::net::SocketAddr::new(c.address, c.native_port)
    }
}

/// One user of a loaded database.
pub struct Lease(Arc<Loaded>);

impl Lease {
    /// A lease on `l`, or `None` if it is closing (unloading or moving away); look it up again.
    pub fn acquire(l: &Arc<Loaded>) -> Option<Self> {
        // Pairs with the unloader, which sets `closed` and then counts users.
        l.users.fetch_add(1, Ordering::SeqCst);
        if l.is_closed() {
            l.users.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Lease(l.clone()))
    }

    pub fn loaded(&self) -> &Arc<Loaded> {
        &self.0
    }

    pub fn db(&self) -> &Arc<eng::Database> {
        &self.0.db
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.touch();
        self.0.users.fetch_sub(1, Ordering::SeqCst);
    }
}

impl std::ops::Deref for Lease {
    type Target = Loaded;

    fn deref(&self) -> &Loaded {
        &self.0
    }
}

/// How a connection presents the node before and after routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Presented {
    pub dialect: Dialect,
    pub host_id: [u8; 16],
    pub shards: u32,
}

impl Presented {
    pub fn of(spec: &DbSpec) -> Self {
        Presented {
            dialect: crate::convert::proto_dialect(spec.config.dialect),
            host_id: spec.config.host_id(),
            shards: spec.config.settings.shards.max(1),
        }
    }

    /// An unrouted connection: the directory's dialect, one shard, a host id of its own.
    pub fn unrouted(dialect: Dialect) -> Self {
        let cfg = eng::DbConfig::new("", crate::convert::engine_dialect(dialect));
        Presented { dialect, host_id: cfg.host_id(), shards: 1 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// The database is closing (moving away, being restored): try again shortly.
    Closing,
    /// It exists but couldn't be loaded.
    Unavailable(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Closing => f.write_str("the database is restarting, retry"),
            OpenError::Unavailable(e) => write!(f, "the database could not be loaded: {e}"),
        }
    }
}

pub trait Directory: Send + Sync + 'static {
    fn settings(&self) -> &ServerSettings;
    /// The database a TLS server name names (or the directory's default without one).
    fn route_sni<'a>(&'a self, sni: Option<&'a str>) -> BoxFuture<'a, Option<Arc<DbSpec>>>;
    /// The database a SASL PLAIN user names (`role@db`), with the role inside it.
    fn route_user<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Option<(Arc<DbSpec>, String)>>;
    /// The database an HTTP token (`<db>_<secret>`, or the directory's default with none)
    /// grants access to.
    fn route_token<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<Arc<DbSpec>>>;
    /// Loads the database if needed and takes a lease on it.
    fn open<'a>(&'a self, spec: &'a Arc<DbSpec>) -> BoxFuture<'a, Result<Lease, OpenError>>;
    /// The database of an Alternator request: named by its SigV4 access key (`role@db`) or its
    /// `Host` (`<db>.alternator.<domain>`), else the directory's default.
    fn route_alternator<'a>(&'a self, host: Option<&'a str>, access_key: Option<&'a str>) -> BoxFuture<'a, Option<Arc<DbSpec>>> {
        Box::pin(async move {
            if let Some(k) = access_key.filter(|k| k.contains('@'))
                && let Some((spec, _)) = self.route_user(k).await
            {
                return Some(spec);
            }
            self.route_sni(host).await
        })
    }
}

/// Constant-time comparison of secrets.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// One database (tests, embedding, single-tenant processes), never unloaded. Users connect
/// with any SASL user (passed to the engine as is) and HTTP requests with the token, if set.
pub struct Single {
    loaded: Arc<Loaded>,
    settings: ServerSettings,
}

impl Single {
    pub fn new(db: Arc<eng::Database>) -> Self {
        Self::with_spec(db, |_| {})
    }

    /// With changes to the spec derived from the database's config (credentials, tokens).
    pub fn with_spec(db: Arc<eng::Database>, f: impl FnOnce(&mut DbSpec)) -> Self {
        let mut spec = DbSpec::new(db.config().id.clone(), db.config().dialect);
        spec.config = db.config().clone();
        f(&mut spec);
        let metrics = Arc::new(Metrics::default());
        let settings = ServerSettings { dialect: crate::convert::proto_dialect(spec.config.dialect), ..Default::default() };
        Single { loaded: Loaded::new(Arc::new(spec), db, metrics), settings }
    }

    pub fn settings_mut(&mut self) -> &mut ServerSettings {
        &mut self.settings
    }

    pub fn loaded(&self) -> &Arc<Loaded> {
        &self.loaded
    }
}

impl Directory for Single {
    fn settings(&self) -> &ServerSettings {
        &self.settings
    }

    fn route_sni<'a>(&'a self, _: Option<&'a str>) -> BoxFuture<'a, Option<Arc<DbSpec>>> {
        Box::pin(async move { Some(self.loaded.spec.clone()) })
    }

    fn route_user<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Option<(Arc<DbSpec>, String)>> {
        Box::pin(async move { Some((self.loaded.spec.clone(), user.to_string())) })
    }

    fn route_token<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<Arc<DbSpec>>> {
        Box::pin(async move {
            let spec = &self.loaded.spec;
            let ok = match token {
                _ if spec.tokens.is_empty() => true,
                Some(t) => {
                    let secret = t.rsplit_once('_').map_or(t, |(_, s)| s);
                    spec.tokens.iter().any(|x| ct_eq(x.as_bytes(), secret.as_bytes()) || ct_eq(x.as_bytes(), t.as_bytes()))
                }
                None => false,
            };
            ok.then(|| spec.clone())
        })
    }

    fn open<'a>(&'a self, _: &'a Arc<DbSpec>) -> BoxFuture<'a, Result<Lease, OpenError>> {
        Box::pin(async move {
            // A kicked single database lets clients back in at once.
            if self.loaded.is_closed() && self.loaded.users() == 0 {
                self.loaded.reopen();
            }
            Lease::acquire(&self.loaded).ok_or(OpenError::Closing)
        })
    }
}
