//! Databases and transactions: snapshots over the MVCC store, buffered writes, row locks,
//! savepoints, sequences, and transactional catalog changes.

pub mod locks;
pub mod xids;

use crate::catalog::{Catalog, Sequence};
use crate::error::{Error, Result, code, internal};
use crate::storage::{SnapshotPin, Store};
use crate::{bail, err};
use bytes::Bytes;
use locks::{LockManager, LockMode, WaitPolicy};
use parking_lot::{Mutex, RwLock};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Reserved keyspace (table oid 0).
pub const CATALOG_KEY: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 1];
const SEQ_PREFIX: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 2];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl Isolation {
    pub fn name(&self) -> &'static str {
        match self {
            Isolation::ReadUncommitted => "read uncommitted",
            Isolation::ReadCommitted => "read committed",
            Isolation::RepeatableRead => "repeatable read",
            Isolation::Serializable => "serializable",
        }
    }

    pub fn parse(s: &str) -> Option<Isolation> {
        Some(match s.to_ascii_lowercase().as_str() {
            "read uncommitted" => Isolation::ReadUncommitted,
            "read committed" => Isolation::ReadCommitted,
            "repeatable read" => Isolation::RepeatableRead,
            "serializable" => Isolation::Serializable,
            _ => return None,
        })
    }

    /// Whether one snapshot covers the whole transaction.
    pub fn txn_snapshot(&self) -> bool {
        matches!(self, Isolation::RepeatableRead | Isolation::Serializable)
    }
}

/// In-memory sequence state: values are handed out from a range already made durable.
/// The CACHE of a table's hidden row id sequence (`rowid_sequence`): values are logged
/// ahead this many at a time.
pub const ROWID_CACHE: i64 = 1024;

struct SeqState {
    /// Last value returned (or the start, if not called yet).
    last: i64,
    is_called: bool,
    /// Values up to here are logged; beyond it we must log a new chunk first.
    logged_to: i64,
    /// The sequence tuple's log_cnt as Postgres keeps it (values logged ahead, 32 a time).
    log_cnt: i64,
}

pub struct Database {
    pub name: String,
    pub oid: u32,
    /// Unique among the databases this process opened (oids aren't: every engine, every
    /// store opened apart hands them out from the same start). What per-database state kept
    /// outside the database is keyed by.
    pub instance: u64,
    pub store: Arc<Store>,
    catalog: RwLock<Arc<Catalog>>,
    pub locks: LockManager,
    seqs: Mutex<HashMap<u32, SeqState>>,
    next_txn: AtomicU64,
    /// Open sessions; the engine closes a database once it reaches 0 and stays idle.
    pub sessions: AtomicUsize,
    /// Ended sessions still dropping their temporary objects; a new session's login waits
    /// for them, so its first statements don't race their catalog change.
    pub temp_cleanups: AtomicUsize,
    pub last_active: Mutex<Instant>,
    /// LISTEN / NOTIFY between this database's sessions.
    pub notify: crate::notify::Hub,
    /// Where lo_create(0) looks for a free large object oid next (Postgres's oid counter
    /// isn't transactional either).
    lo_oid: Mutex<u32>,
    /// The catalog version the last commit other than a shared-catalog update left.
    last_local: AtomicU64,
    /// Transaction ids (the engine's, shared by every database).
    xids: RwLock<Arc<xids::Xids>>,
    /// The modelled on-disk layout of tables and indexes (`exec::sizing`).
    pub relfiles: Mutex<crate::exec::sizing::RelFiles>,
    /// The cumulative statistics (`exec::pgstat`).
    pub pgstat: Mutex<crate::exec::pgstat::Shared>,
}

impl Database {
    pub fn open(name: &str, oid: u32, store: Arc<Store>, owner: &str) -> Result<Arc<Database>> {
        let ts = store.visible_ts();
        let catalog = match store.get(CATALOG_KEY, ts)? {
            Some(b) => {
                let mut c: Catalog = serde_json::from_slice(&b)
                    .map_err(|e| internal(format!("corrupt catalog: {e}")))?;
                c.reindex();
                c
            }
            None => {
                let c = Catalog::bootstrap(owner);
                let blob = serde_json::to_vec(&c).map_err(|e| internal(e.to_string()))?;
                store.commit(
                    vec![(CATALOG_KEY.to_vec(), Some(Bytes::from(blob)))],
                    |_| Ok(()),
                )?;
                c
            }
        };
        // Its own oid counter until it joins a cluster's (`set_oids`): like Postgres's, it
        // never goes back, so oids a failed statement took aren't handed out again.
        let mut catalog = catalog;
        catalog.oids = Some(Arc::new(crate::catalog::shared::OidGen::new(
            catalog.next_oid,
            |_| {},
        )));
        let version = catalog.version;
        static INSTANCES: AtomicU64 = AtomicU64::new(1);
        Ok(Arc::new(Database {
            name: name.to_string(),
            oid,
            instance: INSTANCES.fetch_add(1, Ordering::Relaxed),
            store,
            catalog: RwLock::new(Arc::new(catalog)),
            locks: LockManager::default(),
            seqs: Mutex::new(HashMap::new()),
            next_txn: AtomicU64::new(1),
            sessions: AtomicUsize::new(0),
            temp_cleanups: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            notify: Default::default(),
            lo_oid: Mutex::new(0),
            last_local: AtomicU64::new(version),
            xids: RwLock::new(Arc::new(xids::Xids::local())),
            relfiles: Mutex::new(Default::default()),
            pgstat: Mutex::new(Default::default()),
        }))
    }

    /// Allocate oids from the cluster's counter.
    pub fn set_oids(&self, g: Arc<crate::catalog::shared::OidGen>) {
        let mut w = self.catalog.write();
        let mut c = (**w).clone();
        c.oids = Some(g);
        *w = Arc::new(c);
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        self.catalog.read().clone()
    }

    /// Hand out transaction ids from the cluster's counter.
    pub fn set_xids(&self, x: Arc<xids::Xids>) {
        *self.xids.write() = x;
    }

    pub fn xids(&self) -> Arc<xids::Xids> {
        self.xids.read().clone()
    }

    /// A large object oid to try: at least `floor`, past every one handed out before.
    pub fn next_lo_oid(&self, floor: u32) -> u32 {
        let mut n = self.lo_oid.lock();
        let o = (*n).max(floor);
        *n = o.wrapping_add(1);
        o
    }

    pub fn touch(&self) {
        *self.last_active.lock() = Instant::now();
    }

    pub fn begin(
        self: &Arc<Self>,
        isolation: Isolation,
        read_only: bool,
        cancel: Arc<AtomicBool>,
    ) -> Txn {
        let id = self.next_txn.fetch_add(1, Ordering::Relaxed);
        let ts = self.store.visible_ts();
        let pin = self.store.pin(ts);
        Txn {
            id,
            xid: 0,
            db: self.clone(),
            isolation,
            read_only,
            snapshot_ts: ts,
            start_ts: ts,
            _pin: Some(pin),
            writes: BTreeMap::new(),
            journal: Vec::new(),
            rollbacks: 0,
            cid: 0,
            write_cid: 0,
            written: HashMap::new(),
            pg_cid: 0,
            pg_cid_used: false,
            combos: Vec::new(),
            sub_committed: Vec::new(),
            catalog: None,
            catalog_base: None,
            locks_held: HashSet::new(),
            savepoints: Vec::new(),
            cancel,
            lock_timeout: None,
            lock_peer: None,
            wrote: false,
            started: Instant::now(),
            seq_last: HashMap::new(),
            deferred: Vec::new(),
            deferred_triggers: Vec::new(),
            constraint_modes: HashMap::new(),
            all_deferred: None,
            notifies: Vec::new(),
            lo_descs: Vec::new(),
            publish: false,
            new_storage: HashMap::new(),
            rel_locks: Vec::new(),
            si_locks: Vec::new(),
            seq_undo: Vec::new(),
            pgstat: Default::default(),
            db_owner_undo: Vec::new(),
        }
    }

    fn seq_key(oid: u32) -> Vec<u8> {
        let mut k = SEQ_PREFIX.to_vec();
        k.extend_from_slice(&oid.to_be_bytes());
        k
    }

    fn load_seq(&self, seq: &Sequence) -> Result<SeqState> {
        let ts = self.store.visible_ts();
        Ok(match self.store.get(&Self::seq_key(seq.oid), ts)? {
            Some(b) if b.len() >= 9 => {
                let v = i64::from_le_bytes(b[0..8].try_into().unwrap());
                let called = b[8] != 0;
                // Everything up to v may have been handed out before a crash.
                SeqState {
                    last: v,
                    is_called: called,
                    logged_to: v,
                    log_cnt: 0,
                }
            }
            _ => SeqState {
                last: seq.start,
                is_called: false,
                logged_to: seq.start - seq.increment,
                log_cnt: 0,
            },
        })
    }

    fn log_seq(&self, oid: u32, value: i64, called: bool) -> Result<()> {
        let mut b = value.to_le_bytes().to_vec();
        b.push(called as u8);
        self.store
            .commit(vec![(Self::seq_key(oid), Some(Bytes::from(b)))], |_| Ok(()))?;
        Ok(())
    }

    /// nextval(): not transactional, like Postgres.
    pub fn nextval(&self, seq: &Sequence) -> Result<i64> {
        let mut seqs = self.seqs.lock();
        if let std::collections::hash_map::Entry::Vacant(e) = seqs.entry(seq.oid) {
            e.insert(self.load_seq(seq)?);
        }
        let st = seqs.get_mut(&seq.oid).unwrap();
        let next = if st.is_called {
            st.last.checked_add(seq.increment).unwrap_or(i64::MAX)
        } else {
            st.last
        };
        let next = if next > seq.max
            || next < seq.min
            || (st.is_called && st.last.checked_add(seq.increment).is_none())
        {
            if !seq.cycle {
                let which = if seq.increment > 0 {
                    "maximum"
                } else {
                    "minimum"
                };
                let limit = if seq.increment > 0 { seq.max } else { seq.min };
                bail!(
                    SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
                    "nextval: reached {which} value of sequence \"{}\" ({limit})",
                    seq.name
                );
            }
            if seq.increment > 0 { seq.min } else { seq.max }
        } else {
            next
        };
        // Log ahead in chunks so most calls don't write.
        let past_logged = if seq.increment > 0 {
            next > st.logged_to
        } else {
            next < st.logged_to
        };
        if past_logged {
            let chunk = seq.increment.saturating_mul(32.max(seq.cache));
            let mut to = next.saturating_add(chunk - seq.increment);
            to = to.clamp(seq.min, seq.max);
            self.log_seq(seq.oid, to, true)?;
            st.logged_to = to;
        }
        // nextval_internal: a fresh sequence, or one whose logged values ran out, logs
        // SEQ_LOG_VALS (32) ahead.
        let fetch = seq.cache.max(1);
        st.log_cnt = if st.log_cnt < fetch || !st.is_called {
            32
        } else {
            st.log_cnt - fetch
        };
        st.last = next;
        st.is_called = true;
        Ok(next)
    }

    /// `nextval(seq())` of a sequence counting up by one (a table's hidden row ids): while
    /// the next value is among those already logged ahead, taken without building the
    /// sequence's definition or writing anything (as `nextval` would take it).
    pub fn nextval_by_one(&self, oid: u32, seq: impl FnOnce() -> Sequence) -> Result<i64> {
        {
            let mut seqs = self.seqs.lock();
            if let Some(st) = seqs.get_mut(&oid)
                && st.is_called
                && st.last < st.logged_to
            {
                st.last += 1;
                st.log_cnt = if st.log_cnt < ROWID_CACHE {
                    32
                } else {
                    st.log_cnt - ROWID_CACHE
                };
                return Ok(st.last);
            }
        }
        let seq = seq();
        debug_assert!(seq.increment == 1 && seq.cache == ROWID_CACHE && seq.max == i64::MAX);
        self.nextval(&seq)
    }

    /// nextval() of a sequence with CACHE n: up to n values taken at once (fewer where
    /// it reaches its limit), the session handing out the rest.
    pub fn nextval_batch(&self, seq: &Sequence, n: i64) -> Result<Vec<i64>> {
        let (log_before, called_before) = {
            let mut seqs = self.seqs.lock();
            if let std::collections::hash_map::Entry::Vacant(e) = seqs.entry(seq.oid) {
                e.insert(self.load_seq(seq)?);
            }
            let st = &seqs[&seq.oid];
            (st.log_cnt, st.is_called)
        };
        let mut out = vec![self.nextval(seq)?];
        while (out.len() as i64) < n {
            match self.nextval(seq) {
                Ok(v) => out.push(v),
                Err(_) => break,
            }
        }
        if let Some(st) = self.seqs.lock().get_mut(&seq.oid) {
            st.log_cnt = if log_before < n || !called_before {
                32
            } else {
                log_before - n
            };
        }
        Ok(out)
    }

    pub fn setval(&self, seq: &Sequence, value: i64, is_called: bool) -> Result<()> {
        if value < seq.min || value > seq.max {
            bail!(
                NUMERIC_VALUE_OUT_OF_RANGE,
                "setval: value {value} is out of bounds for sequence \"{}\" ({}..{})",
                seq.name,
                seq.min,
                seq.max
            );
        }
        let mut seqs = self.seqs.lock();
        self.log_seq(seq.oid, value, is_called)?;
        seqs.insert(
            seq.oid,
            SeqState {
                last: value,
                is_called,
                logged_to: value,
                log_cnt: 0,
            },
        );
        Ok(())
    }

    /// (last_value, is_called) for `SELECT * FROM seq`.
    pub fn seq_state(&self, seq: &Sequence) -> Result<(i64, bool)> {
        let mut seqs = self.seqs.lock();
        if let std::collections::hash_map::Entry::Vacant(e) = seqs.entry(seq.oid) {
            e.insert(self.load_seq(seq)?);
        }
        let st = &seqs[&seq.oid];
        Ok((st.last, st.is_called))
    }

    /// The sequence's log_cnt (SELECT * FROM seq).
    pub fn seq_log_cnt(&self, seq: &Sequence) -> i64 {
        self.seqs.lock().get(&seq.oid).map_or(0, |s| s.log_cnt)
    }

    /// A sequence's whole state now (last value, is_called, logged up to, log_cnt), for a
    /// transactional reset to put back (`restore_seq`).
    pub fn seq_snapshot(&self, seq: &Sequence) -> Result<(i64, bool, i64, i64)> {
        let mut seqs = self.seqs.lock();
        if let std::collections::hash_map::Entry::Vacant(e) = seqs.entry(seq.oid) {
            e.insert(self.load_seq(seq)?);
        }
        let st = &seqs[&seq.oid];
        Ok((st.last, st.is_called, st.logged_to, st.log_cnt))
    }

    /// Put back a sequence's state (`seq_snapshot`): its reset rolled back, so its old
    /// file is the one in use again.
    pub fn restore_seq(&self, oid: u32, (last, is_called, logged_to, log_cnt): (i64, bool, i64, i64)) {
        let mut seqs = self.seqs.lock();
        let _ = self.log_seq(oid, logged_to, is_called);
        seqs.insert(
            oid,
            SeqState {
                last,
                is_called,
                logged_to,
                log_cnt,
            },
        );
    }

    /// Forget sequence state (ALTER SEQUENCE … RESTART, DROP).
    pub fn reset_seq(&self, oid: u32, value: Option<i64>) -> Result<()> {
        let mut seqs = self.seqs.lock();
        seqs.remove(&oid);
        match value {
            Some(v) => {
                self.log_seq(oid, v, false)?;
                seqs.insert(
                    oid,
                    SeqState {
                        last: v,
                        is_called: false,
                        logged_to: v,
                        log_cnt: 0,
                    },
                );
            }
            None => {
                let ts = self.store.visible_ts();
                if self.store.get(&Self::seq_key(oid), ts)?.is_some() {
                    self.store
                        .commit(vec![(Self::seq_key(oid), None)], |_| Ok(()))?;
                }
            }
        }
        Ok(())
    }
}

struct Savepoint {
    name: String,
    journal_len: usize,
    catalog: Option<Arc<Catalog>>,
    locks: HashSet<Vec<u8>>,
    deferred_len: usize,
    deferred_triggers_len: usize,
    notifies_len: usize,
    modes: (HashMap<u32, bool>, Option<bool>),
    /// The subtransaction's own transaction id, given when it first writes (0: none yet).
    xid: u64,
    /// Ids of subtransactions released into this one (they end as it does).
    committed: Vec<u64>,
    seq_undo_len: usize,
}

/// A deferred constraint trigger event, fired at COMMIT or SET CONSTRAINTS ... IMMEDIATE.
#[derive(Clone, Debug)]
pub struct DeferredTrigger {
    pub trigger: u32,
    /// The table it fires on, and the constraint SET CONSTRAINTS names it by (a constraint
    /// trigger's own oid, or a foreign key's).
    pub table: u32,
    pub constraint: u32,
    /// The new row's data key (empty: unknown), to skip rows gone since.
    pub key: Vec<u8>,
    pub event: u16,
    pub old: Option<Vec<crate::types::Value>>,
    pub new: Option<Vec<crate::types::Value>>,
    /// The role that queued it (the function runs as it).
    pub role: u32,
}

/// A foreign-key check postponed to COMMIT (or SET CONSTRAINTS ... IMMEDIATE).
#[derive(Clone, Debug)]
pub struct DeferredFk {
    pub constraint: u32,
    /// The referencing table.
    pub table: u32,
    /// Child side: the new row's key must have a parent. Parent side: the old key must be unreferenced.
    pub parent_side: bool,
    pub values: Vec<crate::types::Value>,
    /// A deferrable unique or exclusion constraint's recheck of the row stored under this
    /// data key (the other fields then only name the constraint and table).
    pub recheck: Option<Vec<u8>>,
    /// Checked when the statement ends (the constraint is deferrable but not deferred).
    pub stmt: bool,
}

pub struct Txn {
    /// Lock owner id (every transaction has one).
    pub id: u64,
    /// The Postgres transaction id, once it has one (0: none yet).
    xid: u64,
    pub db: Arc<Database>,
    pub isolation: Isolation,
    pub read_only: bool,
    /// Snapshot of the current statement (READ COMMITTED) or of the transaction.
    snapshot_ts: u64,
    pub start_ts: u64,
    _pin: Option<SnapshotPin>,
    writes: BTreeMap<Vec<u8>, Option<Bytes>>,
    /// Undo log: (key, what the buffer held before, the command id of the write it
    /// replaced), for savepoints.
    journal: Vec<(Vec<u8>, Option<Option<Bytes>>, Option<u32>)>,
    /// ROLLBACK TO SAVEPOINTs so far: the journal was cut back (`Snap`).
    rollbacks: u64,
    /// The command counter (CommandCounterIncrement): the last command id handed out.
    cid: u32,
    /// The command id this transaction's writes are stamped with now (the running
    /// statement's).
    write_cid: u32,
    /// The command id of the latest buffered write of each key.
    written: HashMap<Vec<u8>, u32>,
    /// Postgres's command counter as tuples' cmin / cmax show it (currentCommandId): it
    /// moves on only past a command that used it (currentCommandIdUsed), where `cid`
    /// moves on for every statement.
    pg_cid: u32,
    pg_cid_used: bool,
    /// The combo command ids handed out (combocid.c): (cmin, cmax) by combo id.
    combos: Vec<(u32, u32)>,
    /// Ids of subtransactions released into the top level (committed with it).
    sub_committed: Vec<u64>,
    catalog: Option<Arc<Catalog>>,
    /// Catalog version this transaction's DDL started from.
    catalog_base: Option<u64>,
    locks_held: HashSet<Vec<u8>>,
    savepoints: Vec<Savepoint>,
    pub cancel: Arc<AtomicBool>,
    pub lock_timeout: Option<Duration>,
    /// The session's own lock owner (session-level advisory locks), whose locks don't
    /// conflict with this transaction's.
    pub lock_peer: Option<u64>,
    wrote: bool,
    pub started: Instant,
    /// currval() per sequence within this session's transaction.
    pub seq_last: HashMap<u32, i64>,
    /// Pending deferred constraint checks.
    pub deferred: Vec<DeferredFk>,
    /// Pending deferred constraint trigger events.
    pub deferred_triggers: Vec<DeferredTrigger>,
    /// SET CONSTRAINTS name ... per constraint oid (true = DEFERRED).
    pub constraint_modes: HashMap<u32, bool>,
    /// SET CONSTRAINTS ALL ...
    pub all_deferred: Option<bool>,
    /// NOTIFY (channel, payload)s, sent when the transaction commits.
    pub notifies: Vec<(String, String)>,
    /// Open large-object descriptors (lo_open), by descriptor number.
    pub lo_descs: Vec<Option<crate::largeobj::Desc>>,
    /// It only brings the cluster-wide catalogs up to date (another database changed them).
    pub publish: bool,
    /// Relations given new storage (TRUNCATE, a rewrite) in this transaction, by the
    /// subtransaction depth that gave it (rd_newRelfilelocatorSubid).
    new_storage: HashMap<u32, usize>,
    /// Heavyweight relation locks pg_locks shows this transaction holding (relation oid,
    /// mode name), for the operations that record theirs (statistics import).
    pub rel_locks: Vec<(u32, &'static str)>,
    /// SSI predicate (SIRead) locks a serializable transaction's reads took, as pg_locks
    /// shows them: (relation, page, tuple); page and tuple None for a relation lock.
    pub si_locks: Vec<(u32, Option<u32>, Option<u16>)>,
    /// Sequences given a new file here (TRUNCATE ... RESTART IDENTITY), with the state of
    /// the old one, which a rollback makes current again.
    pub seq_undo: Vec<(u32, (i64, bool, i64, i64))>,
    /// The transactional statistics counts (`exec::pgstat`).
    pub pgstat: crate::exec::pgstat::XactStats,
    /// Databases whose owner this transaction changed: (name, the previous owner), put
    /// back when it rolls back.
    pub db_owner_undo: Vec<(String, String)>,
}

impl Txn {
    /// Whether this transaction's reads take SIRead predicate locks (a serializable one
    /// that may write: a read-only one is treated as running on a safe snapshot).
    pub fn takes_predicate_locks(&self) -> bool {
        self.isolation == Isolation::Serializable && !self.read_only
    }

    /// PredicateLockRelation / PredicateLockTID: record an SIRead lock on a relation, or on
    /// one tuple of it, coarsening as Postgres does: a covered lock isn't taken, more than
    /// max_pred_locks_per_page (2) tuple locks on a page become a page lock, more than
    /// max_pred_locks_per_relation (-2: 64 / 2) page or tuple locks a relation lock.
    pub fn predicate_lock(&mut self, rel: u32, tid: Option<(u32, u16)>) {
        let locks = &mut self.si_locks;
        if locks.iter().any(|l| l.0 == rel && l.1.is_none()) {
            return;
        }
        let Some((page, tuple)) = tid else {
            locks.retain(|l| l.0 != rel);
            locks.push((rel, None, None));
            return;
        };
        if locks
            .iter()
            .any(|l| *l == (rel, Some(page), None) || *l == (rel, Some(page), Some(tuple)))
        {
            return;
        }
        locks.push((rel, Some(page), Some(tuple)));
        if locks
            .iter()
            .filter(|l| l.0 == rel && l.1 == Some(page) && l.2.is_some())
            .count()
            > 2
        {
            locks.retain(|l| !(l.0 == rel && l.1 == Some(page)));
            locks.push((rel, Some(page), None));
        }
        if locks.iter().filter(|l| l.0 == rel).count() > 32 {
            locks.retain(|l| l.0 != rel);
            locks.push((rel, None, None));
        }
    }

    /// Queue a NOTIFY; an identical one already queued is not repeated.
    pub fn notify(&mut self, channel: &str, payload: &str) {
        if !self
            .notifies
            .iter()
            .any(|(c, p)| c == channel && p == payload)
        {
            self.notifies
                .push((channel.to_string(), payload.to_string()));
        }
    }
}

impl Txn {
    pub fn snapshot_ts(&self) -> u64 {
        self.snapshot_ts
    }

    /// Start a statement: READ COMMITTED takes a fresh snapshot.
    pub fn begin_statement(&mut self) {
        if !self.isolation.txn_snapshot() {
            let ts = self.db.store.visible_ts();
            if ts != self.snapshot_ts {
                self._pin = Some(self.db.store.pin(ts));
                self.snapshot_ts = ts;
            }
        }
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        match &self.catalog {
            Some(c) => c.clone(),
            None => self.db.catalog(),
        }
    }

    pub fn has_writes(&self) -> bool {
        self.wrote
    }

    /// Replace the catalog (DDL). It's written at commit.
    pub fn set_catalog(&mut self, mut c: Catalog) -> Result<()> {
        if self.read_only {
            bail!(
                READ_ONLY_SQL_TRANSACTION,
                "cannot execute DDL in a read-only transaction"
            );
        }
        if self.catalog_base.is_none() {
            self.catalog_base = Some(self.db.catalog().version);
        }
        c.bump();
        let blob = serde_json::to_vec(&c).map_err(|e| internal(e.to_string()))?;
        self.buffer(CATALOG_KEY.to_vec(), Some(Bytes::from(blob)));
        self.catalog = Some(Arc::new(c));
        Ok(())
    }

    /// Replace the catalog by in-place updates (plain VACUUM's statistics), which give the
    /// transaction no id.
    pub fn set_catalog_inplace(&mut self, mut c: Catalog) -> Result<()> {
        if self.read_only {
            bail!(
                READ_ONLY_SQL_TRANSACTION,
                "cannot execute DDL in a read-only transaction"
            );
        }
        if self.catalog_base.is_none() {
            self.catalog_base = Some(self.db.catalog().version);
        }
        c.bump();
        let blob = serde_json::to_vec(&c).map_err(|e| internal(e.to_string()))?;
        self.record(CATALOG_KEY.to_vec(), Some(Bytes::from(blob)));
        self.wrote = true;
        self.catalog = Some(Arc::new(c));
        Ok(())
    }

    /// The catalog this transaction's DDL produced (None: it has none).
    pub fn changed_catalog(&self) -> Option<Arc<Catalog>> {
        self.catalog.clone()
    }

    /// Swap the changed catalog for `c` (same version) just before committing. With `rebase`,
    /// it moves onto the database's current catalog if only shared-catalog updates were
    /// committed since this transaction's DDL began (`c` must carry those already).
    pub fn replace_catalog(&mut self, mut c: Catalog, rebase: bool) -> Result<()> {
        if rebase
            && let Some(base) = self.catalog_base
            && self.db.last_local.load(Ordering::Acquire) <= base
        {
            let cur = self.db.catalog().version;
            self.catalog_base = Some(cur);
            // (A new version, unique as `bump` makes them: `cur + 1` may be one another
            // transaction's catalog already has, which caches keyed by the version take for
            // this one.)
            c.version = cur;
            c.bump();
        }
        let blob = serde_json::to_vec(&c).map_err(|e| internal(e.to_string()))?;
        self.writes
            .insert(CATALOG_KEY.to_vec(), Some(Bytes::from(blob)));
        self.catalog = Some(Arc::new(c));
        Ok(())
    }

    fn buffer(&mut self, key: Vec<u8>, value: Option<Bytes>) {
        self.record(key, value);
        self.wrote = true;
        self.cur_xid();
    }

    /// Put a write in the buffer, stamped with the current command id, and journal it.
    fn record(&mut self, key: Vec<u8>, value: Option<Bytes>) {
        let prev = self.writes.insert(key.clone(), value);
        let prev_cid = self.written.insert(key.clone(), self.write_cid);
        self.journal.push((key, prev, prev_cid));
    }

    /// CommandCounterIncrement: a new command id (see `set_write_cid`).
    pub fn next_cid(&mut self) -> u32 {
        self.cid += 1;
        if self.pg_cid_used {
            self.pg_cid += 1;
            self.pg_cid_used = false;
        }
        self.cid
    }

    /// GetCurrentCommandId(true): the command id (as Postgres numbers them) the running
    /// command marks the tuples it writes or locks with, which the next command then
    /// moves past.
    pub fn output_cid(&mut self) -> u32 {
        self.pg_cid_used = true;
        self.pg_cid
    }

    /// HeapTupleHeaderAdjustCmax: the raw command id a tuple this transaction deletes now
    /// keeps, given the one it had (`cmin`) and whether this transaction inserted it: a
    /// combo command id standing for both, or else just the deleting command's.
    pub fn adjust_cmax(&mut self, cmin: u32, inserted_here: bool) -> u32 {
        let cmax = self.output_cid();
        if !inserted_here {
            return cmax;
        }
        match self.combos.iter().position(|&c| c == (cmin, cmax)) {
            Some(i) => i as u32,
            None => {
                self.combos.push((cmin, cmax));
                self.combos.len() as u32 - 1
            }
        }
    }

    /// Stamp this transaction's writes from now on with command id `cid`; returns the one
    /// they were stamped with.
    pub fn set_write_cid(&mut self, cid: u32) -> u32 {
        std::mem::replace(&mut self.write_cid, cid)
    }

    /// The command id of this transaction's latest write of `key` (None: it hasn't written
    /// it), as a tuple's cmin / cmax tells which command made it.
    pub fn written_by(&self, key: &[u8]) -> Option<u32> {
        if !self.writes.contains_key(key) {
            return None;
        }
        self.written.get(key).copied()
    }

    /// This transaction's id, given it now if it has none (as writing or
    /// pg_current_xact_id() does).
    pub fn xid(&mut self) -> u64 {
        if self.xid == 0 {
            self.xid = self.db.xids().assign();
        }
        self.xid
    }

    /// The id of the running (sub)transaction, which what it writes is stamped with
    /// (GetCurrentTransactionId): a subtransaction gets one of its own when it first
    /// writes, its parents (and the top level) theirs first.
    pub fn cur_xid(&mut self) -> u64 {
        let top = self.xid();
        let xids = self.db.xids();
        let mut cur = top;
        for sp in &mut self.savepoints {
            if sp.xid == 0 {
                sp.xid = xids.assign_sub();
            }
            cur = sp.xid;
        }
        cur
    }

    /// TransactionIdIsCurrentTransactionId: `x` is this transaction's id or one of its
    /// subtransactions' (open, or released into it).
    pub fn is_current_xid(&self, x: u64) -> bool {
        x != 0
            && (x == self.xid
                || self.sub_committed.contains(&x)
                || self
                    .savepoints
                    .iter()
                    .any(|sp| sp.xid == x || sp.committed.contains(&x)))
    }

    /// The transaction id, if it has been given one.
    pub fn xid_if_assigned(&self) -> Option<u64> {
        (self.xid != 0).then_some(self.xid)
    }

    /// Record how the transaction ended (its subtransactions' ids end with it).
    fn finish_xid(&mut self, committed: bool) {
        let undo = std::mem::take(&mut self.seq_undo);
        let renewed = std::mem::take(&mut self.new_storage);
        if !committed {
            for (oid, st) in undo.into_iter().rev() {
                self.db.restore_seq(oid, st);
            }
            // The relations' old files are in use again.
            for rel in renewed.into_keys() {
                crate::exec::sizing::storage_rolled_back(rel, &self.db);
            }
        }
        let xids = self.db.xids();
        let subs: Vec<u64> = std::mem::take(&mut self.sub_committed)
            .into_iter()
            .chain(
                self.savepoints
                    .iter_mut()
                    .flat_map(|sp| std::mem::take(&mut sp.committed).into_iter().chain([sp.xid])),
            )
            .filter(|&x| x != 0)
            .collect();
        for sp in &mut self.savepoints {
            sp.xid = 0;
        }
        for x in subs {
            xids.finish(x, committed);
        }
        if self.xid != 0 {
            xids.finish(self.xid, committed);
            self.xid = 0;
        }
    }

    /// A temporary table's key: a read-only transaction may still write it
    /// (ExecCheckXactReadOnly skips temporary relations).
    fn temp_key(&self, key: &[u8]) -> bool {
        let Some(b) = key.get(..4) else {
            return false;
        };
        let oid = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let cat = self.catalog();
        cat.tables
            .get(&oid)
            .is_some_and(|t| cat.schema_name(t.schema).starts_with("pg_temp"))
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        if self.read_only && !self.temp_key(&key) {
            bail!(
                READ_ONLY_SQL_TRANSACTION,
                "cannot execute this statement in a read-only transaction"
            );
        }
        self.buffer(key, Some(Bytes::from(value)));
        Ok(())
    }

    pub fn delete(&mut self, key: Vec<u8>) -> Result<()> {
        if self.read_only && !self.temp_key(&key) {
            bail!(
                READ_ONLY_SQL_TRANSACTION,
                "cannot execute this statement in a read-only transaction"
            );
        }
        self.buffer(key, None);
        Ok(())
    }

    /// Read through this transaction's own writes.
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        if let Some(v) = self.writes.get(key) {
            return Ok(v.clone());
        }
        self.db.store.get(key, self.snapshot_ts)
    }

    /// Newest committed version (ignoring our snapshot), for locking reads after a wait.
    pub fn get_latest(&self, key: &[u8]) -> Result<Option<(u64, Option<Bytes>)>> {
        if let Some(v) = self.writes.get(key) {
            return Ok(Some((u64::MAX, v.clone())));
        }
        self.db.store.get_version(key, u64::MAX >> 1)
    }

    /// Read through this transaction's own writes over the data committed as of `ts`.
    pub fn get_at(&self, key: &[u8], ts: u64) -> Result<Option<Bytes>> {
        if let Some(v) = self.writes.get(key) {
            return Ok(v.clone());
        }
        self.db.store.get(key, ts)
    }

    /// Scan [start, end) merging our buffered writes over the snapshot.
    pub fn scan(&self, start: &[u8], end: &[u8]) -> TxnScan {
        self.scan_at(start, end, self.snapshot_ts)
    }

    /// Scan [start, end) over the newest committed data plus our writes (uniqueness checks).
    pub fn scan_latest(&self, start: &[u8], end: &[u8]) -> TxnScan {
        self.scan_at(start, end, self.db.store.visible_ts())
    }

    /// Scan [start, end) merging our buffered writes over the data committed as of `ts`.
    pub fn scan_at(&self, start: &[u8], end: &[u8], ts: u64) -> TxnScan {
        self.scan_overlay(start, end, ts, None)
    }

    /// Our buffered writes in [start, end), as they are now.
    pub fn writes_in(&self, start: &[u8], end: &[u8]) -> BTreeMap<Vec<u8>, Option<Bytes>> {
        self.writes
            .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Scan [start, end) at `ts`, merging `own` (writes captured earlier; None: our current
    /// writes) over it.
    pub fn scan_overlay(
        &self,
        start: &[u8],
        end: &[u8],
        ts: u64,
        own: Option<&BTreeMap<Vec<u8>, Option<Bytes>>>,
    ) -> TxnScan {
        let buffered: Vec<(Vec<u8>, Option<Bytes>)> = own
            .unwrap_or(&self.writes)
            .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        TxnScan {
            store: self.db.store.scan(start, end, ts),
            store_head: None,
            buffered: buffered.into_iter().peekable(),
            store_done: false,
        }
    }

    /// The transaction's view as it is now (a PL/pgSQL cursor's OPEN), for reading later as
    /// it was: the data snapshot, and how far its own writes had got.
    pub fn snap(&self) -> Snap {
        self.snap_for(self.cid)
    }

    /// A snapshot taken now for command `cid` (a statement's: it sees what earlier commands
    /// wrote, and none of what it and later ones write).
    pub fn snap_for(&self, cid: u32) -> Snap {
        Snap {
            cid,
            st: RefCell::new(self.snap_state()),
        }
    }

    fn snap_state(&self) -> SnapState {
        SnapState {
            txn: self.id,
            ts: self.snapshot_ts,
            _pin: Some(self.db.store.pin(self.snapshot_ts)),
            mark: self.journal.len(),
            seen: self.journal.len(),
            rollbacks: self.rollbacks,
            prev: BTreeMap::new(),
        }
    }

    /// Bring `s` up to date with the writes made since it was taken.
    pub fn catch_up(&self, s: &Snap) {
        let mut s = s.st.borrow_mut();
        if s.txn != self.id {
            // Another transaction (a COMMIT in a procedure): nothing of ours to see past.
            *s = self.snap_state();
            return;
        }
        if s.rollbacks != self.rollbacks || self.journal.len() < s.seen {
            // Writes were undone: redo the record from the snapshot's mark.
            s.prev.clear();
            s.mark = s.mark.min(self.journal.len());
            s.seen = s.mark;
            s.rollbacks = self.rollbacks;
        }
        if s.seen == self.journal.len() {
            return;
        }
        let from = s.seen;
        for (k, prev, _) in &self.journal[from..] {
            s.prev.entry(k.clone()).or_insert_with(|| prev.clone());
        }
        s.seen = self.journal.len();
    }

    /// Scan [start, end) as of the snapshot `s`.
    pub fn scan_snap(&self, start: &[u8], end: &[u8], s: &Snap) -> TxnScan {
        self.catch_up(s);
        let st = s.st.borrow();
        if st.prev.is_empty() {
            return self.scan_at(start, end, st.ts);
        }
        let mut own = self.writes_in(start, end);
        for (k, prev) in st
            .prev
            .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
        {
            match prev {
                Some(v) => own.insert(k.clone(), v.clone()),
                None => own.remove(k),
            };
        }
        self.scan_overlay(start, end, st.ts, Some(&own))
    }

    /// Read `key` as of the snapshot `s`.
    pub fn get_snap(&self, key: &[u8], s: &Snap) -> Result<Option<Bytes>> {
        self.catch_up(s);
        let st = s.st.borrow();
        match st.prev.get(key) {
            Some(Some(v)) => Ok(v.clone()),
            Some(None) => self.db.store.get(key, st.ts),
            None => self.get_at(key, st.ts),
        }
    }

    /// Whether this transaction holds a lock on `key`.
    pub fn holds_lock(&self, key: &[u8]) -> bool {
        self.locks_held.contains(key)
    }

    /// Lock a row (or any key) until the transaction ends.
    pub fn lock(
        &mut self,
        key: &[u8],
        mode: LockMode,
        policy: WaitPolicy,
        what: &dyn Fn() -> String,
    ) -> Result<bool> {
        if self.locks_held.contains(key) {
            return Ok(true);
        }
        let got = self.db.locks.acquire(
            key,
            self.id,
            self.lock_peer,
            mode,
            policy,
            &self.cancel,
            self.lock_timeout,
            what,
        )?;
        if got {
            self.locks_held.insert(key.to_vec());
        }
        Ok(got)
    }

    /// Whether `key` changed after our snapshot (a concurrent committed write).
    pub fn changed_since_snapshot(&self, key: &[u8]) -> Result<bool> {
        if self.writes.contains_key(key) {
            return Ok(false);
        }
        Ok(self
            .db
            .store
            .latest_version_ts(key)?
            .is_some_and(|ts| ts > self.snapshot_ts))
    }

    /// The open savepoints (subtransactions).
    pub fn savepoint_depth(&self) -> usize {
        self.savepoints.len()
    }

    /// Whether a savepoint (subtransaction) is open.
    pub fn in_savepoint(&self) -> bool {
        !self.savepoints.is_empty()
    }

    pub fn savepoint(&mut self, name: &str) {
        self.savepoints.push(Savepoint {
            name: name.to_string(),
            journal_len: self.journal.len(),
            catalog: self.catalog.clone(),
            locks: self.locks_held.clone(),
            deferred_len: self.deferred.len(),
            deferred_triggers_len: self.deferred_triggers.len(),
            notifies_len: self.notifies.len(),
            modes: (self.constraint_modes.clone(), self.all_deferred),
            xid: 0,
            committed: Vec::new(),
            seq_undo_len: self.seq_undo.len(),
        });
    }

    fn find_savepoint(&self, name: &str) -> Result<usize> {
        self.savepoints
            .iter()
            .rposition(|s| s.name == name)
            .ok_or_else(|| {
                err!(
                    INVALID_SAVEPOINT_SPECIFICATION,
                    "savepoint \"{name}\" does not exist"
                )
            })
    }

    /// ROLLBACK TO SAVEPOINT: undo writes after it; the savepoint stays.
    pub fn rollback_to(&mut self, name: &str) -> Result<()> {
        let i = self.find_savepoint(name)?;
        let sp = &self.savepoints[i];
        let target = sp.journal_len;
        let catalog = sp.catalog.clone();
        let keep_locks = sp.locks.clone();
        self.deferred.truncate(sp.deferred_len);
        self.deferred_triggers.truncate(sp.deferred_triggers_len);
        self.notifies.truncate(sp.notifies_len);
        (self.constraint_modes, self.all_deferred) = sp.modes.clone();
        let seq_undo_len = sp.seq_undo_len;
        while self.seq_undo.len() > seq_undo_len {
            let (oid, st) = self.seq_undo.pop().unwrap();
            self.db.restore_seq(oid, st);
        }
        self.rollbacks += 1;
        while self.journal.len() > target {
            let (k, prev, prev_cid) = self.journal.pop().unwrap();
            match prev_cid {
                Some(c) => {
                    self.written.insert(k.clone(), c);
                }
                None => {
                    self.written.remove(&k);
                }
            }
            match prev {
                Some(v) => {
                    self.writes.insert(k, v);
                }
                None => {
                    self.writes.remove(&k);
                }
            }
        }
        self.catalog = catalog;
        if self.catalog.is_none() {
            self.catalog_base = None;
        }
        // Locks taken after the savepoint are released, as Postgres does for subtransactions.
        let release: Vec<Vec<u8>> = self.locks_held.difference(&keep_locks).cloned().collect();
        if !release.is_empty() {
            self.db.locks.release_all(self.id, release.clone());
            for k in release {
                self.locks_held.remove(&k);
            }
        }
        // The aborted subtransactions' statistics counts (levels i+2 and deeper).
        for nest in (i + 2..=self.savepoints.len() + 1).rev() {
            self.pgstat.end_subxact(nest, false);
        }
        // The subtransaction and those under it abort; the savepoint starts a new one.
        let xids = self.db.xids();
        let aborted: Vec<u64> = self.savepoints[i..]
            .iter_mut()
            .flat_map(|sp| std::mem::take(&mut sp.committed).into_iter().chain([sp.xid]))
            .filter(|&x| x != 0)
            .collect();
        for x in aborted {
            xids.finish(x, false);
        }
        self.savepoints[i].xid = 0;
        self.savepoints.truncate(i + 1);
        let renewed: Vec<u32> = self
            .new_storage
            .iter()
            .filter(|(_, d)| **d > i)
            .map(|(r, _)| *r)
            .collect();
        self.new_storage.retain(|_, d| *d <= i);
        for rel in renewed {
            crate::exec::sizing::storage_rolled_back(rel, &self.db);
        }
        // Large-object descriptors opened in the rolled back subtransaction close.
        for d in &mut self.lo_descs {
            if d.as_ref().is_some_and(|d| d.subid > i) {
                *d = None;
            }
        }
        Ok(())
    }

    pub fn release_savepoint(&mut self, name: &str) -> Result<()> {
        let i = self.find_savepoint(name)?;
        for nest in (i + 2..=self.savepoints.len() + 1).rev() {
            self.pgstat.end_subxact(nest, true);
        }
        // The released subtransactions' ids become their parent's to end with.
        let ended: Vec<u64> = self.savepoints[i..]
            .iter_mut()
            .flat_map(|sp| std::mem::take(&mut sp.committed).into_iter().chain([sp.xid]))
            .filter(|&x| x != 0)
            .collect();
        match i.checked_sub(1) {
            Some(p) => self.savepoints[p].committed.extend(ended),
            None => self.sub_committed.extend(ended),
        }
        self.savepoints.truncate(i);
        for d in self.lo_descs.iter_mut().flatten() {
            d.subid = d.subid.min(i);
        }
        for d in self.new_storage.values_mut() {
            *d = (*d).min(i);
        }
        Ok(())
    }

    /// Relation `rel` got new storage (a new relfilenode) in the current subtransaction.
    pub fn mark_new_storage(&mut self, rel: u32) {
        self.new_storage.insert(rel, self.savepoints.len());
    }

    /// Whether `rel` was created, or given new storage, in the current subtransaction
    /// (rd_createSubid / rd_newRelfilelocatorSubid), so TRUNCATE can empty it in place.
    pub fn storage_new_here(&self, rel: u32) -> bool {
        if self.new_storage.get(&rel) == Some(&self.savepoints.len()) {
            return true;
        }
        let base = match self.savepoints.last() {
            Some(sp) => sp.catalog.clone().unwrap_or_else(|| self.db.catalog()),
            None => self.db.catalog(),
        };
        !base.tables.contains_key(&rel) && !base.views.contains_key(&rel)
    }

    /// Whether `rel` was created, or given new storage, anywhere in this transaction
    /// (rd_createSubid or rd_firstRelfilelocatorSubid is set).
    pub fn storage_new_in_txn(&self, rel: u32) -> bool {
        if self.new_storage.contains_key(&rel) {
            return true;
        }
        let base = self.db.catalog();
        !base.tables.contains_key(&rel) && !base.views.contains_key(&rel)
    }

    /// Nesting depth of the current subtransaction (0: none).
    pub fn subtxn_depth(&self) -> usize {
        self.savepoints.len()
    }

    /// The newest committed version's timestamp of `key` (ignoring our own writes).
    pub fn latest_commit_ts(&self, key: &[u8]) -> Result<Option<u64>> {
        self.db.store.latest_version_ts(key)
    }

    /// Is a constraint with this (DEFERRABLE, INITIALLY DEFERRED) currently deferred?
    pub fn is_deferred(&self, constraint: u32, deferral: (bool, bool)) -> bool {
        deferral.0
            && self
                .constraint_modes
                .get(&constraint)
                .copied()
                .or(self.all_deferred)
                .unwrap_or(deferral.1)
    }

    pub fn commit(mut self) -> Result<u64> {
        let writes: Vec<(Vec<u8>, Option<Bytes>)> =
            std::mem::take(&mut self.writes).into_iter().collect();
        let catalog = self.catalog.take();
        let base = self.catalog_base;
        let start = self.start_ts;
        let strict = self.isolation.txn_snapshot();
        let keys: Vec<Vec<u8>> = if strict {
            writes.iter().map(|(k, _)| k.clone()).collect()
        } else {
            Vec::new()
        };
        let db = self.db.clone();
        let check = |store: &Store| -> Result<()> {
            if let Some(base) = base
                && db.catalog().version != base
            {
                return Err(Error::new(
                    code::SERIALIZATION_FAILURE,
                    "could not serialize access due to concurrent schema change",
                ));
            }
            if strict {
                for k in &keys {
                    if k.as_slice() == CATALOG_KEY {
                        continue;
                    }
                    if store.latest_version_ts(k)?.is_some_and(|ts| ts > start) {
                        return Err(Error::new(
                            code::SERIALIZATION_FAILURE,
                            "could not serialize access due to concurrent update",
                        ));
                    }
                }
            }
            Ok(())
        };
        let result = if writes.is_empty() {
            Ok(self.db.store.visible_ts())
        } else {
            let db2 = self.db.clone();
            let publish = self.publish;
            self.db.store.commit_with(writes, check, move |_| {
                if let Some(c) = catalog {
                    if !publish {
                        db2.last_local.store(c.version, Ordering::Release);
                    }
                    *db2.catalog.write() = c;
                }
            })
        };
        self.finish_xid(result.is_ok());
        self.release_locks();
        result
    }

    pub fn rollback(mut self) {
        self.finish_xid(false);
        self.release_locks();
    }

    fn release_locks(&mut self) {
        if !self.locks_held.is_empty() {
            let keys: Vec<Vec<u8>> = self.locks_held.drain().collect();
            self.db.locks.release_all(self.id, keys);
        }
    }

    pub fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::new(
                code::QUERY_CANCELED,
                "canceling statement due to user request",
            ));
        }
        Ok(())
    }
}

impl Drop for Txn {
    fn drop(&mut self) {
        self.finish_xid(false);
        self.release_locks();
    }
}

/// A transaction's view at one moment (`Txn::snap`): what its later writes replaced, so
/// reads see past them.
pub struct Snap {
    /// The command it was taken for (the snapshot's curcid).
    pub cid: u32,
    st: RefCell<SnapState>,
}

struct SnapState {
    txn: u64,
    ts: u64,
    _pin: Option<SnapshotPin>,
    /// Journal length when taken, and how much of the journal `prev` covers.
    mark: usize,
    seen: usize,
    rollbacks: u64,
    /// Keys written since, with what the write buffer held for them then (None: nothing).
    prev: BTreeMap<Vec<u8>, Option<Option<Bytes>>>,
}

/// Merge of buffered writes over a store scan; buffered entries win.
pub struct TxnScan {
    store: crate::storage::Scan,
    store_head: Option<(Vec<u8>, Bytes)>,
    buffered: std::iter::Peekable<std::vec::IntoIter<(Vec<u8>, Option<Bytes>)>>,
    store_done: bool,
}

impl Iterator for TxnScan {
    type Item = Result<(Vec<u8>, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.store_head.is_none() && !self.store_done {
                match self.store.next() {
                    Some(Ok(e)) => self.store_head = Some(e),
                    Some(Err(e)) => return Some(Err(e)),
                    None => self.store_done = true,
                }
            }
            match (self.store_head.as_ref(), self.buffered.peek()) {
                (None, None) => return None,
                (Some(_), None) => return self.store_head.take().map(Ok),
                (None, Some(_)) => {
                    let (k, v) = self.buffered.next().unwrap();
                    match v {
                        Some(v) => return Some(Ok((k, v))),
                        None => continue,
                    }
                }
                (Some((sk, _)), Some((bk, _))) => {
                    if sk < bk {
                        return self.store_head.take().map(Ok);
                    }
                    if sk == bk {
                        self.store_head = None;
                    }
                    let (k, v) = self.buffered.next().unwrap();
                    match v {
                        Some(v) => return Some(Ok((k, v))),
                        None => continue,
                    }
                }
            }
        }
    }
}
