//! Per-connection state that statements can read and change: user, settings, sequences'
//! currval/lastval, advisory locks, the random generator.

use super::settings::Settings;
use crate::error::Result;
use crate::txn::Database;
use crate::types::DataType;
use crate::types::collation::DbLocale;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

/// What a session can ask of the server it runs in (the engine implements this).
pub trait Cluster: Send + Sync {
    /// (oid, name, owner oid)
    fn databases(&self) -> Vec<(u32, String, u32)>;
    /// pg_stat_activity rows.
    fn activity(&self) -> Vec<Activity>;
    fn cancel_backend(&self, pid: i32) -> bool;
    fn terminate_backend(&self, pid: i32) -> bool;
    /// Create database `name` from `template` (None: template1) with `locale` (what
    /// CREATE DATABASE settled from its options and the template's) and `options`
    /// (IS_TEMPLATE, ALLOW_CONNECTIONS, CONNECTION LIMIT).
    fn create_database(
        &self,
        name: &str,
        owner: &str,
        template: Option<&str>,
        locale: &DbLocale,
        options: DbOptions,
    ) -> Result<()>;
    /// A database's encoding and locale (its pg_database row's); None: no such database.
    fn database_locale(&self, _name: &str) -> Option<DbLocale> {
        Some(DbLocale::default())
    }
    /// Record a database's collation version (ALTER DATABASE ... REFRESH COLLATION VERSION).
    fn set_database_collversion(&self, _name: &str, _version: Option<String>) -> Result<()> {
        Err(crate::error::unsupported("ALTER DATABASE REFRESH COLLATION VERSION"))
    }
    /// The configuration files (postgresql.conf, postgresql.auto.conf).
    fn server_config(&self) -> Option<&crate::sysconf::ServerConfig> {
        None
    }
    /// The server's command-line settings (`roll-engine --max-prepared-transactions`, ...).
    fn command_line(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn drop_database(
        &self,
        name: &str,
        if_exists: bool,
        force: bool,
        current: &str,
    ) -> Result<bool>;
    fn rename_database(&self, from: &str, to: &str) -> Result<()>;
    /// Microseconds since 2000-01-01 when the server started.
    fn start_time(&self) -> i64;
    fn database_size(&self, name: &str) -> Option<i64>;
    /// The owner (role name) of database `name`.
    fn database_owner(&self, _name: &str) -> Option<String> {
        None
    }
    fn set_database_owner(&self, _name: &str, _owner: &str) -> Result<()> {
        Err(crate::error::unsupported("ALTER DATABASE OWNER"))
    }
    /// pg_database's datistemplate / datallowconn / datconnlimit of database `oid`.
    fn database_options(&self, _oid: u32) -> DbOptions {
        DbOptions::default()
    }
    fn set_database_options(&self, _name: &str, _options: DbOptions) -> Result<()> {
        Err(crate::error::unsupported("ALTER DATABASE"))
    }
    /// Commit `txn`; the engine also takes changes to the cluster-wide catalogs (roles,
    /// memberships, …) to every other database.
    fn commit(&self, txn: crate::txn::Txn) -> Result<u64> {
        txn.commit()
    }
    /// The other databases, opened, for what refers to roles there: (oid, name, database).
    fn other_databases(&self, _current: u32) -> Vec<(u32, String, Arc<Database>)> {
        Vec::new()
    }
    /// The databases open now (sessions' locks and statistics are in them; a closed one
    /// has no sessions).
    fn opened_databases(&self) -> Vec<Arc<Database>> {
        Vec::new()
    }
    /// Backend `pid` waits for `event` (wait_event_type, wait_event) now, or no longer.
    fn set_wait(&self, _pid: i32, _event: Option<(&'static str, &'static str)>) {}
    /// Database `oid`'s cumulative statistics entry (pg_stat_database), if it has one.
    fn db_stats(&self, _oid: u32) -> Option<crate::exec::pgstat::DbEntry> {
        None
    }
    /// The directory server-side files are confined to, when they're allowed at all.
    fn server_files(&self) -> Option<&crate::server_files::ServerFiles> {
        None
    }
    /// The directory server programs run in, when they're allowed at all.
    fn server_programs(&self) -> Option<&std::path::Path> {
        None
    }
    /// pg_stat_statements' statistics, when the library is preloaded.
    fn statements(&self) -> Option<&crate::ext::pg_stat_statements::Store> {
        None
    }
    /// Where outbound connections may go (None: nowhere).
    fn outbound(&self) -> Option<&crate::remote::outbound::Outbound> {
        None
    }
    /// The server's listening address, when it serves the wire protocol.
    fn listen_addr(&self) -> Option<std::net::SocketAddr> {
        None
    }
    /// The superuser the cluster was bootstrapped with (the default user of outbound
    /// connections, as libpq's is the operating system user the server runs as).
    fn bootstrap_superuser(&self) -> String {
        "postgres".into()
    }
    /// The logical replication apply workers (None: subscriptions never run).
    fn logical_workers(&self) -> Option<Arc<crate::repl::apply::Workers>> {
        None
    }
}

/// A database's own options (pg_database's datistemplate, datallowconn, datconnlimit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DbOptions {
    pub is_template: bool,
    pub allow_connections: bool,
    /// DATCONNLIMIT_UNLIMITED, a limit, or DATCONNLIMIT_INVALID_DB.
    pub conn_limit: i32,
}

/// datconnlimit's "no limit".
pub const DATCONNLIMIT_UNLIMITED: i32 = -1;
/// datconnlimit of an invalid database (one a DROP DATABASE didn't finish, or a superuser
/// marked so): it takes no connections and is good only for DROP DATABASE.
pub const DATCONNLIMIT_INVALID_DB: i32 = -2;

impl Default for DbOptions {
    fn default() -> DbOptions {
        DbOptions {
            is_template: false,
            allow_connections: true,
            conn_limit: DATCONNLIMIT_UNLIMITED,
        }
    }
}

impl DbOptions {
    pub fn is_invalid(&self) -> bool {
        self.conn_limit == DATCONNLIMIT_INVALID_DB
    }
}

#[derive(Clone, Debug, Default)]
pub struct Activity {
    pub pid: i32,
    pub database: String,
    pub user: String,
    pub application_name: String,
    pub client_addr: Option<String>,
    /// The client's TCP port (None: not over TCP).
    pub client_port: Option<u16>,
    pub backend_start: i64,
    pub xact_start: Option<i64>,
    pub query_start: Option<i64>,
    /// When `state` last changed.
    pub state_change: Option<i64>,
    pub state: String,
    pub query: String,
    /// The transaction's id and its snapshot's xmin, as of the backend's last report (its
    /// own row reads them live).
    pub backend_xid: Option<u64>,
    pub backend_xmin: Option<u64>,
    /// What it waits for besides locks and its client (`Cluster::set_wait`).
    pub wait: Option<(&'static str, &'static str)>,
    /// A background worker's backend_type ("autovacuum worker"; None: a client backend).
    pub backend_type: Option<&'static str>,
    /// The backend's running VACUUM / ANALYZE progress (its session's `progress`).
    pub progress: Option<Arc<Progress>>,
}

/// A backend's command progress other backends read (st_progress_command / _param):
/// written with atomics, so reporting takes no lock.
#[derive(Debug, Default)]
pub struct Progress {
    /// PROGRESS_COMMAND_*: 0 none, 1 VACUUM, 2 ANALYZE.
    pub command: std::sync::atomic::AtomicU8,
    pub relid: std::sync::atomic::AtomicU32,
    pub params: [std::sync::atomic::AtomicI64; 20],
}

impl Progress {
    pub const VACUUM: u8 = 1;
    pub const ANALYZE: u8 = 2;

    /// pgstat_progress_start_command.
    pub fn start(&self, command: u8, relid: u32) {
        use std::sync::atomic::Ordering::Relaxed;
        self.command.store(0, Relaxed);
        for p in &self.params {
            p.store(0, Relaxed);
        }
        self.relid.store(relid, Relaxed);
        self.command.store(command, std::sync::atomic::Ordering::Release);
    }

    /// pgstat_progress_update_param (`i` from 0).
    pub fn set(&self, i: usize, v: i64) {
        self.params[i].store(v, std::sync::atomic::Ordering::Relaxed);
    }

    /// pgstat_progress_end_command.
    pub fn end(&self) {
        self.command.store(0, std::sync::atomic::Ordering::Release);
    }

    /// The command running (as pg_stat_get_progress_info names it) and its parameters.
    pub fn read(&self) -> Option<(&'static str, u32, [i64; 20])> {
        use std::sync::atomic::Ordering::{Acquire, Relaxed};
        let kind = match self.command.load(Acquire) {
            Self::VACUUM => "VACUUM",
            Self::ANALYZE => "ANALYZE",
            _ => return None,
        };
        Some((kind, self.relid.load(Relaxed), std::array::from_fn(|i| self.params[i].load(Relaxed))))
    }
}

/// A cluster with one database and nothing else (tests, embedded use).
pub struct SingleCluster {
    pub name: String,
    pub started: i64,
    /// The database's locale (what its catalog carries, `Database::set_locale`).
    pub locale: DbLocale,
    /// Its configuration files (postgresql.auto.conf in memory).
    pub conf: crate::sysconf::ServerConfig,
}

impl Cluster for SingleCluster {
    fn server_config(&self) -> Option<&crate::sysconf::ServerConfig> {
        Some(&self.conf)
    }
    fn databases(&self) -> Vec<(u32, String, u32)> {
        vec![(5, self.name.clone(), 10)]
    }
    fn database_locale(&self, name: &str) -> Option<DbLocale> {
        (name == self.name).then(|| self.locale.clone())
    }
    fn activity(&self) -> Vec<Activity> {
        Vec::new()
    }
    fn cancel_backend(&self, _: i32) -> bool {
        false
    }
    fn terminate_backend(&self, _: i32) -> bool {
        false
    }
    fn create_database(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &DbLocale,
        _: DbOptions,
    ) -> Result<()> {
        Err(crate::error::unsupported("CREATE DATABASE"))
    }
    fn drop_database(&self, _: &str, _: bool, _: bool, _: &str) -> Result<bool> {
        Err(crate::error::unsupported("DROP DATABASE"))
    }
    fn rename_database(&self, _: &str, _: &str) -> Result<()> {
        Err(crate::error::unsupported("ALTER DATABASE RENAME"))
    }
    fn start_time(&self) -> i64 {
        self.started
    }
    fn database_size(&self, _: &str) -> Option<i64> {
        None
    }
}

pub struct SessionState {
    pub db: Arc<Database>,
    pub cluster: Arc<dyn Cluster>,
    pub database: String,
    pub user: String,
    pub user_oid: u32,
    pub pid: i32,
    /// Lock-manager owner id for session-level advisory locks.
    pub lock_owner: u64,
    pub settings: RefCell<Settings>,
    /// pg_prng's xoroshiro128** state (s0, s1) for random() and friends.
    rng: Cell<(u64, u64)>,
    /// pg_global_prng_state: the backend's own generator (what C code draws from: pgvector's
    /// k-means sampling and HNSW levels), seeded at random when the session starts.
    global_rng: Cell<(u64, u64)>,
    pub lastval: Cell<Option<i64>>,
    /// The sequence lastval() reports (0: none).
    pub lastval_seq: Cell<u32>,
    /// Values of CACHE > 1 sequences this session took and hasn't handed out yet.
    pub seq_cache: RefCell<HashMap<u32, std::collections::VecDeque<i64>>>,
    pub currval: RefCell<HashMap<u32, i64>>,
    /// The plan cache of SQL functions' statements that run when called (functions.c's
    /// cached plans): by function and statement, how they were planned so far.
    pub sql_fn_plans: RefCell<HashMap<(u32, usize), SqlFnPlans>>,
    /// PL/pgSQL statements' CachedPlanSource counters, per function (oid, body) and
    /// statement, for the session (a DO block's are its run's: `Interp::do_plans`).
    pub pl_plans: RefCell<HashMap<(u32, u64, usize), PlPlans>>,
    /// Session-level advisory locks held: key → count.
    pub advisory: RefCell<HashMap<Vec<u8>, u32>>,
    /// Temporary tables with ON COMMIT DELETE ROWS (false) or DROP (true): what each
    /// commit does to them.
    pub on_commit: RefCell<Vec<(u32, bool)>>,
    /// Set while ON COMMIT DELETE ROWS empties temporary tables (heap_truncate: in place).
    pub on_commit_truncating: std::cell::Cell<bool>,
    /// A VACUUM or ANALYZE is running (no other may start inside it).
    pub in_vacuum: std::cell::Cell<bool>,
    /// The session is an autovacuum worker's (`autovacuum.rs`): its VACUUM / ANALYZE
    /// report as autovacuum's, and pg_stat_statements doesn't see its commands.
    pub autovacuum: std::cell::Cell<bool>,
    /// The running VACUUM's / ANALYZE's progress (shared with pg_stat_activity's entry).
    pub progress: Arc<Progress>,
    /// Cost-based vacuum delay of the running VACUUM / ANALYZE: (cost balance, delay in
    /// ms, cost limit, page-hit cost, whether delays are timed). Delay 0: off.
    pub vacuum_cost: std::cell::Cell<(i64, f64, i64, i64, bool)>,
    /// The running statement doesn't take a snapshot (transaction control, LOCK, SET,
    /// SHOW: PlannedStmtRequiresSnapshot), so a REPEATABLE READ / SERIALIZABLE
    /// transaction's snapshot waits for a later one.
    pub no_snapshot: std::cell::Cell<bool>,
    /// The backend's pending cumulative statistics (`exec::pgstat`).
    pub pgstat: RefCell<crate::exec::pgstat::Pending>,
    /// The current transaction used temporary objects (XACT_FLAGS_ACCESSEDTEMPNAMESPACE).
    pub temp_used: std::cell::Cell<bool>,
    /// The failing statement committed part of its work in transactions of its own (a
    /// CREATE INDEX CONCURRENTLY whose build failed leaves the index, INVALID): its
    /// implicit transaction commits when it fails.
    pub commit_on_error: std::cell::Cell<bool>,
    /// The session has used temporary tables (temp_buffers can't change any more).
    pub temp_ever: std::cell::Cell<bool>,
    /// pg_overexplain is loaded (LOAD): EXPLAIN takes its DEBUG and RANGE_TABLE options.
    pub overexplain: std::cell::Cell<bool>,
    pub backend_start: i64,
    /// pg_prepared_statements rows: (name, statement, prepare time, parameter types, result
    /// types, from SQL, generic plans, custom plans).
    /// SQL PREPAREd statements, for EXPLAIN EXECUTE run by a function (SPI).
    pub prepared_sql: RefCell<HashMap<String, Arc<crate::session::Prepared>>>,
    /// pg_stat_statements: the nesting level and prepared statements' query ids.
    pub pgss: RefCell<crate::ext::pg_stat_statements::SessionStats>,
    pub prepared_list: RefCell<
        Vec<(
            String,
            String,
            i64,
            Vec<DataType>,
            Option<Vec<DataType>>,
            bool,
            u64,
            u64,
        )>,
    >,
    /// Open cursors (portals) by name.
    pub cursors: RefCell<HashMap<String, super::cursor::Cursor>>,
    /// Names given to unnamed PL/pgSQL cursors (`<unnamed portal N>`).
    pub portal_seq: Cell<u32>,
    /// `plpgsql.variable_conflict` as each function body was first run in this session
    /// (Postgres fixes it when it compiles the function).
    pub pl_conflicts: RefCell<HashMap<String, crate::sql::binder::VarConflict>>,
    /// `plpgsql.print_strict_params` as each function body was first run in this session.
    pub pl_strict_params: RefCell<HashMap<String, bool>>,
    /// Each PL/pgSQL function's signature as CONTEXT lines show it, formatted at its first
    /// run in this session (by oid, with a hash of its schema, name, arguments and body).
    pub pl_signatures: RefCell<HashMap<u32, (u64, String)>>,
    /// PL/pgSQL functions (database, oid and body) compiled for a call in this session, and
    /// those (database, schema, name and body) its validator compiled, kept for calls whose
    /// input collation is the validator's (`plpgsql::compile_for_call`).
    pub pl_compiled: RefCell<std::collections::HashSet<(u64, u32, String)>>,
    pub pl_validated: RefCell<std::collections::HashSet<(u64, u32, String, String)>>,
    /// PL/pgSQL statements and expressions as first scanned in this session, with standard
    /// literals: what a cached plan's raw parse tree keeps (later settings of
    /// standard_conforming_strings don't change it, and its warnings are given once).
    pub pl_lexed: RefCell<HashMap<String, std::sync::Arc<str>>>,
    /// LISTEN (true) / UNLISTEN of a channel (None: all) in the current transaction, applied
    /// when it commits.
    pub listen_actions: RefCell<Vec<(bool, Option<String>)>>,
    /// A LISTEN of this session committed once (unlistenExitRegistered).
    pub ever_listened: Cell<bool>,
    /// The COPY running (pg_stat_progress_copy).
    pub copy_progress: RefCell<Option<CopyProgress>>,
    /// Owners of the SECURITY DEFINER functions running (innermost last): the current user.
    pub definer: RefCell<Vec<u32>>,
    /// Security-restricted operations running (REFRESH MATERIALIZED VIEW's query, ...):
    /// InSecurityRestrictedOperation.
    pub sro: Cell<u32>,
    /// What is running, outermost first: errors and notices raised in it get CONTEXT lines.
    pub frames: RefCell<Vec<crate::error::Frame>>,
    /// SYSTEM_USER: `auth_method:identity` when the client authenticated (None: trusted).
    pub system_user: Option<String>,
    /// Tables whose triggers are running (the statement firing them has them open):
    /// CheckTableNotInUse refuses DDL on them.
    pub in_use: RefCell<Vec<u32>>,
    /// Above 0: notices are collected, not sent as they are raised (code that looks at or
    /// drops what a step raised: `hold_notices`).
    pub notice_hold: Cell<u32>,
    /// Connections this session made to other servers (dblink's, postgres_fdw's cache).
    pub remote: RefCell<crate::remote::SessionConns>,
    /// The client's address (None: an embedded session).
    pub peer: Cell<Option<std::net::SocketAddr>>,
    /// The server's end of the connection (inet_server_addr / _port).
    pub local_addr: Cell<Option<std::net::SocketAddr>>,
    /// What the server log reads of the connection (addresses, line counters).
    pub log_conn: RefCell<crate::log::Conn>,
}

/// While alive, notices are collected instead of streamed (`SessionState::hold_notices`).
pub struct NoticeHold<'a>(&'a Cell<u32>);

impl Drop for NoticeHold<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

/// One PL/pgSQL statement's CachedPlanSource counters (choose_custom_plan).
#[derive(Default)]
pub struct PlPlans {
    pub custom: u32,
    pub custom_cost: f64,
    /// The generic plan's cost, and which plan it is of (a new binding is costed again).
    pub generic_cost: Option<(std::sync::Weak<crate::sql::plan::Plan>, f64)>,
}

/// One SQL function statement's CachedPlanSource counters (choose_custom_plan).
#[derive(Default)]
pub struct SqlFnPlans {
    /// The function's body these are of: a new definition starts over.
    pub body: Arc<str>,
    pub custom: u32,
    pub custom_cost: f64,
    pub generic_cost: Option<f64>,
}

impl SessionState {
    pub fn new(db: Arc<Database>, cluster: Arc<dyn Cluster>, user: &str, pid: i32) -> SessionState {
        let mut seed = [0u8; 8];
        let _ = getrandom::fill(&mut seed);
        let database = db.name.clone();
        let cat = db.catalog();
        let role = cat.role_by_name(user);
        let user_oid = role
            .map(|r| r.oid)
            .unwrap_or(crate::catalog::BOOTSTRAP_SUPERUSER_OID);
        let mut settings = Settings::new();
        if let Some(conf) = cluster.server_config() {
            settings.adopt_file_layer(conf.loaded());
            // The operator's files, absolute, as SelectConfigFiles sets them (PGC_S_OVERRIDE).
            let files = conf.auth_files();
            for (name, path) in [("hba_file", &files.hba_file), ("ident_file", &files.ident_file)] {
                if let Some(p) = path {
                    settings.set_server_start(name, &p.to_string_lossy());
                }
            }
        }
        let argv = cluster.command_line();
        for (name, value) in &argv {
            settings.set_command_line(name, value);
        }
        // Where the server listens (`roll-engine --listen`, as `postgres -h -p`): port, and
        // listen_addresses unless the command line names the host as given.
        if let Some(a) = cluster.listen_addr() {
            settings.set_command_line("port", &a.port().to_string());
            if !argv.iter().any(|(n, _)| n == "listen_addresses") {
                settings.set_command_line("listen_addresses", &a.ip().to_string());
            }
        }
        // The server's max_prepared_transactions (a postmaster setting; an embedder's).
        let max_prepared = db.twophase().max();
        if max_prepared > 0 && settings.get_str("max_prepared_transactions") != Some(max_prepared.to_string().as_str()) {
            settings.set_command_line("max_prepared_transactions", &max_prepared.to_string());
        }
        // A preloaded library's parameters are there from the start.
        if cluster.statements().is_some() {
            let _ = settings.load_module("pg_stat_statements");
            if settings.get_str("shared_preload_libraries").is_none_or(|v| !v.contains("pg_stat_statements")) {
                settings.set_command_line("shared_preload_libraries", "pg_stat_statements");
            }
        }
        settings.changed.clear();
        // The database's encoding is server_encoding, and client_encoding's default
        // (PGC_S_DYNAMIC_DEFAULT) until the client names its own.
        let enc = crate::types::collation::db_locale(&cat).encoding;
        if enc != crate::types::mbconv::UTF8 {
            let name = crate::types::mbconv::server::name(enc);
            settings.set_config_file("server_encoding", name);
            settings.set_config_file("client_encoding", name);
        }
        settings.set_internal("session_authorization", user);
        settings.set_internal(
            "is_superuser",
            if role.is_none_or(|r| r.superuser) {
                "on"
            } else {
                "off"
            },
        );
        SessionState {
            db,
            cluster,
            database,
            user: user.to_string(),
            user_oid,
            pid,
            lock_owner: (1u64 << 63) | pid as u32 as u64,
            settings: RefCell::new(settings),
            rng: Cell::new(prng_seeded(u64::from_le_bytes(seed))),
            global_rng: Cell::new(prng_seeded(u64::from_le_bytes(seed).rotate_left(29) ^ 0x6A09_E667_F3BC_C908)),
            lastval: Cell::new(None),
            lastval_seq: Cell::new(0),
            seq_cache: RefCell::new(HashMap::new()),
            currval: RefCell::new(HashMap::new()),
            sql_fn_plans: RefCell::new(HashMap::new()),
            pl_plans: RefCell::new(HashMap::new()),
            advisory: RefCell::new(HashMap::new()),
            on_commit: RefCell::new(Vec::new()),
            on_commit_truncating: std::cell::Cell::new(false),
            in_vacuum: std::cell::Cell::new(false),
            autovacuum: std::cell::Cell::new(false),
            progress: Arc::default(),
            vacuum_cost: std::cell::Cell::new((0, 0.0, 200, 1, false)),
            no_snapshot: Default::default(),
            pgstat: RefCell::new(crate::exec::pgstat::Pending::new_session()),
            temp_used: std::cell::Cell::new(false),
            commit_on_error: std::cell::Cell::new(false),
            temp_ever: std::cell::Cell::new(false),
            overexplain: std::cell::Cell::new(false),
            backend_start: crate::types::datetime::Clock::system().now,
            prepared_sql: RefCell::new(HashMap::new()),
            pgss: RefCell::default(),
            prepared_list: RefCell::new(Vec::new()),
            cursors: RefCell::new(HashMap::new()),
            pl_conflicts: RefCell::new(HashMap::new()),
            pl_strict_params: RefCell::new(HashMap::new()),
            pl_signatures: RefCell::new(HashMap::new()),
            pl_compiled: Default::default(),
            pl_validated: Default::default(),
            pl_lexed: Default::default(),
            listen_actions: RefCell::new(Vec::new()),
            ever_listened: Cell::new(false),
            copy_progress: RefCell::new(None),
            portal_seq: Cell::new(0),
            definer: RefCell::new(Vec::new()),
            sro: Cell::new(0),
            frames: RefCell::new(Vec::new()),
            system_user: None,
            in_use: RefCell::new(Vec::new()),
            notice_hold: Cell::new(0),
            remote: RefCell::new(Default::default()),
            peer: Cell::new(None),
            local_addr: Cell::new(None),
            log_conn: RefCell::default(),
        }
    }

    /// Send a notice to the client now, when the running statement's notices stream
    /// (`exec::streaming_notices`; PostgreSQL sends each one at once); false: collect it.
    pub fn stream_notice(&self, e: &crate::error::Error) -> bool {
        self.notice_hold.get() == 0 && crate::exec::send_notice_now(e)
    }

    /// Collect the notices raised until the guard goes, instead of sending them: for code
    /// that counts, moves or drops what a step raised.
    pub fn hold_notices(&self) -> NoticeHold<'_> {
        self.notice_hold.set(self.notice_hold.get() + 1);
        NoticeHold(&self.notice_hold)
    }

    /// Inside a security-restricted operation.
    pub fn security_restricted(&self) -> bool {
        self.sro.get() > 0
    }

    /// CheckTableNotInUse: `stmt` can't change a table the session's running statements (or
    /// open cursors) use.
    pub fn check_not_in_use(&self, oid: u32, stmt: &str, name: &str) -> Result<()> {
        let cursor = self
            .cursors
            .try_borrow()
            .is_ok_and(|c| c.values().any(|c| c.lazy.is_some() && c.rels.contains(&oid)));
        if cursor || self.in_use.borrow().contains(&oid) {
            return Err(crate::error::Error::new(
                crate::error::code::OBJECT_IN_USE,
                format!(
                    "cannot {stmt} \"{name}\" because it is being used by active queries in this session"
                ),
            ));
        }
        Ok(())
    }

    /// Run `f` inside `frame`: an error leaving it gets the CONTEXT lines of every frame
    /// it was raised in. `f` gets the frame's index (for `set_frame`).
    pub fn framed<T>(
        &self,
        frame: crate::error::Frame,
        f: impl FnOnce(usize) -> crate::error::Result<T>,
    ) -> crate::error::Result<T> {
        let n = {
            let mut frames = self.frames.borrow_mut();
            frames.push(frame);
            frames.len() - 1
        };
        // (Popped when this returns, on a panic too.)
        struct Pop<'a>(&'a RefCell<Vec<crate::error::Frame>>, usize);
        impl Drop for Pop<'_> {
            fn drop(&mut self) {
                if let Ok(mut f) = self.0.try_borrow_mut() {
                    f.truncate(self.1);
                }
            }
        }
        let _pop = Pop(&self.frames, n);
        f(n).map_err(|e| e.in_frames(&self.frames.borrow()))
    }

    /// After a panic ended the running statement: what its calls had pushed goes (the
    /// SECURITY DEFINER roles and restricted operations running, the frames, the tables in
    /// use, the COPY's progress).
    pub(crate) fn clear_running(&self) {
        self.definer.borrow_mut().clear();
        self.sro.set(0);
        self.frames.borrow_mut().clear();
        self.in_use.borrow_mut().clear();
        *self.copy_progress.borrow_mut() = None;
        self.in_vacuum.set(false);
        self.on_commit_truncating.set(false);
        self.notice_hold.set(0);
    }

    /// What `clear_running` clears that isn't at rest (outside any statement). For tests.
    pub fn running_for_test(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (name, set) in [
            ("definer", !self.definer.borrow().is_empty()),
            ("sro", self.sro.get() != 0),
            ("frames", !self.frames.borrow().is_empty()),
            ("in_use", !self.in_use.borrow().is_empty()),
            ("copy_progress", self.copy_progress.borrow().is_some()),
            ("in_vacuum", self.in_vacuum.get()),
            ("on_commit_truncating", self.on_commit_truncating.get()),
            ("notice_hold", self.notice_hold.get() != 0),
        ] {
            if set {
                out.push(name);
            }
        }
        out
    }

    /// Change frame `i` (a PL/pgSQL function moving to another statement).
    pub fn set_frame(&self, i: usize, at: crate::error::PlAt) {
        if let Some(crate::error::Frame::Pl { at: a, .. }) = self.frames.borrow_mut().get_mut(i) {
            *a = at;
        }
    }

    /// Where frame `i` is.
    pub fn frame_at(&self, i: usize) -> crate::error::PlAt {
        match self.frames.borrow().get(i) {
            Some(crate::error::Frame::Pl { at, .. }) => at.clone(),
            _ => crate::error::PlAt::Function,
        }
    }

    /// The CONTEXT lines of what is running now (PG_CONTEXT).
    pub fn context_now(&self) -> Option<String> {
        crate::error::frames_text(&self.frames.borrow())
    }

    /// session_user: the login role, or SET SESSION AUTHORIZATION's.
    pub fn session_role(&self, cat: &crate::catalog::Catalog) -> u32 {
        let st = self.settings.borrow();
        let name = st.session_authorization();
        if name.is_empty() || name == self.user {
            return self.user_oid;
        }
        cat.role_by_name(name).map_or(self.user_oid, |r| r.oid)
    }

    /// current_user: a SECURITY DEFINER function's owner, SET ROLE's role, or the session user.
    pub fn current_role(&self, cat: &crate::catalog::Catalog) -> u32 {
        if let Some(r) = self.definer.borrow().last() {
            return *r;
        }
        {
            let st = self.settings.borrow();
            let role = st.role();
            if !role.is_empty()
                && role != "none"
                && let Some(r) = cat.role_by_name(role)
            {
                return r.oid;
            }
        }
        self.session_role(cat)
    }

    /// The current database's owner.
    pub fn db_owner(&self, cat: &crate::catalog::Catalog) -> u32 {
        self.cluster
            .database_owner(&self.database)
            .and_then(|n| cat.role_by_name(&n).map(|r| r.oid))
            .unwrap_or(crate::catalog::BOOTSTRAP_SUPERUSER_OID)
    }

    /// Whether the current user has the privileges of pg_read_all_settings (superusers do):
    /// what GUC_SUPERUSER_ONLY parameters and pg_settings' sourcefile need.
    pub fn reads_all_settings(&self) -> bool {
        if self.settings.borrow().get_str("is_superuser") == Some("on") {
            return true;
        }
        let cat = self.db.catalog();
        cat.has_privs_of_role(self.current_role(&cat), crate::catalog::ROLE_PG_READ_ALL_SETTINGS, self.db_owner(&cat))
    }

    /// GetConfigOptionByName's check: a GUC_SUPERUSER_ONLY parameter (file locations,
    /// libraries, SSL settings) is examined only with pg_read_all_settings' privileges.
    pub fn check_examine(&self, name: &str) -> crate::error::Result<()> {
        match super::settings::superuser_only(name) {
            Some(canon) if !self.reads_all_settings() => Err(crate::error::Error::new(
                crate::error::code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to examine \"{canon}\""),
            )
            .detail("Only roles with privileges of the \"pg_read_all_settings\" role may examine this parameter.")),
            _ => Ok(()),
        }
    }

    /// The current database's pg_database oid.
    pub fn database_oid(&self) -> u32 {
        self.cluster
            .databases()
            .into_iter()
            .find(|d| d.1 == self.database)
            .map_or(self.db.oid, |d| d.0)
    }

    /// pg_prng_uint64: the next xoroshiro128** value.
    pub fn random_u64(&self) -> u64 {
        let (s0, s1) = self.rng.get();
        let sx = s1 ^ s0;
        let val = s0.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        self.rng
            .set((s0.rotate_left(24) ^ sx ^ (sx << 16), sx.rotate_left(37)));
        val
    }

    /// pg_prng_uint64 over pg_global_prng_state.
    pub fn global_random_u64(&self) -> u64 {
        let (s0, s1) = self.global_rng.get();
        let sx = s1 ^ s0;
        let val = s0.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        self.global_rng
            .set((s0.rotate_left(24) ^ sx ^ (sx << 16), sx.rotate_left(37)));
        val
    }

    /// pg_prng_uint32 over pg_global_prng_state.
    pub fn global_random_u32(&self) -> u32 {
        (self.global_random_u64() >> 32) as u32
    }

    /// pg_prng_double over pg_global_prng_state.
    pub fn global_random_double(&self) -> f64 {
        (self.global_random_u64() >> 12) as f64 * (2f64).powi(-52)
    }

    /// pg_prng_seed of pg_global_prng_state.
    pub fn global_seed(&self, seed: u64) {
        self.global_rng.set(prng_seeded(seed));
    }

    /// pg_prng_uint64_range: uniform in [rmin, rmax] (bitmask rejection).
    pub fn random_u64_range(&self, rmin: u64, rmax: u64) -> u64 {
        if rmax <= rmin {
            return rmin;
        }
        let range = rmax - rmin;
        let rshift = range.leading_zeros();
        loop {
            let v = self.random_u64() >> rshift;
            if v <= range {
                return rmin + v;
            }
        }
    }

    /// pg_prng_int64_range.
    pub fn random_i64_range(&self, rmin: i64, rmax: i64) -> i64 {
        if rmax <= rmin {
            return rmin;
        }
        let u = (rmin as u64).wrapping_add(self.random_u64_range(0, (rmax as u64).wrapping_sub(rmin as u64)));
        u as i64
    }

    /// pg_prng_double: uniform in [0, 1), 52 bits.
    pub fn random(&self) -> f64 {
        let v = self.random_u64();
        (v >> 12) as f64 * (2f64).powi(-52)
    }

    /// pg_prng_double_normal: Box-Muller over two uniform draws.
    pub fn random_normal(&self) -> f64 {
        let u1 = 1.0 - self.random();
        let u2 = self.random();
        // (The reference's draws come out with this sign.)
        -((-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).sin())
    }

    /// setseed: pg_prng_fseed.
    pub fn set_seed(&self, seed: f64) {
        let s = (((1u64 << 52) - 1) as f64 * seed) as i64;
        self.rng.set(prng_seeded(s as u64));
    }

    pub fn random_bytes(&self, out: &mut [u8]) {
        if getrandom::fill(out).is_err() {
            for b in out.iter_mut() {
                *b = (self.random() * 256.0) as u8;
            }
        }
    }

    /// The search path, with this session's temporary schema first (or where `pg_temp` is).
    pub fn search_path(&self) -> Vec<String> {
        // `$user` is the current user (SET ROLE, a SECURITY DEFINER function's owner).
        let cat = self.db.catalog();
        let oid = self.current_role(&cat);
        let user = cat.roles.get(&oid).map_or(self.user.as_str(), |r| r.name.as_str());
        self.settings
            .borrow()
            .session_search_path(user, self.pid, || self.temp_schema())
    }

    /// Name of this session's schema for temporary tables.
    pub fn temp_schema(&self) -> String {
        format!("pg_temp_{}", self.pid)
    }

    /// Release session-level advisory locks (pg_advisory_unlock_all, disconnect).
    pub fn release_advisory(&self) {
        // The map's keys end in the lock mode (0 exclusive, 1 shared).
        let mut keys: Vec<Vec<u8>> = self
            .advisory
            .borrow_mut()
            .drain()
            .map(|(mut k, _)| {
                k.pop();
                k
            })
            .collect();
        keys.sort();
        keys.dedup();
        if !keys.is_empty() {
            self.db.locks.release_all(self.lock_owner, keys);
        }
    }
}

impl Drop for SessionState {
    fn drop(&mut self) {
        self.release_advisory();
        // ReplicationSlotCleanup: its temporary slots go, the slots it uses are released.
        if let Some(r) = self.db.repl() {
            for db in r.slots.backend_exit(self.pid) {
                r.slots_changed(db);
            }
            r.origins.backend_exit(self.pid);
        }
        crate::exec::pgstat::session_ends(self);
    }
}

/// A running COPY's progress as pg_stat_get_progress_info('COPY') reports it.
#[derive(Clone, Debug, Default)]
pub struct CopyProgress {
    pub relid: u32,
    /// PROGRESS_COPY_COMMAND: 1 COPY FROM, 2 COPY TO.
    pub command: i64,
    /// PROGRESS_COPY_TYPE: 1 file, 2 program, 3 pipe, 4 callback.
    pub kind: i64,
    pub bytes_processed: i64,
    pub bytes_total: i64,
    pub tuples_processed: i64,
    pub tuples_excluded: i64,
    pub tuples_skipped: i64,
}

/// pg_prng_seed: the state splitmix64 makes of `seed` (never all zeroes).
fn prng_seeded(mut seed: u64) -> (u64, u64) {
    let mut splitmix = || {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut v = seed;
        v = (v ^ (v >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        v = (v ^ (v >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        v ^ (v >> 31)
    };
    let s0 = splitmix();
    let s1 = splitmix();
    if s0 == 0 && s1 == 0 {
        (0x5851_F42D_4C95_7F2D, 0x1405_7B7E_F767_814F)
    } else {
        (s0, s1)
    }
}
