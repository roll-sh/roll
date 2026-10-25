//! Per-connection state that statements can read and change: user, settings, sequences'
//! currval/lastval, advisory locks, the random generator.

use super::settings::Settings;
use crate::error::Result;
use crate::txn::Database;
use crate::types::DataType;
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
    fn create_database(&self, name: &str, owner: &str, template: Option<&str>) -> Result<()>;
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
    /// Commit `txn`; the engine also takes changes to the cluster-wide catalogs (roles,
    /// memberships, …) to every other database.
    fn commit(&self, txn: crate::txn::Txn) -> Result<u64> {
        txn.commit()
    }
    /// The other databases, opened, for what refers to roles there: (oid, name, database).
    fn other_databases(&self, _current: u32) -> Vec<(u32, String, Arc<Database>)> {
        Vec::new()
    }
    /// The directory server-side files are confined to, when they're allowed at all.
    fn server_files(&self) -> Option<&crate::server_files::ServerFiles> {
        None
    }
    /// pg_stat_statements' statistics, when the library is preloaded.
    fn statements(&self) -> Option<&crate::ext::pg_stat_statements::Store> {
        None
    }
}

#[derive(Clone, Debug, Default)]
pub struct Activity {
    pub pid: i32,
    pub database: String,
    pub user: String,
    pub application_name: String,
    pub client_addr: Option<String>,
    pub backend_start: i64,
    pub xact_start: Option<i64>,
    pub query_start: Option<i64>,
    pub state: String,
    pub query: String,
}

/// A cluster with one database and nothing else (tests, embedded use).
pub struct SingleCluster {
    pub name: String,
    pub started: i64,
}

impl Cluster for SingleCluster {
    fn databases(&self) -> Vec<(u32, String, u32)> {
        vec![(5, self.name.clone(), 10)]
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
    fn create_database(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
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
    pub lastval: Cell<Option<i64>>,
    /// The sequence lastval() reports (0: none).
    pub lastval_seq: Cell<u32>,
    /// Values of CACHE > 1 sequences this session took and hasn't handed out yet.
    pub seq_cache: RefCell<HashMap<u32, std::collections::VecDeque<i64>>>,
    pub currval: RefCell<HashMap<u32, i64>>,
    /// Session-level advisory locks held: key → count.
    pub advisory: RefCell<HashMap<Vec<u8>, u32>>,
    /// Temporary tables with ON COMMIT DELETE ROWS (false) or DROP (true): what each
    /// commit does to them.
    pub on_commit: RefCell<Vec<(u32, bool)>>,
    /// Set while ON COMMIT DELETE ROWS empties temporary tables (heap_truncate: in place).
    pub on_commit_truncating: std::cell::Cell<bool>,
    /// A VACUUM or ANALYZE is running (no other may start inside it).
    pub in_vacuum: std::cell::Cell<bool>,
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
    /// LISTEN (true) / UNLISTEN of a channel (None: all) in the current transaction, applied
    /// when it commits.
    pub listen_actions: RefCell<Vec<(bool, Option<String>)>>,
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
        // A preloaded library's parameters are there from the start.
        if cluster.statements().is_some() {
            let _ = settings.load_module("pg_stat_statements");
            settings.set_internal("shared_preload_libraries", "pg_stat_statements");
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
            lastval: Cell::new(None),
            lastval_seq: Cell::new(0),
            seq_cache: RefCell::new(HashMap::new()),
            currval: RefCell::new(HashMap::new()),
            advisory: RefCell::new(HashMap::new()),
            on_commit: RefCell::new(Vec::new()),
            on_commit_truncating: std::cell::Cell::new(false),
            in_vacuum: std::cell::Cell::new(false),
            no_snapshot: Default::default(),
            pgstat: RefCell::new(crate::exec::pgstat::Pending::new_session()),
            temp_used: std::cell::Cell::new(false),
            commit_on_error: std::cell::Cell::new(false),
            temp_ever: std::cell::Cell::new(false),
            backend_start: crate::types::datetime::Clock::system().now,
            prepared_sql: RefCell::new(HashMap::new()),
            pgss: RefCell::default(),
            prepared_list: RefCell::new(Vec::new()),
            cursors: RefCell::new(HashMap::new()),
            pl_conflicts: RefCell::new(HashMap::new()),
            pl_strict_params: RefCell::new(HashMap::new()),
            pl_signatures: RefCell::new(HashMap::new()),
            listen_actions: RefCell::new(Vec::new()),
            copy_progress: RefCell::new(None),
            portal_seq: Cell::new(0),
            definer: RefCell::new(Vec::new()),
            sro: Cell::new(0),
            frames: RefCell::new(Vec::new()),
            system_user: None,
            in_use: RefCell::new(Vec::new()),
        }
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
