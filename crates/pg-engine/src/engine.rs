//! The database registry: many databases in one process. A database is opened when a session
//! connects and closed again once nobody has used it for a while, so an idle database costs
//! no memory, only its files.

use crate::catalog::Catalog;
use crate::catalog::shared::{OidGen, SharedCatalog};
use crate::error::{Error, Result, code, internal};
use crate::session::Session;
use crate::session::state::{Activity, Cluster, DATCONNLIMIT_UNLIMITED, DbOptions, SessionState};
use crate::storage::cache::BlockCache;
use crate::storage::{Store, StoreOptions};
use crate::txn::{Database, Isolation, Txn};
use crate::types::collation::{DbDefault, DbLocale};
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tracing::{info, warn};

/// About how many bytes of row data the storage upgrade moves per transaction.
static UPGRADE_BATCH_BYTES: AtomicUsize = AtomicUsize::new(32 << 20);
/// Batch transactions the storage upgrade has committed (tests count them).
static UPGRADE_BATCHES: AtomicUsize = AtomicUsize::new(0);
/// Fail the upgrade after this many batches (tests of an interrupted upgrade; 0: never).
static UPGRADE_STOP_AFTER: AtomicUsize = AtomicUsize::new(0);
/// Index ids in a table's key space (no index's) under which the storage upgrade keeps its
/// staged rows, its map of old keys to new ones, and the mark that every row is staged.
const UPGRADE_STAGE_ROWS: u32 = u32::MAX - 1;
const UPGRADE_STAGE_MAP: u32 = u32::MAX - 2;
const UPGRADE_STAGED: u32 = u32::MAX - 3;

#[derive(Clone, Debug)]
pub struct EngineOptions {
    pub data_dir: PathBuf,
    /// Block cache shared by every open database.
    pub cache_bytes: usize,
    pub memtable_bytes: usize,
    /// fsync the WAL on commit.
    pub sync: bool,
    /// Close a database this long after its last session ends.
    pub idle_close: Duration,
    /// The superuser every new database is bootstrapped with.
    pub superuser: String,
    /// Its password; None leaves it without one (trust authentication only).
    pub password: Option<String>,
    /// Server-side file access (`pg_read_file`, server-side `lo_import` / `lo_export`,
    /// `COPY ... FROM / TO '<file>'`) for the roles Postgres allows, confined to this
    /// directory (see `server_files`). None (the default): refused for every role.
    pub server_files: Option<PathBuf>,
    /// Libraries loaded at server start (`shared_preload_libraries`): `pg_stat_statements`
    /// collects statement statistics from then on.
    pub shared_preload_libraries: Vec<String>,
    /// initdb's locale and encoding options, for a new data directory (an existing one keeps
    /// what it was made with). The default is initdb's `--locale=C --encoding=UTF8`.
    pub locale: crate::types::collation::dblocale::InitdbLocale,
    /// Where outbound connections (dblink, postgres_fdw) may go: `local` (the engine
    /// itself, reached without a host), `host[:port]`, `*[:port]`, `addr/bits` (see
    /// `remote::outbound`). Empty (the default): refused for every role.
    pub outbound: Vec<String>,
    /// max_prepared_transactions: how many transactions may be prepared at once (PREPARE
    /// TRANSACTION). 0 (the default, as PostgreSQL's): two-phase commit is disabled.
    pub max_prepared_transactions: usize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            data_dir: PathBuf::from(".roll/engine"),
            cache_bytes: 64 << 20,
            memtable_bytes: 8 << 20,
            sync: true,
            idle_close: Duration::from_secs(60),
            superuser: "postgres".into(),
            password: None,
            server_files: None,
            shared_preload_libraries: Vec::new(),
            locale: Default::default(),
            outbound: Vec::new(),
            max_prepared_transactions: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DbEntry {
    name: String,
    oid: u32,
    owner: String,
    /// template0 does not accept connections.
    #[serde(default = "yes")]
    allow_connections: bool,
    /// Its encoding and locale (None: a registry from before they were kept, libc C).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    locale: Option<DbLocale>,
    /// datistemplate (None: a registry from before it was kept, where template0 and
    /// template1 are the templates; `Engine::open` fills it in).
    #[serde(default)]
    is_template: Option<bool>,
    /// datconnlimit: DATCONNLIMIT_UNLIMITED, a limit, or DATCONNLIMIT_INVALID_DB.
    #[serde(default = "unlimited")]
    conn_limit: i32,
}

impl DbEntry {
    fn locale(&self) -> DbLocale {
        self.locale.clone().unwrap_or_default()
    }
}

fn yes() -> bool {
    true
}

fn unlimited() -> i32 {
    DATCONNLIMIT_UNLIMITED
}

impl DbEntry {
    fn options(&self) -> DbOptions {
        DbOptions {
            is_template: self.is_template.unwrap_or(false),
            allow_connections: self.allow_connections,
            conn_limit: self.conn_limit,
        }
    }

    fn set_options(&mut self, o: DbOptions) {
        self.is_template = Some(o.is_template);
        self.allow_connections = o.allow_connections;
        self.conn_limit = o.conn_limit;
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    /// Oids below this may be in use (the cluster's oid counter starts here).
    next_oid: u32,
    databases: Vec<DbEntry>,
    /// The cluster-wide catalogs and their version (None: a registry from before they were
    /// kept here, when every database had its own roles and oid counter).
    #[serde(default)]
    shared: Option<Arc<SharedCatalog>>,
    #[serde(default)]
    shared_version: u64,
    /// Transaction ids below this may have been handed out.
    #[serde(default)]
    next_xid: u64,
    /// The oldest transaction id whose status is still known (Postgres's oldestClogXid):
    /// a cluster starts where initdb leaves Postgres's, its earlier ids frozen. 0: a registry
    /// from before this was kept (every id known).
    #[serde(default)]
    oldest_xid: u64,
    /// What initdb wrote to postgresql.conf from its locale options (lc_messages,
    /// lc_monetary, lc_numeric, lc_time; empty: C, a registry from before they were kept).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    config: Vec<(String, String)>,
    /// Databases that may hold prepared transactions' state records (added before a
    /// transaction there prepares): opened at startup to recover them.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    prepared_dbs: BTreeSet<u32>,
}

/// A connected client, for pg_stat_activity and cancel / terminate.
struct Backend {
    secret: i32,
    database: u32,
    cancel: Option<Arc<AtomicBool>>,
    socket: Option<TcpStream>,
    terminated: Arc<AtomicBool>,
    activity: Activity,
}

/// The open databases, and those whose store is closing. A database's files aren't opened
/// again until its old store is done with them: opening a store clears out what a closing
/// one may still be writing (the files it flushes to, its MANIFEST.tmp, the WAL it retires).
#[derive(Default)]
struct OpenDbs {
    dbs: HashMap<u32, Arc<Database>>,
    closing: HashSet<u32>,
}

/// A database being closed (or kept closed while its files are copied, renamed or removed):
/// it is opened again only once this drops.
struct Closing<'a> {
    engine: &'a Engine,
    oid: u32,
}

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        self.engine.open.lock().closing.remove(&self.oid);
        self.engine.reopenable.notify_all();
    }
}

pub struct Engine {
    opts: EngineOptions,
    cache: Arc<BlockCache>,
    registry: Mutex<Registry>,
    open: Mutex<OpenDbs>,
    /// Signalled when a database stops closing (`OpenDbs::closing`).
    reopenable: Condvar,
    /// Held while committing catalog changes, so the cluster-wide catalogs change one
    /// transaction at a time and reach every open database in order.
    shared_lock: Mutex<()>,
    /// Recent versions of the cluster-wide catalogs, for merging a transaction's changes to
    /// an older one into the current one.
    shared_history: Mutex<std::collections::VecDeque<(u64, Arc<SharedCatalog>)>>,
    oids: Arc<OidGen>,
    xids: Arc<crate::txn::xids::Xids>,
    /// The cluster's prepared transactions.
    twophase: Arc<crate::txn::twophase::TwoPhase>,
    backends: Mutex<HashMap<i32, Backend>>,
    next_pid: AtomicI32,
    /// Held while a database's indexes are brought to the current key format.
    index_upgrade: Mutex<()>,
    started: i64,
    stopped: AtomicBool,
    me: Weak<Engine>,
    server_files: Option<crate::server_files::ServerFiles>,
    statements: Option<crate::ext::pg_stat_statements::Store>,
    /// The outbound allow-list (None: refused).
    outbound: Option<crate::remote::outbound::Outbound>,
    /// The address the server listens on (`server::serve`), which connections without a host
    /// reach.
    listen: std::sync::OnceLock<std::net::SocketAddr>,
}

const REGISTRY_FILE: &str = "databases.json";

impl Engine {
    /// The server's listening address (the first one noted), for connections the engine
    /// makes to itself (dblink, postgres_fdw without a host).
    pub fn note_listen_addr(&self, addr: std::net::SocketAddr) {
        let _ = self.listen.set(addr);
    }

    pub fn open(opts: EngineOptions) -> Result<Arc<Engine>> {
        let server_files = match &opts.server_files {
            Some(d) => Some(crate::server_files::ServerFiles::new(d).map_err(|e| {
                internal(format!("server files directory \"{}\": {e}", d.display()))
            })?),
            None => None,
        };
        let outbound = if opts.outbound.is_empty() {
            None
        } else {
            Some(crate::remote::outbound::Outbound::parse(&opts.outbound).map_err(internal)?)
        };
        std::fs::create_dir_all(opts.data_dir.join("base"))?;
        let path = opts.data_dir.join(REGISTRY_FILE);
        let mut registry: Registry = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| internal(format!("bad {REGISTRY_FILE}: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // initdb: the template databases' locale, and postgresql.conf's lc_*.
                let init = crate::types::collation::dblocale::initdb(&opts.locale, &|v| {
                    std::env::var(v).ok()
                })
                .map_err(|e| internal(format!("initdb: {}", e.message)))?;
                let owner = opts.superuser.clone();
                let entry = |name: &str, oid, allow| DbEntry {
                    name: name.into(),
                    oid,
                    owner: owner.clone(),
                    allow_connections: allow,
                    // (template0 records no collation version.)
                    locale: Some(DbLocale {
                        collversion: init.db.collversion.clone().filter(|_| oid != 4),
                        ..init.db.clone()
                    }),
                    is_template: Some(name != "postgres"),
                    conn_limit: DATCONNLIMIT_UNLIMITED,
                };
                let r = Registry {
                    next_oid: 16384,
                    next_xid: crate::txn::xids::INITDB_XID,
                    oldest_xid: crate::txn::xids::INITDB_XID,
                    databases: vec![
                        entry("template1", 1, true),
                        entry("template0", 4, false),
                        entry("postgres", 5, true),
                    ],
                    config: init.config.clone(),
                    ..Registry::default()
                };
                write_registry(&opts.data_dir, &r)?;
                r
            }
            Err(e) => return Err(e.into()),
        };
        // A registry from before datistemplate was kept: initdb's templates are.
        for d in registry.databases.iter_mut().filter(|d| d.is_template.is_none()) {
            d.is_template = Some(d.name == "template0" || d.name == "template1");
        }
        let cache = BlockCache::new(opts.cache_bytes);
        let statements = opts
            .shared_preload_libraries
            .iter()
            .any(|l| l == "pg_stat_statements")
            .then(|| crate::ext::pg_stat_statements::Store::new(crate::types::datetime::Clock::system().now, 5000));
        let engine = Arc::new_cyclic(|me: &Weak<Engine>| {
            let weak = me.clone();
            let oids = Arc::new(OidGen::new(registry.next_oid, move |n| {
                if let Some(e) = weak.upgrade() {
                    let mut r = e.registry.lock();
                    r.next_oid = r.next_oid.max(n);
                    if let Err(err) = e.save(&r) {
                        warn!(error = %err, "saving the oid counter failed");
                    }
                }
            }));
            let weak = me.clone();
            let oldest_xid = registry.oldest_xid;
            let xids = Arc::new(crate::txn::xids::Xids::new(registry.next_xid, move |n| {
                if let Some(e) = weak.upgrade() {
                    let mut r = e.registry.lock();
                    r.next_xid = r.next_xid.max(n);
                    if let Err(err) = e.save(&r) {
                        warn!(error = %err, "saving the transaction id counter failed");
                    }
                }
            }).with_oldest(oldest_xid));
            let weak = me.clone();
            let twophase = Arc::new(crate::txn::twophase::TwoPhase::new(
                opts.max_prepared_transactions,
                move |db| {
                    if let Some(e) = weak.upgrade() {
                        let mut r = e.registry.lock();
                        if r.prepared_dbs.insert(db)
                            && let Err(err) = e.save(&r)
                        {
                            warn!(error = %err, "saving the registry failed");
                        }
                    }
                },
            ));
            Engine {
                opts,
                cache,
                registry: Mutex::new(registry),
                open: Mutex::new(OpenDbs::default()),
                reopenable: Condvar::new(),
                shared_lock: Mutex::new(()),
                shared_history: Mutex::new(Default::default()),
                oids,
                xids,
                twophase,
                index_upgrade: Mutex::new(()),
                backends: Mutex::new(HashMap::new()),
                next_pid: AtomicI32::new(1000 + (std::process::id() as i32 % 1000) * 10),
                started: crate::types::datetime::Clock::system().now,
                stopped: AtomicBool::new(false),
                me: me.clone(),
                server_files,
                statements,
                outbound,
                listen: std::sync::OnceLock::new(),
            }
        });
        engine.init_shared()?;
        // Databases with prepared transactions open now, so those are there (in
        // pg_prepared_xacts, holding their locks) from the start.
        let prepared: Vec<DbEntry> = {
            let r = engine.registry.lock();
            r.databases.iter().filter(|d| r.prepared_dbs.contains(&d.oid)).cloned().collect()
        };
        for e in &prepared {
            engine.open_db(e)?;
        }
        let weak = Arc::downgrade(&engine);
        let tick =
            (engine.opts.idle_close / 4).clamp(Duration::from_millis(100), Duration::from_secs(10));
        std::thread::Builder::new()
            .name("roll-idle".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(tick);
                    let Some(e) = weak.upgrade() else { return };
                    if e.stopped.load(Ordering::Acquire) {
                        return;
                    }
                    e.close_idle(e.opts.idle_close);
                }
            })
            .map_err(|e| internal(format!("spawn idle thread: {e}")))?;
        Ok(engine)
    }

    pub fn options(&self) -> &EngineOptions {
        &self.opts
    }

    fn arc(&self) -> Arc<Engine> {
        self.me.upgrade().expect("engine dropped")
    }

    fn entry(&self, name: &str) -> Option<DbEntry> {
        self.registry
            .lock()
            .databases
            .iter()
            .find(|d| d.name == name)
            .cloned()
    }

    fn db_dir(&self, oid: u32) -> PathBuf {
        self.opts.data_dir.join("base").join(oid.to_string())
    }

    fn store_options(&self) -> StoreOptions {
        StoreOptions {
            memtable_bytes: self.opts.memtable_bytes,
            sync: self.opts.sync,
            cache: self.cache.clone(),
            ..StoreOptions::default()
        }
    }

    /// The open database for `entry`, opening (and on first use bootstrapping) it; once its
    /// store has finished closing, if it is closing.
    fn open_db(&self, entry: &DbEntry) -> Result<Arc<Database>> {
        let mut open = self.open.lock();
        loop {
            if let Some(db) = open.dbs.get(&entry.oid) {
                return Ok(db.clone());
            }
            if !open.closing.contains(&entry.oid) {
                break;
            }
            self.reopenable.wait(&mut open);
        }
        // (It may have been dropped or renamed meanwhile.)
        if !self
            .registry
            .lock()
            .databases
            .iter()
            .any(|d| d.oid == entry.oid && d.name == entry.name)
        {
            return Err(Error::new(
                code::INVALID_CATALOG_NAME,
                format!("database \"{}\" does not exist", entry.name),
            )
            .fatal());
        }
        let store = Store::open(&self.db_dir(entry.oid), self.store_options())?;
        let db = Database::open(&entry.name, entry.oid, store, &entry.owner)?;
        // CheckMyDatabase: its default collation must open (ICU rules may not).
        let locale = DbDefault::open(entry.locale()).map_err(Error::fatal)?;
        db.set_locale(Arc::new(locale));
        db.set_oids(self.oids.clone());
        db.set_xids(self.xids.clone());
        db.set_twophase(self.twophase.clone());
        // Its prepared transactions, with their locks, before any session can see it.
        if db.recover_prepared()? == 0 {
            let mut r = self.registry.lock();
            if r.prepared_dbs.remove(&entry.oid) {
                let _ = self.save(&r);
            }
        }
        info!(database = %entry.name, "opened database");
        open.dbs.insert(entry.oid, db.clone());
        Ok(db)
    }

    /// Whether `db` is (still) the open instance of its database.
    fn is_open(&self, db: &Arc<Database>) -> bool {
        self.open
            .lock()
            .dbs
            .get(&db.oid)
            .is_some_and(|d| Arc::ptr_eq(d, db))
    }

    /// First start: take the cluster-wide catalogs from `postgres` and move the oid counter
    /// past every database's oids. Then give the bootstrap superuser the configured password
    /// if it has none yet.
    fn init_shared(&self) -> Result<()> {
        if self.registry.lock().shared.is_none() {
            let entries = self.registry.lock().databases.clone();
            let mut floor = 0;
            let mut base = None;
            for e in &entries {
                let db = self.open_db(e)?;
                let c = db.catalog();
                floor = floor.max(c.next_oid);
                if e.name == "postgres" {
                    base = Some(SharedCatalog::extract(&c, &self.database_oids()));
                }
            }
            self.oids.take(floor, 0);
            let mut r = self.registry.lock();
            r.next_oid = r.next_oid.max(floor);
            r.shared = Some(Arc::new(base.unwrap_or_else(|| {
                SharedCatalog::extract(&Catalog::bootstrap(&self.opts.superuser), &BTreeSet::new())
            })));
            r.shared_version = 1;
            self.save(&r)?;
        }
        {
            let r = self.registry.lock();
            if let Some(s) = &r.shared {
                self.remember(r.shared_version, s.clone());
            }
        }
        let Some(pw) = &self.opts.password else {
            return Ok(());
        };
        let superuser = self.opts.superuser.clone();
        let pw = pw.clone();
        self.publish(|s| {
            if let Some(r) = s
                .roles
                .values_mut()
                .find(|r| r.name == superuser && r.password.is_none())
            {
                r.password = Some(crate::auth::hash_password(&pw));
                return true;
            }
            false
        })
    }

    fn database_oids(&self) -> BTreeSet<u32> {
        self.registry
            .lock()
            .databases
            .iter()
            .map(|d| d.oid)
            .collect()
    }

    /// Change the cluster-wide catalogs outside a transaction (`f` returns whether it changed
    /// anything) and bring every open database up to date.
    fn publish(&self, f: impl FnOnce(&mut SharedCatalog) -> bool) -> Result<()> {
        let _g = self.shared_lock.lock();
        {
            let mut r = self.registry.lock();
            let mut s = r.shared.as_deref().cloned().unwrap_or_default();
            if !f(&mut s) {
                return Ok(());
            }
            let s = Arc::new(s);
            r.shared = Some(s.clone());
            r.shared_version += 1;
            self.remember(r.shared_version, s);
            self.save(&r)?;
        }
        self.sync_open(None);
        Ok(())
    }

    fn remember(&self, version: u64, s: Arc<SharedCatalog>) {
        let mut h = self.shared_history.lock();
        if h.len() >= 64 {
            h.pop_front();
        }
        h.push_back((version, s));
    }

    /// Bring every open database but `except` up to date with the cluster-wide catalogs
    /// (`shared_lock` held).
    fn sync_open(&self, except: Option<u32>) {
        let dbs: Vec<Arc<Database>> = self
            .open
            .lock()
            .dbs
            .iter()
            .filter(|(oid, _)| Some(**oid) != except)
            .map(|(_, d)| d.clone())
            .collect();
        for db in dbs {
            if let Err(e) = self.sync_locked(&db) {
                warn!(database = %db.name, error = %e, "updating shared catalogs failed");
            }
        }
    }

    /// Bring `db`'s copy of the cluster-wide catalogs up to date.
    fn ensure_synced(&self, db: &Arc<Database>) -> Result<()> {
        let _g = self.shared_lock.lock();
        self.sync_locked(db)
    }

    fn sync_locked(&self, db: &Arc<Database>) -> Result<()> {
        let (version, shared, dbs) = {
            let r = self.registry.lock();
            (
                r.shared_version,
                r.shared.clone(),
                r.databases.iter().map(|d| d.oid).collect::<BTreeSet<u32>>(),
            )
        };
        let Some(shared) = shared else { return Ok(()) };
        for _ in 0..20 {
            let cur = db.catalog();
            if cur.shared_stamp.0 == version {
                return Ok(());
            }
            let mut c: Catalog = (*cur).clone();
            shared.apply(&mut c, &dbs, version);
            let mut txn = db.begin(
                Isolation::ReadCommitted,
                false,
                Arc::new(AtomicBool::new(false)),
            );
            txn.publish = true;
            txn.set_catalog(c)?;
            match txn.commit() {
                Ok(_) => return Ok(()),
                Err(e) if e.code == code::SERIALIZATION_FAILURE => continue,
                Err(e) => return Err(e),
            }
        }
        Err(internal(format!(
            "could not update the shared catalogs of database \"{}\"",
            db.name
        )))
    }

    /// Roles for authenticating to `dbname`: its own catalog, or `postgres`'s when it doesn't
    /// exist or takes no connections (so a wrong database name, or template0, is reported
    /// only after authentication, and template0 isn't opened for it).
    pub fn auth_catalog(&self, dbname: &str) -> Result<Arc<Catalog>> {
        let entry = self
            .entry(dbname)
            .filter(|e| e.allow_connections && !e.options().is_invalid())
            .or_else(|| self.entry("postgres"))
            .ok_or_else(|| internal("no postgres database"))?;
        loop {
            let db = self.open_db(&entry)?;
            match self.ensure_synced(&db) {
                Ok(()) => return Ok(db.catalog()),
                // Closed meanwhile (DROP / RENAME / template copy): open it again.
                Err(_) if !self.is_open(&db) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Allocate a backend id and cancel key for a new connection.
    pub fn register_backend(
        &self,
        socket: Option<TcpStream>,
        client_addr: Option<String>,
    ) -> (i32, i32, Arc<AtomicBool>) {
        let pid = self.next_pid.fetch_add(1, Ordering::Relaxed);
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let secret = i32::from_le_bytes(b);
        let terminated = Arc::new(AtomicBool::new(false));
        let activity = Activity {
            pid,
            client_addr,
            backend_start: crate::types::datetime::Clock::system().now,
            state: "starting".into(),
            ..Activity::default()
        };
        self.backends.lock().insert(
            pid,
            Backend {
                secret,
                database: 0,
                cancel: None,
                socket,
                terminated: terminated.clone(),
                activity,
            },
        );
        (pid, secret, terminated)
    }

    pub fn unregister_backend(&self, pid: i32) {
        self.backends.lock().remove(&pid);
    }

    /// Start a session on `dbname` as `user`.
    pub fn connect(
        &self,
        pid: i32,
        dbname: &str,
        user: &str,
        application_name: &str,
    ) -> Result<Session> {
        let entry = self.entry(dbname).ok_or_else(|| {
            Error::new(
                code::INVALID_CATALOG_NAME,
                format!("database \"{dbname}\" does not exist"),
            )
            .fatal()
        })?;
        // InitPostgres, then CheckMyDatabase.
        if entry.options().is_invalid() {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("cannot connect to invalid database \"{dbname}\""),
            )
            .hint("Use DROP DATABASE to drop invalid databases.")
            .fatal());
        }
        if !entry.allow_connections {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("database \"{dbname}\" is not currently accepting connections"),
            )
            .fatal());
        }
        // Create the session under the lock so the database can't be closed between the
        // check that it is still open and the session count going up; its roles must be up
        // to date first (which can't happen under that lock). (The idle closer leaves a
        // database alone while this holds it; DROP / RENAME / a template copy don't, and
        // then the steps may fail on the closed store: it is opened again, or reported gone.)
        let session = loop {
            let db = self.open_db(&entry)?;
            let ready = self
                .ensure_synced(&db)
                .and_then(|_| self.upgrade_index_keys(&db))
                .and_then(|_| self.upgrade_storage(&db));
            let open = self.open.lock();
            if !open.dbs.get(&entry.oid).is_some_and(|d| Arc::ptr_eq(d, &db)) {
                continue;
            }
            ready?;
            break Session::new(SessionState::new(db, self.arc(), user, pid));
        };
        let (others, db_others) = {
            let b = self.backends.lock();
            let others = b
                .iter()
                .filter(|(p, b)| **p != pid && b.activity.user == user)
                .count();
            let db_others = b
                .iter()
                .filter(|(p, b)| **p != pid && b.database == entry.oid)
                .count();
            (others, db_others)
        };
        session.login(dbname, others, Some((entry.conn_limit, db_others)))?;
        // CheckMyDatabase: a collation version other than the provider's.
        let mut session = session;
        if let Some(w) = entry.locale().version_warning(dbname) {
            session.notices.push(w);
        }
        if let Some(b) = self.backends.lock().get_mut(&pid) {
            b.database = entry.oid;
            b.cancel = Some(session.cancel.clone());
            b.activity.database = dbname.to_string();
            b.activity.user = user.to_string();
            b.activity.application_name = application_name.to_string();
            b.activity.state = "idle".into();
        }
        Ok(session)
    }

    /// Update what pg_stat_activity shows for a backend.
    pub fn set_activity(&self, pid: i32, f: impl FnOnce(&mut Activity)) {
        if let Some(b) = self.backends.lock().get_mut(&pid) {
            f(&mut b.activity);
        }
    }

    /// CancelRequest: true if the key matched a backend.
    pub fn cancel(&self, pid: i32, secret: i32) -> bool {
        match self.backends.lock().get(&pid) {
            Some(b) if b.secret == secret => {
                if let Some(c) = &b.cancel {
                    c.store(true, Ordering::Relaxed);
                }
                true
            }
            _ => false,
        }
    }

    fn terminate(b: &Backend) {
        b.terminated.store(true, Ordering::Relaxed);
        if let Some(c) = &b.cancel {
            c.store(true, Ordering::Relaxed);
        }
        if let Some(s) = &b.socket {
            let _ = s.shutdown(Shutdown::Read);
        }
    }

    fn sessions_on(&self, oid: u32, except: Option<i32>) -> Vec<i32> {
        self.backends
            .lock()
            .iter()
            .filter(|(pid, b)| b.database == oid && Some(**pid) != except)
            .map(|(pid, _)| *pid)
            .collect()
    }

    /// Close databases nobody has used for `idle`.
    pub fn close_idle(&self, idle: Duration) {
        let closing: Vec<(Arc<Database>, Closing<'_>)> = {
            let mut open = self.open.lock();
            let ids: Vec<u32> = open
                .dbs
                .iter()
                .filter(|(_, db)| {
                    // Held by nothing but this map: no session, nor a login, a catalog sync
                    // or another database's statement working with it (each holds it; a
                    // new hold needs this lock).
                    Arc::strong_count(db) == 1
                        && db.sessions.load(Ordering::Acquire) == 0
                        && db.last_active.lock().elapsed() >= idle
                })
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| {
                    let db = open.dbs.remove(&id)?;
                    open.closing.insert(id);
                    Some((db, Closing { engine: self, oid: id }))
                })
                .collect()
        };
        for (db, _closing) in closing {
            if let Err(e) = db.persist_sequences() {
                warn!(database = %db.name, error = %e, "saving sequence positions failed");
            }
            match db.store.close() {
                Ok(()) => info!(database = %db.name, "closed idle database"),
                Err(e) => warn!(database = %db.name, error = %e, "closing idle database failed"),
            }
        }
    }

    /// The indexes whose keys changed since index key format `from` (non-temporary,
    /// stored): format 1, those with a column whose NULLS FIRST / LAST isn't its direction's
    /// default; format 2, those keyed under a collation that doesn't sort by bytes (an
    /// expression key's collation may be one when the database's or a column's is).
    fn reencoded_indexes(cat: &Catalog, from: u32) -> Vec<String> {
        let mut out = Vec::new();
        let icu_default = !crate::types::collation::default_for(cat).bytewise();
        for ix in cat.indexes.values() {
            let Some(t) = cat.table(ix.table) else { continue };
            let nulls = from < 1 && ix.columns.iter().any(|c| c.nulls_first != c.desc);
            let collated = from < 2
                && (!crate::sql::binder::column_key_colls(cat, t, ix).is_empty()
                    || (matches!(ix.method.as_str(), "btree" | "hash")
                        && ix.columns.iter().any(|c| matches!(c.key, crate::catalog::IndexKey::Expr(_)))
                        && (icu_default
                            || ix.columns.iter().any(|c| c.collation.is_some())
                            || t.columns.iter().any(|c| c.collation.is_some()))));
            if ix.id == 0 || !(nulls || collated) {
                continue;
            }
            let schema = cat.schema_name(t.schema);
            if t.partkey.is_some() || schema.starts_with("pg_temp") || schema.starts_with("pg_toast_temp") {
                continue;
            }
            out.push(format!(
                "{}.{}",
                crate::catalog::quote_ident(schema),
                crate::catalog::quote_ident(&ix.name)
            ));
        }
        out
    }

    /// An internal session on `db` as the bootstrap superuser.
    fn internal_session(&self, db: &Arc<Database>) -> Session {
        Session::new(SessionState::new(db.clone(), self.arc(), &self.opts.superuser, 0))
    }

    /// Record `db`'s index key format.
    fn set_index_key_format(db: &Arc<Database>, format: u32) -> Result<()> {
        for _ in 0..20 {
            let mut c: Catalog = (*db.catalog()).clone();
            c.index_key_format = format;
            let mut txn = db.begin(Isolation::ReadCommitted, false, Arc::new(AtomicBool::new(false)));
            txn.set_catalog(c)?;
            match txn.commit() {
                Ok(_) => return Ok(()),
                Err(e) if e.code == code::SERIALIZATION_FAILURE => continue,
                Err(e) => return Err(e),
            }
        }
        Err(internal(format!(
            "could not record the index key format of database \"{}\"",
            db.name
        )))
    }

    /// A database from before the current index key format: rebuild the indexes whose keys
    /// it changed, then record the format (once, before its first session).
    fn upgrade_index_keys(&self, db: &Arc<Database>) -> Result<()> {
        if db.catalog().index_key_format >= crate::storage::keys::INDEX_KEY_FORMAT {
            return Ok(());
        }
        let _g = self.index_upgrade.lock();
        let cat = db.catalog();
        if cat.index_key_format >= crate::storage::keys::INDEX_KEY_FORMAT {
            return Ok(());
        }
        let names = Self::reencoded_indexes(&cat, cat.index_key_format);
        drop(cat);
        if !names.is_empty() {
            let mut s = self.internal_session(db);
            for n in &names {
                s.query(&format!("REINDEX INDEX {n}"))?;
            }
            info!(database = %db.name, indexes = names.len(), "rebuilt indexes for the current key format");
        }
        Self::set_index_key_format(db, crate::storage::keys::INDEX_KEY_FORMAT)
    }

    /// Testing the upgrade: rebuild `dbname`'s affected indexes in the legacy key format and
    /// record that format.
    #[doc(hidden)]
    pub fn downgrade_index_keys_for_test(&self, dbname: &str) -> Result<()> {
        let entry = self
            .entry(dbname)
            .ok_or_else(|| internal(format!("no database {dbname}")))?;
        let db = self.open_db(&entry)?;
        let names = Self::reencoded_indexes(&db.catalog(), 0);
        let mut s = self.internal_session(&db);
        crate::storage::keys::with_legacy_index_keys(|| -> Result<()> {
            for n in &names {
                s.query(&format!("REINDEX INDEX {n}"))?;
            }
            Ok(())
        })?;
        Self::set_index_key_format(&db, 0)
    }

    /// Record `db`'s table storage format.
    fn set_storage_format(db: &Arc<Database>, format: u32) -> Result<()> {
        for _ in 0..20 {
            let mut c: Catalog = (*db.catalog()).clone();
            c.storage_format = format;
            let mut txn = db.begin(Isolation::ReadCommitted, false, Arc::new(AtomicBool::new(false)));
            txn.set_catalog(c)?;
            match txn.commit() {
                Ok(_) => return Ok(()),
                Err(e) if e.code == code::SERIALIZATION_FAILURE => continue,
                Err(e) => return Err(e),
            }
        }
        Err(internal(format!(
            "could not record the storage format of database \"{}\"",
            db.name
        )))
    }

    /// A database from before the current storage format: the tables whose rows are keyed
    /// by their primary key's values (deferrable ones' too, which could lose rows) are
    /// re-keyed by hidden row ids in heap order, their primary key made an index of its own
    /// and their other indexes pointed at the new keys; then the format is recorded (once,
    /// before its first session). A table moves in transactions of a bounded size (see
    /// `unkey_table`), so an interrupted upgrade carries on where it stopped.
    fn upgrade_storage(&self, db: &Arc<Database>) -> Result<()> {
        if db.catalog().storage_format >= crate::storage::keys::STORAGE_FORMAT {
            return Ok(());
        }
        let _g = self.index_upgrade.lock();
        let cat = db.catalog();
        if cat.storage_format >= crate::storage::keys::STORAGE_FORMAT {
            return Ok(());
        }
        // (A `roll_pk_clustered` table is keyed by its primary key on purpose.)
        let keyed: Vec<u32> = cat
            .tables
            .values()
            .filter(|t| t.storage_pkey().is_some() && !t.pk_clustered())
            .map(|t| t.oid)
            .collect();
        drop(cat);
        let mut rows = 0;
        for &oid in &keyed {
            rows += Self::unkey_table(db, oid)?;
        }
        if !keyed.is_empty() {
            info!(database = %db.name, tables = keyed.len(), rows, "re-keyed primary-keyed tables for the current storage format");
        }
        Self::set_storage_format(db, crate::storage::keys::STORAGE_FORMAT)
    }

    /// Run one batch of the storage upgrade in a transaction of its own (`f` writes it),
    /// again on a serialization failure; then count it (for tests).
    fn upgrade_batch(db: &Arc<Database>, f: &mut dyn FnMut(&mut Txn) -> Result<()>) -> Result<()> {
        for _ in 0..20 {
            let mut txn = db.begin(Isolation::ReadCommitted, false, Arc::new(AtomicBool::new(false)));
            f(&mut txn)?;
            match txn.commit() {
                Ok(_) => {
                    let n = UPGRADE_BATCHES.fetch_add(1, Ordering::Relaxed) + 1;
                    if UPGRADE_STOP_AFTER.load(Ordering::Relaxed) == n {
                        return Err(internal("storage upgrade stopped for a test"));
                    }
                    return Ok(());
                }
                Err(e) if e.code == code::SERIALIZATION_FAILURE => continue,
                Err(e) => return Err(e),
            }
        }
        Err(internal(format!(
            "could not move a batch of rows in database \"{}\"",
            db.name
        )))
    }

    /// The next batch of entries in `[start, end)` from `from` on: about `UPGRADE_BATCH_BYTES`
    /// of keys and values (at least one entry), and where the one after starts.
    fn upgrade_slice(
        txn: &Txn,
        from: &[u8],
        end: &[u8],
    ) -> Result<(Vec<(Vec<u8>, bytes::Bytes)>, Vec<u8>)> {
        let limit = UPGRADE_BATCH_BYTES.load(Ordering::Relaxed);
        let mut out = Vec::new();
        let mut bytes = 0;
        for e in txn.scan(from, end) {
            let (k, v) = e?;
            bytes += k.len() + v.len();
            out.push((k, v));
            if bytes >= limit {
                break;
            }
        }
        let next = match out.last() {
            Some((k, _)) => {
                let mut n = k.clone();
                n.push(0);
                n
            }
            None => end.to_vec(),
        };
        Ok((out, next))
    }

    /// Move one table's rows from its primary key's values to hidden row ids (see
    /// `upgrade_storage`); the number of rows moved. Each step goes in batches of about
    /// `UPGRADE_BATCH_BYTES`, each a transaction of its own that leaves the table's key space
    /// in a state the next run of the upgrade resumes from (the catalog changes last, so
    /// until then the table reads as it did):
    ///
    /// 1. Rows go from their keys to a staging space keyed in heap order (the rows' recorded
    ///    heap TIDs, then the rest in key order, as a heap laid out afresh puts them). The
    ///    batch that empties the table also records that the rows are staged.
    /// 2. In that order, each staged row gets the next row id: its new key, its primary key
    ///    entry, and a map from its old key to the new one.
    /// 3. Each other index's entries move, pointed at the new keys, under an index id of its
    ///    own (old and new keys can coincide: a separate space keeps them apart).
    /// 4. The map goes; then the catalog takes the new layout and the indexes' new ids.
    fn unkey_table(db: &Arc<Database>, oid: u32) -> Result<usize> {
        use crate::catalog::IndexKey;
        use crate::storage::keys::{index_prefix, prefix_end};
        let cat = db.catalog();
        let Some(t) = cat
            .tables
            .get(&oid)
            .filter(|t| t.storage_pkey().is_some() && !t.pk_clustered())
            .cloned()
        else {
            return Ok(0);
        };
        let mut nt = (*t).clone();
        let Some(pk) = t.pkey_index.and_then(|i| cat.indexes.get(&i)).cloned() else {
            return Err(internal(format!(
                "primary key index of table \"{}\" not found",
                t.name
            )));
        };
        // (The same ids on every run: the catalog is unchanged until the end.)
        let mut pix = (*pk).clone();
        // (Keyed under its columns' collations, as the current index key format has them.)
        let pcolls = crate::sql::binder::column_key_colls(&cat, &t, &pk);
        pix.id = nt.next_index_id;
        nt.next_index_id += 1;
        nt.pkey_unkeyed = true;
        let mut others = Vec::new();
        for ioid in &t.indexes {
            if let Some(ix) = cat.indexes.get(ioid).filter(|ix| ix.id != 0) {
                let mut moved = (**ix).clone();
                moved.id = nt.next_index_id;
                nt.next_index_id += 1;
                others.push((ix.id, moved));
            }
        }
        drop(cat);
        let data = index_prefix(oid, 0);
        let data_end = prefix_end(&data);
        let stage = index_prefix(oid, UPGRADE_STAGE_ROWS);
        let stage_end = prefix_end(&stage);
        let map = index_prefix(oid, UPGRADE_STAGE_MAP);
        let map_end = prefix_end(&map);
        let staged = index_prefix(oid, UPGRADE_STAGED);
        let mut rows = 0;

        // 1. Stage the rows in heap order.
        let done = {
            let txn = db.begin(Isolation::ReadCommitted, false, Arc::new(AtomicBool::new(false)));
            txn.get(&staged)?.is_some()
        };
        if !done {
            let mut from = data.clone();
            loop {
                // (Set by the batch that commits: a retried one starts over.)
                let (mut last, mut after) = (false, Vec::new());
                Self::upgrade_batch(db, &mut |txn| {
                    let (batch, next) = Self::upgrade_slice(txn, &from, &data_end)?;
                    for (key, bytes) in &batch {
                        let mut sk = stage.clone();
                        match crate::exec::scan::stored_tid(bytes) {
                            Some((block, off)) => {
                                sk.push(0);
                                sk.extend_from_slice(&block.to_be_bytes());
                                sk.extend_from_slice(&off.to_be_bytes());
                            }
                            None => sk.push(1),
                        }
                        sk.extend_from_slice(&key[data.len()..]);
                        txn.delete(key.clone())?;
                        txn.put(sk, bytes.to_vec())?;
                    }
                    last = batch.is_empty();
                    if last {
                        txn.put(staged.clone(), Vec::new())?;
                    }
                    after = next;
                    Ok(())
                })?;
                if last {
                    break;
                }
                from = after;
            }
        }

        // 2. Row ids in heap order, the primary key's entries, the map of old keys.
        let seq = crate::session::ddl::rowid_sequence(&nt);
        let mut from = stage.clone();
        loop {
            let (mut moved, mut after) = (0, Vec::new());
            Self::upgrade_batch(db, &mut |txn| {
                let (batch, next) = Self::upgrade_slice(txn, &from, &stage_end)?;
                let (last, called) = db.seq_state(&seq)?;
                let mut id = if called { last + 1 } else { last };
                for (sk, bytes) in &batch {
                    let suffix = match sk.get(stage.len()) {
                        Some(0) => &sk[stage.len() + 7..],
                        _ => &sk[stage.len() + 1..],
                    };
                    let row = crate::exec::scan::decode_table_row(&nt, bytes)?;
                    let nkey = crate::exec::scan::data_key(&nt, &row, Some(id));
                    id += 1;
                    let mut vals = Vec::with_capacity(pix.columns.len());
                    for c in &pix.columns {
                        match c.key {
                            IndexKey::Column(i) => {
                                vals.push(row.get(i).cloned().unwrap_or(crate::types::Value::Null))
                            }
                            IndexKey::Expr(_) => {
                                return Err(internal(format!(
                                    "primary key of table \"{}\" has an expression",
                                    t.name
                                )));
                            }
                        }
                    }
                    txn.delete(sk.clone())?;
                    txn.put(
                        crate::exec::scan::index_entry_key(&nt, &pix, &vals, &nkey, &pcolls),
                        nkey.clone(),
                    )?;
                    let mut mk = map.clone();
                    mk.extend_from_slice(suffix);
                    txn.put(mk, nkey.clone())?;
                    txn.put(nkey, bytes.to_vec())?;
                }
                if id > 1 && !batch.is_empty() {
                    db.setval(&seq, id - 1, true)?;
                }
                (moved, after) = (batch.len(), next);
                Ok(())
            })?;
            if moved == 0 {
                break;
            }
            rows += moved;
            from = after;
        }

        // 3. The other indexes' entries (ending with the data key's suffix and holding the
        // key), pointed at the new keys under their new ids.
        for (old_id, moved) in &others {
            let start = index_prefix(oid, *old_id);
            let end = prefix_end(&start);
            let to = index_prefix(oid, moved.id);
            let mut from = start.clone();
            loop {
                let (mut empty, mut after) = (false, Vec::new());
                Self::upgrade_batch(db, &mut |txn| {
                    let (batch, next) = Self::upgrade_slice(txn, &from, &end)?;
                    empty = batch.is_empty();
                    for (ek, ev) in &batch {
                        let mut nek = to.clone();
                        let mut mk = map.clone();
                        mk.extend_from_slice(ev.get(data.len()..).unwrap_or_default());
                        match txn.get(&mk)? {
                            Some(nkey) if ev.len() >= data.len() => {
                                let cut = ek.len().saturating_sub(ev.len() - data.len());
                                nek.extend_from_slice(&ek[start.len()..cut.max(start.len())]);
                                nek.extend_from_slice(&nkey[data.len()..]);
                                txn.put(nek, nkey.to_vec())?;
                            }
                            // (An entry of no row: kept as it was.)
                            _ => {
                                nek.extend_from_slice(&ek[start.len()..]);
                                txn.put(nek, ev.to_vec())?;
                            }
                        }
                        txn.delete(ek.clone())?;
                    }
                    after = next;
                    Ok(())
                })?;
                if empty {
                    break;
                }
                from = after;
            }
        }

        // 4. The map goes; the catalog takes the new layout.
        let mut from = map.clone();
        loop {
            let (mut empty, mut after) = (false, Vec::new());
            Self::upgrade_batch(db, &mut |txn| {
                let (batch, next) = Self::upgrade_slice(txn, &from, &map_end)?;
                empty = batch.is_empty();
                for (k, _) in batch {
                    txn.delete(k)?;
                }
                after = next;
                Ok(())
            })?;
            if empty {
                break;
            }
            from = after;
        }
        Self::upgrade_batch(db, &mut |txn| {
            txn.delete(staged.clone())?;
            let mut c: Catalog = (*db.catalog()).clone();
            c.indexes.insert(pix.oid, Arc::new(pix.clone()));
            for (_, moved) in &others {
                c.indexes.insert(moved.oid, Arc::new(moved.clone()));
            }
            c.tables.insert(oid, Arc::new(nt.clone()));
            txn.set_catalog(c)
        })?;
        // Its models were of the old keys: laid out again when next needed.
        let mut forget: Vec<u32> = vec![oid];
        forget.extend(t.indexes.iter().chain(&t.pkey_index));
        crate::exec::sizing::dropped(&forget, db);
        Ok(rows)
    }

    /// Testing the upgrade: move tables in batches of about this many bytes of row data
    /// (the default is tens of megabytes).
    #[doc(hidden)]
    pub fn set_upgrade_batch_bytes_for_test(bytes: usize) {
        UPGRADE_BATCH_BYTES.store(bytes.max(1), Ordering::Relaxed);
    }

    /// Testing the upgrade: the number of batch transactions committed so far, and stop
    /// (fail) the upgrade right after the `n`th (0: never).
    #[doc(hidden)]
    pub fn upgrade_batches_for_test(stop_after: usize) -> usize {
        UPGRADE_STOP_AFTER.store(stop_after, Ordering::Relaxed);
        UPGRADE_BATCHES.load(Ordering::Relaxed)
    }

    /// Testing the upgrade: record `dbname` as in the legacy storage format (its tables
    /// made with `storage::keys::with_legacy_pk_storage` are stored that way).
    #[doc(hidden)]
    pub fn downgrade_storage_for_test(&self, dbname: &str) -> Result<()> {
        let entry = self
            .entry(dbname)
            .ok_or_else(|| internal(format!("no database {dbname}")))?;
        let db = self.open_db(&entry)?;
        Self::set_storage_format(&db, 0)
    }

    /// Testing the catalog format upgrade: store `dbname`'s catalog as one value, as before
    /// catalog format 1 (`catalog::persist`), and the number of records that took.
    #[doc(hidden)]
    pub fn downgrade_catalog_format_for_test(&self, dbname: &str) -> Result<usize> {
        let entry = self
            .entry(dbname)
            .ok_or_else(|| internal(format!("no database {dbname}")))?;
        let db = self.open_db(&entry)?;
        let (from, to) = crate::catalog::persist::record_range();
        let ts = db.store.visible_ts();
        let records: Vec<Vec<u8>> = db
            .store
            .scan(from, to, ts)
            .map(|r| r.map(|(k, _)| k))
            .collect::<Result<_>>()?;
        let n = records.len();
        let writes = crate::catalog::persist::legacy_writes(&db.catalog(), records)?;
        db.store.commit(writes, |_| Ok(()))?;
        Ok(n)
    }

    /// Testing the catalog format upgrade: the number of catalog records `dbname` has
    /// stored.
    #[doc(hidden)]
    pub fn catalog_records_for_test(&self, dbname: &str) -> Result<usize> {
        let entry = self
            .entry(dbname)
            .ok_or_else(|| internal(format!("no database {dbname}")))?;
        let db = self.open_db(&entry)?;
        let (from, to) = crate::catalog::persist::record_range();
        let ts = db.store.visible_ts();
        Ok(db.store.scan(from, to, ts).count())
    }

    /// Testing the upgrade: `dbname`'s recorded storage format, and the number of its
    /// tables still stored by their primary key (`roll_pk_clustered` ones aside).
    #[doc(hidden)]
    pub fn storage_format_for_test(&self, dbname: &str) -> Result<(u32, usize)> {
        let entry = self
            .entry(dbname)
            .ok_or_else(|| internal(format!("no database {dbname}")))?;
        let db = self.open_db(&entry)?;
        let cat = db.catalog();
        let keyed = cat
            .tables
            .values()
            .filter(|t| t.storage_pkey().is_some() && !t.pk_clustered())
            .count();
        Ok((cat.storage_format, keyed))
    }

    /// Testing the upgrade: `dbname`'s recorded index key format.
    #[doc(hidden)]
    pub fn index_key_format_for_test(&self, dbname: &str) -> Result<u32> {
        let entry = self
            .entry(dbname)
            .ok_or_else(|| internal(format!("no database {dbname}")))?;
        let db = self.open_db(&entry)?;
        Ok(db.catalog().index_key_format)
    }

    /// Number of databases currently open (holding memory).
    pub fn open_databases(&self) -> usize {
        self.open.lock().dbs.len()
    }

    /// Flush and close everything (server shutdown).
    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        for b in self.backends.lock().values() {
            Self::terminate(b);
        }
        let dbs: Vec<(Arc<Database>, Closing<'_>)> = {
            let mut open = self.open.lock();
            let dbs: Vec<(u32, Arc<Database>)> = open.dbs.drain().collect();
            dbs.into_iter()
                .map(|(oid, db)| {
                    open.closing.insert(oid);
                    (db, Closing { engine: self, oid })
                })
                .collect()
        };
        for (db, _closing) in dbs {
            if let Err(e) = db.persist_sequences() {
                warn!(database = %db.name, error = %e, "saving sequence positions failed");
            }
            if let Err(e) = db.store.close() {
                warn!(database = %db.name, error = %e, "close failed");
            }
        }
        // Prepared transactions go from memory (their state records stay stored, for the
        // next start); they hold their databases.
        self.twophase.clear();
    }

    /// Close a database that must have no sessions (DROP / RENAME / template copy), and keep
    /// it closed while the result lives (its files are the caller's to copy, move or remove).
    /// `busy` is the error when sessions came in after the caller looked.
    fn close_now(&self, oid: u32, busy: impl FnOnce(usize) -> Error) -> Result<Closing<'_>> {
        let db = {
            let mut open = self.open.lock();
            while open.closing.contains(&oid) {
                self.reopenable.wait(&mut open);
            }
            // (Sessions count themselves under this lock, once they found it still open.)
            if let Some(n) = open
                .dbs
                .get(&oid)
                .map(|db| db.sessions.load(Ordering::SeqCst))
                .filter(|n| *n > 0)
            {
                return Err(busy(n));
            }
            let db = open.dbs.remove(&oid);
            open.closing.insert(oid);
            db
        };
        let closing = Closing { engine: self, oid };
        if let Some(db) = db {
            db.store.close()?;
        }
        Ok(closing)
    }

    fn busy(name: &str, n: usize) -> Error {
        Error::new(
            code::OBJECT_IN_USE,
            format!("database \"{name}\" is being accessed by other users"),
        )
        .detail(Self::others_detail(n))
    }

    fn source_busy(name: &str, n: usize) -> Error {
        Error::new(
            code::OBJECT_IN_USE,
            format!("source database \"{name}\" is being accessed by other users"),
        )
        .detail(Self::others_detail(n))
    }

    fn others_detail(n: usize) -> String {
        if n == 1 {
            "There is 1 other session using the database.".to_string()
        } else {
            format!("There are {n} other sessions using the database.")
        }
    }

    fn prepared_detail(n: usize) -> String {
        if n == 1 {
            "There is 1 prepared transaction using the database.".to_string()
        } else {
            format!("There are {n} prepared transactions using the database.")
        }
    }

    /// The "is being accessed by other users" error with errdetail_busy_db's detail.
    fn busy_with(name: &str, sessions: usize, prepared: usize) -> Error {
        let e = Error::new(
            code::OBJECT_IN_USE,
            format!("database \"{name}\" is being accessed by other users"),
        );
        e.detail(match (sessions, prepared) {
            (0, p) => Self::prepared_detail(p),
            (s, 0) => Self::others_detail(s),
            (s, p) => format!(
                "There are {s} other session(s) and {p} prepared transaction(s) using the database."
            ),
        })
    }

    fn save(&self, r: &Registry) -> Result<()> {
        write_registry(&self.opts.data_dir, r)
    }
}

/// A role name two roles have (after merging concurrent changes).
fn duplicate_role(s: &SharedCatalog) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    s.roles
        .values()
        .find(|r| !seen.insert(r.name.as_str()))
        .map(|r| r.name.clone())
}

fn write_registry(dir: &Path, r: &Registry) -> Result<()> {
    let tmp = dir.join(format!("{REGISTRY_FILE}.tmp"));
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(r).map_err(|e| internal(e.to_string()))?,
    )?;
    std::fs::rename(&tmp, dir.join(REGISTRY_FILE))?;
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), target)?;
        }
    }
    Ok(())
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| match e.file_type() {
                    Ok(t) if t.is_dir() => dir_size(&e.path()),
                    _ => e.metadata().map(|m| m.len()).unwrap_or(0),
                })
                .sum()
        })
        .unwrap_or(0)
}

impl Cluster for Engine {
    fn server_files(&self) -> Option<&crate::server_files::ServerFiles> {
        self.server_files.as_ref()
    }

    fn outbound(&self) -> Option<&crate::remote::outbound::Outbound> {
        self.outbound.as_ref()
    }

    fn listen_addr(&self) -> Option<std::net::SocketAddr> {
        self.listen.get().copied()
    }

    fn bootstrap_superuser(&self) -> String {
        self.opts.superuser.clone()
    }

    fn statements(&self) -> Option<&crate::ext::pg_stat_statements::Store> {
        self.statements.as_ref()
    }

    fn database_owner(&self, name: &str) -> Option<String> {
        self.registry
            .lock()
            .databases
            .iter()
            .find(|d| d.name == name)
            .map(|d| d.owner.clone())
    }

    fn set_database_owner(&self, name: &str, owner: &str) -> Result<()> {
        let mut r = self.registry.lock();
        if let Some(d) = r.databases.iter_mut().find(|d| d.name == name) {
            d.owner = owner.to_string();
        }
        self.save(&r)
    }

    fn database_options(&self, oid: u32) -> DbOptions {
        self.registry
            .lock()
            .databases
            .iter()
            .find(|d| d.oid == oid)
            .map_or_else(DbOptions::default, DbEntry::options)
    }

    fn set_database_options(&self, name: &str, options: DbOptions) -> Result<()> {
        let mut r = self.registry.lock();
        if let Some(d) = r.databases.iter_mut().find(|d| d.name == name) {
            d.set_options(options);
        }
        self.save(&r)
    }

    fn databases(&self) -> Vec<(u32, String, u32)> {
        let r = self.registry.lock();
        let owner = |name: &str| {
            r.shared
                .as_ref()
                .and_then(|s| s.roles.values().find(|x| x.name == name).map(|x| x.oid))
                .unwrap_or(crate::catalog::BOOTSTRAP_SUPERUSER_OID)
        };
        r.databases
            .iter()
            .map(|d| (d.oid, d.name.clone(), owner(&d.owner)))
            .collect()
    }

    fn activity(&self) -> Vec<Activity> {
        let mut v: Vec<Activity> = self
            .backends
            .lock()
            .values()
            .filter(|b| b.database != 0)
            .map(|b| b.activity.clone())
            .collect();
        v.sort_by_key(|a| a.pid);
        v
    }

    fn cancel_backend(&self, pid: i32) -> bool {
        match self.backends.lock().get(&pid) {
            Some(b) => {
                if let Some(c) = &b.cancel {
                    c.store(true, Ordering::Relaxed);
                }
                true
            }
            None => false,
        }
    }

    fn terminate_backend(&self, pid: i32) -> bool {
        match self.backends.lock().get(&pid) {
            Some(b) => {
                Self::terminate(b);
                true
            }
            None => false,
        }
    }

    fn create_database(
        &self,
        name: &str,
        owner: &str,
        template: Option<&str>,
        locale: &DbLocale,
        options: DbOptions,
    ) -> Result<()> {
        if self.entry(name).is_some() {
            return Err(Error::new(
                code::DUPLICATE_DATABASE,
                format!("database \"{name}\" already exists"),
            ));
        }
        let source = match template {
            None | Some("template0" | "template1") => None,
            Some(t) => {
                let e = self.entry(t).ok_or_else(|| {
                    Error::new(
                        code::INVALID_CATALOG_NAME,
                        format!("template database \"{t}\" does not exist"),
                    )
                })?;
                let n = self.sessions_on(e.oid, None).len();
                let p = self.twophase.in_database(e.oid);
                if p > 0 {
                    let mut err = Self::busy_with(t, n, p);
                    err.message = format!("source database \"{t}\" is being accessed by other users");
                    return Err(err);
                }
                if n > 0 {
                    return Err(Self::source_busy(t, n));
                }
                Some(e)
            }
        };
        let oid = self.oids.take(0, 1);
        let dir = self.db_dir(oid);
        if let Some(src) = &source {
            // A copy of a quiet database: its files are complete once it's closed (and stay
            // so while it is kept closed).
            let t = template.unwrap_or_default();
            let _closed = self.close_now(src.oid, |n| Self::source_busy(t, n))?;
            copy_dir(&self.db_dir(src.oid), &dir)?;
        } else {
            // Bootstrap now; it gets the cluster-wide catalogs when it's first opened.
            let store = Store::open(&dir, self.store_options())?;
            let db = Database::open(name, oid, store, owner)?;
            db.store.close()?;
        }
        let mut r = self.registry.lock();
        let mut entry = DbEntry {
            name: name.into(),
            oid,
            owner: owner.into(),
            allow_connections: true,
            locale: Some(locale.clone()),
            is_template: None,
            conn_limit: DATCONNLIMIT_UNLIMITED,
        };
        entry.set_options(options);
        r.databases.push(entry);
        self.save(&r)
    }

    fn database_locale(&self, name: &str) -> Option<DbLocale> {
        self.entry(name).map(|e| e.locale())
    }

    fn set_database_collversion(&self, name: &str, version: Option<String>) -> Result<()> {
        let mut r = self.registry.lock();
        if let Some(d) = r.databases.iter_mut().find(|d| d.name == name) {
            d.locale = Some(DbLocale {
                collversion: version,
                ..d.locale()
            });
        }
        self.save(&r)
    }

    fn config_settings(&self) -> Vec<(String, String)> {
        self.registry.lock().config.clone()
    }

    fn drop_database(
        &self,
        name: &str,
        if_exists: bool,
        force: bool,
        current: &str,
    ) -> Result<bool> {
        let Some(entry) = self.entry(name) else {
            if if_exists {
                return Ok(false);
            }
            return Err(Error::new(
                code::INVALID_CATALOG_NAME,
                format!("database \"{name}\" does not exist"),
            ));
        };
        // (dropdb: a template is dropped only once it is no longer marked one.)
        if entry.options().is_template {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                "cannot drop a template database",
            ));
        }
        if name == current {
            return Err(Error::new(
                code::OBJECT_IN_USE,
                "cannot drop the currently open database",
            ));
        }
        let prepared = self.twophase.in_database(entry.oid);
        if force && prepared > 0 {
            // TerminateOtherDBBackends.
            return Err(Error::new(
                code::OBJECT_IN_USE,
                format!("database \"{name}\" is being used by prepared transactions"),
            )
            .detail(Self::prepared_detail(prepared)));
        }
        let users = self.sessions_on(entry.oid, None);
        if !users.is_empty() || prepared > 0 {
            if !force {
                return Err(Self::busy_with(name, users.len(), prepared));
            }
            for pid in &users {
                self.terminate_backend(*pid);
            }
            // Give the terminated sessions a moment to go away.
            for _ in 0..100 {
                if self.sessions_on(entry.oid, None).is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let left = self.sessions_on(entry.oid, None).len();
            if left > 0 {
                return Err(Self::busy(name, left));
            }
        }
        let closed = self.close_now(entry.oid, |n| Self::busy(name, n))?;
        {
            let mut r = self.registry.lock();
            r.databases.retain(|d| d.oid != entry.oid);
            self.save(&r)?;
        }
        let _ = std::fs::remove_dir_all(self.db_dir(entry.oid));
        drop(closed);
        // dropDatabaseDependencies, DeleteSharedComments: what refers to it goes too.
        self.publish(|s| {
            let n = s.comments.len() + s.acls.len() + s.role_settings.len();
            s.comments.retain(|c| c.0 != entry.oid);
            s.acls.retain(|a| !(a.class == 1262 && a.oid == entry.oid));
            s.role_settings
                .retain(|x| x.database.as_deref() != Some(name));
            n != s.comments.len() + s.acls.len() + s.role_settings.len()
        })?;
        Ok(true)
    }

    fn rename_database(&self, from: &str, to: &str) -> Result<()> {
        let entry = self.entry(from).ok_or_else(|| {
            Error::new(
                code::INVALID_CATALOG_NAME,
                format!("database \"{from}\" does not exist"),
            )
        })?;
        if self.entry(to).is_some() {
            return Err(Error::new(
                code::DUPLICATE_DATABASE,
                format!("database \"{to}\" already exists"),
            ));
        }
        let users = self.sessions_on(entry.oid, None);
        if !users.is_empty() {
            return Err(Self::busy(from, users.len()));
        }
        let closed = self.close_now(entry.oid, |n| Self::busy(from, n))?;
        let mut r = self.registry.lock();
        if let Some(d) = r.databases.iter_mut().find(|d| d.oid == entry.oid) {
            d.name = to.to_string();
        }
        self.save(&r)?;
        drop(r);
        drop(closed);
        // pg_db_role_setting refers to it by name here.
        self.publish(|s| {
            let mut changed = false;
            for x in s
                .role_settings
                .iter_mut()
                .filter(|x| x.database.as_deref() == Some(from))
            {
                x.database = Some(to.to_string());
                changed = true;
            }
            changed
        })
    }

    fn start_time(&self) -> i64 {
        self.started
    }

    fn database_size(&self, name: &str) -> Option<i64> {
        let e = self.entry(name)?;
        Some(dir_size(&self.db_dir(e.oid)) as i64)
    }

    fn commit(&self, mut txn: Txn) -> Result<u64> {
        let Some(cat) = txn.changed_catalog().filter(|_| !txn.publish) else {
            return txn.commit();
        };
        let _g = self.shared_lock.lock();
        let (version, shared, dbs) = {
            let r = self.registry.lock();
            (
                r.shared_version,
                r.shared.clone(),
                r.databases.iter().map(|d| d.oid).collect::<BTreeSet<u32>>(),
            )
        };
        let Some(shared) = shared else {
            return txn.commit();
        };
        let mut mine = SharedCatalog::extract(&cat, &dbs);
        let changed = mine.hash() != cat.shared_stamp.1;
        let stale = cat.shared_stamp.0 != version;
        if stale {
            // Another database changed the shared catalogs since this transaction's DDL
            // started: keep their changes, and merge this one's into them.
            if changed {
                let base = self
                    .shared_history
                    .lock()
                    .iter()
                    .find(|h| h.0 == cat.shared_stamp.0)
                    .map(|h| h.1.clone());
                let Some(merged) = base.and_then(|b| SharedCatalog::merge(&b, &mine, &shared))
                else {
                    txn.rollback();
                    return Err(Error::new(
                        code::SERIALIZATION_FAILURE,
                        "could not serialize access due to concurrent update",
                    ));
                };
                if let Some(dup) = duplicate_role(&merged) {
                    txn.rollback();
                    return Err(Error::new(code::UNIQUE_VIOLATION, "duplicate key value violates unique constraint \"pg_authid_rolname_index\"")
                        .detail(format!("Key (rolname)=({dup}) already exists.")));
                }
                mine = merged;
            } else {
                let mut c: Catalog = (*cat).clone();
                shared.apply(&mut c, &dbs, version);
                txn.replace_catalog(c, true)?;
                return txn.commit();
            }
        } else if !changed {
            return txn.commit();
        }
        let mut c: Catalog = (*cat).clone();
        mine.apply(&mut c, &dbs, version + 1);
        txn.replace_catalog(c, stale)?;
        let db = txn.db.oid;
        let ts = txn.commit()?;
        {
            let mut r = self.registry.lock();
            // Databases record their owner by name.
            for (oid, role) in &mine.roles {
                if let Some(old) = shared.roles.get(oid).filter(|o| o.name != role.name) {
                    for d in r.databases.iter_mut().filter(|d| d.owner == old.name) {
                        d.owner = role.name.clone();
                    }
                }
            }
            let mine = Arc::new(mine);
            r.shared = Some(mine.clone());
            r.shared_version = version + 1;
            self.remember(version + 1, mine);
            if let Err(e) = self.save(&r) {
                warn!(error = %e, "saving shared catalogs failed");
            }
        }
        self.sync_open(Some(db));
        Ok(ts)
    }

    fn other_databases(&self, current: u32) -> Vec<(u32, String, Arc<Database>)> {
        let entries = self.registry.lock().databases.clone();
        let mut out = Vec::new();
        for e in entries.iter().filter(|e| e.oid != current) {
            match self.open_db(e) {
                Ok(db) => out.push((e.oid, e.name.clone(), db)),
                Err(err) => warn!(database = %e.name, error = %err, "opening database failed"),
            }
        }
        out
    }
}
