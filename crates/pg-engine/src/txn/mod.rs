//! Databases and transactions: snapshots over the MVCC store, buffered writes, row locks,
//! savepoints, sequences, and transactional catalog changes.

pub(crate) mod catmerge;
pub mod locks;
pub mod ssi;
pub mod xids;

use crate::catalog::{Catalog, Sequence, persist};
use crate::error::{Error, Result, code};
use crate::storage::{SnapshotPin, Store};
use crate::{bail, err};
use bytes::Bytes;
use locks::{LockManager, LockMode, RowLockMode, WaitPolicy, Waits};
use parking_lot::{Mutex, RwLock};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Reserved keyspace (table oid 0): the catalog's header record (its objects' records are
/// under `persist::RECORD_PREFIX`).
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
    /// Its serializable transactions' predicate locks and rw-conflicts.
    pub ssi: ssi::Ssi,
    /// Rows an UPDATE moved to another partition, by data key, with the commit time of the
    /// move (the deletion that left the old partition): what a transaction waiting for one
    /// learns (Postgres's moved-partitions ctid).
    pub moved: Mutex<HashMap<Vec<u8>, u64>>,
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
    /// System catalogs' rows generated for its sessions' statements, for the next ones that
    /// read them under the same catalog, roles, path and settings (`catalog::system::rows`).
    pub sys_rows: Mutex<crate::catalog::system::SysRowsCache>,
    /// When DETACH PARTITION ... CONCURRENTLY's first transaction committed, by partition
    /// (a marked partition missing here was marked before this process opened the
    /// database: every snapshot sees it). `u64::MAX` while that commit is under way.
    pub detach_marks: Mutex<HashMap<u32, u64>>,
    /// Catalog views leaving out partitions pending detach (`Catalog::detach_view`) made for
    /// the current catalog: (its version, what they leave out, routing_full, the view).
    detach_views: Mutex<Vec<DetachViewEntry>>,
}

type DetachViewEntry = (u64, Vec<(u32, u32)>, bool, Arc<Catalog>);

impl Database {
    pub fn open(name: &str, oid: u32, store: Arc<Store>, owner: &str) -> Result<Arc<Database>> {
        let ts = store.visible_ts();
        let catalog = match store.get(CATALOG_KEY, ts)? {
            Some(b) => {
                let (from, to) = persist::record_range();
                let stored = persist::load(&b, || {
                    store.scan(from, to, ts).collect::<Result<Vec<_>>>()
                })?;
                let mut c = stored.clone();
                c.reindex();
                if c.catalog_format < persist::CATALOG_FORMAT {
                    // Stored as one value (before catalog format 1): rewritten as records,
                    // in one commit.
                    c.catalog_format = persist::CATALOG_FORMAT;
                    store.commit(persist::writes_new(&c)?, |_| Ok(()))?;
                } else {
                    // What `reindex` brought up to date (oids a catalog from before them
                    // lacked) is stored now: later commits write only what they change.
                    let w = persist::changed(&stored, &c)?;
                    if !w.is_empty() {
                        store.commit(w, |_| Ok(()))?;
                    }
                }
                c
            }
            None => {
                let c = Catalog::bootstrap(owner);
                store.commit(persist::writes_new(&c)?, |_| Ok(()))?;
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
        catalog.freeze();
        let version = catalog.version;
        static INSTANCES: AtomicU64 = AtomicU64::new(1);
        Ok(Arc::new(Database {
            name: name.to_string(),
            oid,
            instance: INSTANCES.fetch_add(1, Ordering::Relaxed),
            store,
            catalog: RwLock::new(Arc::new(catalog)),
            locks: LockManager::default(),
            ssi: Default::default(),
            moved: Mutex::new(HashMap::new()),
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
            sys_rows: Mutex::new(Default::default()),
            detach_marks: Mutex::new(HashMap::new()),
            detach_views: Mutex::new(Vec::new()),
        }))
    }

    /// `base` without the partitions pending detach whose marks a snapshot taken at
    /// `snapshot_ts` sees. `routing_full`: insert routing keeps them all (a snapshot
    /// transaction's).
    fn detach_view(&self, base: Arc<Catalog>, snapshot_ts: u64, routing_full: bool) -> Arc<Catalog> {
        let omitted: Vec<(u32, u32)> = {
            let marks = self.detach_marks.lock();
            base.detach_pending()
                .into_iter()
                .filter(|(_, p)| marks.get(p).is_none_or(|ts| *ts <= snapshot_ts))
                .collect()
        };
        if omitted.is_empty() {
            return base;
        }
        let mut views = self.detach_views.lock();
        views.retain(|v| v.0 == base.version);
        if let Some(v) = views.iter().find(|v| v.1 == omitted && v.2 == routing_full) {
            return v.3.clone();
        }
        let mut c = base.detach_view(omitted.clone(), routing_full);
        c.freeze();
        let c = Arc::new(c);
        views.push((base.version, omitted, routing_full, c.clone()));
        c
    }

    /// Allocate oids from the cluster's counter.
    pub fn set_oids(&self, g: Arc<crate::catalog::shared::OidGen>) {
        let mut w = self.catalog.write();
        let mut c = (**w).clone();
        c.oids = Some(g);
        c.freeze();
        *w = Arc::new(c);
    }

    /// The database's locale (its pg_database row's), which its catalogs carry from now on.
    pub fn set_locale(&self, l: Arc<crate::types::collation::DbDefault>) {
        let mut w = self.catalog.write();
        let mut c = (**w).clone();
        c.db_locale = Some(l);
        c.freeze();
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
        let pin = self.store.pin_visible();
        let ts = pin.ts;
        Txn {
            id,
            xid: 0,
            db: self.clone(),
            isolation,
            read_only,
            snapshot_ts: ts,
            start_ts: ts,
            snapshot_fixed: false,
            stmt_snapshot: false,
            _pin: Some(pin),
            xmin: None,
            writes: BTreeMap::new(),
            journal: Vec::new(),
            rollbacks: 0,
            cid: 0,
            write_cid: 0,
            pg_cid: 0,
            pg_cid_used: false,
            combos: Vec::new(),
            sub_committed: Vec::new(),
            catalog: None,
            catalog_base: None,
            catalog_base_cat: None,
            stmt_catalog: None,
            locks_held: HashMap::new(),
            lock_journal: Vec::new(),
            row_locks: Vec::new(),
            xact_locks: Vec::new(),
            spec_token: 0,
            savepoints: Vec::new(),
            cancel,
            lock_timeout: None,
            deadlock_timeout: Duration::from_secs(1),
            deadline: None,
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
            si_locks: Vec::new(),
            sxact: false,
            deferrable: false,
            moved_out: Vec::new(),
            seq_undo: Vec::new(),
            pgstat: Default::default(),
            db_owner_undo: Vec::new(),
            db_options_undo: Vec::new(),
            exported: false,
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

    /// A clean shutdown (`Engine::shutdown`, closing an idle database): the sequences'
    /// positions as they are, not as far as values were logged ahead, as Postgres's
    /// shutdown checkpoint writes their pages (only a crash skips values).
    pub fn persist_sequences(&self) -> Result<()> {
        let cat = self.catalog();
        let seqs = self.seqs.lock();
        for (oid, st) in seqs.iter() {
            if st.logged_to != st.last && cat.sequences.contains_key(oid) {
                self.log_seq(*oid, st.last, st.is_called)?;
            }
        }
        Ok(())
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
    /// Heavyweight locks taken before it (`Txn::lock_journal`).
    lock_journal_len: usize,
    /// The subtransaction's lock id (its row locks' holder, its transaction lock).
    lock_id: u64,
    /// Lock ids of subtransactions released into this one.
    lock_ids: Vec<u64>,
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
    /// The columns the UPDATE that queued it assigned (tg_updatedcols).
    pub set_cols: Vec<usize>,
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
    /// A transaction snapshot (REPEATABLE READ / SERIALIZABLE) was taken.
    snapshot_fixed: bool,
    /// The running statement has a snapshot (Postgres's ActiveSnapshotSet): queries then
    /// leave out partitions whose pending detach it sees.
    stmt_snapshot: bool,
    _pin: Option<SnapshotPin>,
    /// The xmin of the snapshot held (the statement's in READ COMMITTED, the
    /// transaction's), as pruning and VACUUM see it held back.
    xmin: Option<xids::SnapshotXmin>,
    /// Buffered writes: the value (None: a delete) and the command id of the latest write of
    /// the key (None: one that records none, `replace_catalog`'s).
    writes: BTreeMap<Vec<u8>, (Option<Bytes>, Option<u32>)>,
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
    /// That catalog (what a commit after other DDL merges from: `catmerge`).
    catalog_base_cat: Option<Arc<Catalog>>,
    /// The database's catalog as the running statement began: no newer than any catalog the
    /// statement's DDL is made from, so it can stand as that DDL's base (a newer one would
    /// take what other DDL committed meanwhile for this DDL's removals).
    stmt_catalog: Option<Arc<Catalog>>,
    /// Heavyweight locks held, by key and mode, with the subtransaction depth that took them.
    locks_held: HashMap<(Vec<u8>, LockMode), usize>,
    /// The order they were taken in (released by ROLLBACK TO in reverse).
    lock_journal: Vec<(Vec<u8>, LockMode)>,
    /// Rows locked (or written) here, with the depth that locked them.
    row_locks: Vec<(Vec<u8>, usize)>,
    /// Lock ids of the (sub)transactions here holding their transaction lock.
    xact_locks: Vec<u64>,
    /// The last speculative insertion token handed out (`begin_speculative`).
    spec_token: u32,
    savepoints: Vec<Savepoint>,
    pub cancel: Arc<AtomicBool>,
    pub lock_timeout: Option<Duration>,
    /// deadlock_timeout: how long a lock wait lasts before checking for a deadlock.
    pub deadlock_timeout: Duration,
    /// When the running statement times out (statement_timeout).
    pub deadline: Option<Instant>,
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

    /// SSI predicate (SIRead) locks a serializable transaction's reads took, as pg_locks
    /// shows them: (relation, page, tuple); page and tuple None for a relation lock.
    pub si_locks: Vec<(u32, Option<u32>, Option<u16>)>,
    /// SSI tracks it (`ssi::Ssi::register`, when its snapshot is taken).
    sxact: bool,
    /// SERIALIZABLE READ ONLY DEFERRABLE: its snapshot waits to be safe.
    pub deferrable: bool,
    /// Rows it moved out of their partition (`Database::moved` once it commits).
    pub moved_out: Vec<Vec<u8>>,
    /// Sequences given a new file here (TRUNCATE ... RESTART IDENTITY), with the state of
    /// the old one, which a rollback makes current again.
    pub seq_undo: Vec<(u32, (i64, bool, i64, i64))>,
    /// The transactional statistics counts (`exec::pgstat`).
    pub pgstat: crate::exec::pgstat::XactStats,
    /// Databases whose owner this transaction changed: (name, the previous owner), put
    /// back when it rolls back.
    pub db_owner_undo: Vec<(String, String)>,
    /// Databases whose options (datistemplate, datallowconn, datconnlimit) this transaction
    /// changed: (name, the previous options), put back when it rolls back.
    pub db_options_undo: Vec<(String, crate::session::state::DbOptions)>,
    /// It exported snapshots (pg_export_snapshot), which go when it ends.
    exported: bool,
}

impl Txn {
    /// Whether this transaction's reads take SIRead predicate locks: a serializable one SSI
    /// tracks (not a read-only one on a safe snapshot: `ssi::Ssi::register`).
    pub fn takes_predicate_locks(&self) -> bool {
        self.sxact && self.db.ssi.needed_for_read(self.id)
    }

    /// Whether SSI tracks this transaction (its writes are checked for conflicts).
    pub fn serializable_tracked(&self) -> bool {
        self.sxact
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
            self.db.ssi.set_locks(self.id, &self.si_locks);
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
        Self::coarsen_relation(locks, rel);
        self.db.ssi.set_locks(self.id, &self.si_locks);
    }

    /// PredicateLockPage: an SIRead lock on page `page` of `rel` (an index page a scan
    /// read), covering its tuple locks.
    pub fn predicate_lock_page(&mut self, rel: u32, page: u32) {
        let locks = &mut self.si_locks;
        if locks.iter().any(|l| l.0 == rel && (l.1.is_none() || *l == (rel, Some(page), None))) {
            return;
        }
        locks.retain(|l| !(l.0 == rel && l.1 == Some(page)));
        locks.push((rel, Some(page), None));
        Self::coarsen_relation(locks, rel);
        self.db.ssi.set_locks(self.id, &self.si_locks);
    }

    fn coarsen_relation(locks: &mut Vec<(u32, Option<u32>, Option<u16>)>, rel: u32) {
        if locks.iter().filter(|l| l.0 == rel).count() > 32 {
            locks.retain(|l| l.0 != rel);
            locks.push((rel, None, None));
        }
    }

    /// CheckForSerializableConflictIn for a write of row `key` of `rel` (None: a new row,
    /// checked at the relation; Some((page, tuple)): the version it replaces) and of the
    /// index pages its entries go to (index, page: None when not modelled).
    pub fn ssi_write(
        &mut self,
        rel: u32,
        key: &[u8],
        old: Option<(u32, u16)>,
        index_pages: &[(u32, Option<u32>)],
    ) -> Result<()> {
        self.ssi_write_version(rel, key, old, index_pages, None)
    }

    /// `ssi_write`, for a row (of a table without modelled tuple ids) whose replaced
    /// version committed at `version`.
    pub fn ssi_write_version(
        &mut self,
        rel: u32,
        key: &[u8],
        old: Option<(u32, u16)>,
        index_pages: &[(u32, Option<u32>)],
        version: Option<u64>,
    ) -> Result<()> {
        if !self.sxact {
            return Ok(());
        }
        let mut targets = Vec::new();
        if let Some((p, t)) = old {
            targets.push((rel, Some(p), Some(t)));
            targets.push((rel, Some(p), None));
        }
        targets.push((rel, None, None));
        for (ix, page) in index_pages {
            if let Some(p) = page {
                targets.push((*ix, Some(*p), None));
            }
            targets.push((*ix, None, None));
        }
        self.db.ssi.write(self.id, Some((rel, key)), &targets, version)?;
        // (Its own tuple lock on a row it now changes isn't needed any more.)
        if let Some((p, t)) = old
            && self.savepoints.is_empty()
            && let Some(i) = self.si_locks.iter().position(|l| *l == (rel, Some(p), Some(t)))
        {
            self.si_locks.remove(i);
            self.db.ssi.set_locks(self.id, &self.si_locks);
        }
        Ok(())
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
    pub fn begin_statement(&mut self, needs_snapshot: bool) -> Result<()> {
        if self.catalog.is_none() {
            self.stmt_catalog = Some(self.db.catalog());
        }
        self.stmt_snapshot = needs_snapshot;
        // A transaction snapshot is taken by the first statement that needs one.
        if self.isolation.txn_snapshot() {
            if needs_snapshot && !self.snapshot_fixed {
                self.snapshot_fixed = true;
                self.take_snapshot();
                if self.isolation == Isolation::Serializable {
                    self.register_serializable()?;
                }
            }
        } else {
            if self.db.store.visible_ts() != self.snapshot_ts {
                let pin = self.db.store.pin_visible();
                self.snapshot_ts = pin.ts;
                self._pin = Some(pin);
            }
            // The statement's snapshot holds back pruning while it runs (and, here, until
            // the next statement or the transaction's end).
            self.xmin = needs_snapshot.then(|| self.db.xids().register_snapshot_of(Some(self.id)));
            if needs_snapshot {
                self.hold_own_xact_lock();
            }
        }
        Ok(())
    }

    /// READ COMMITTED: a fresh snapshot for the rest of the statement, as the queries SPI
    /// runs for a utility command take (RI_PartitionRemove_Check after waiting for its
    /// lock on the referencing table sees what the transaction it waited for committed).
    pub fn refresh_read_committed_snapshot(&mut self) {
        if !self.isolation.txn_snapshot() && self.db.store.visible_ts() != self.snapshot_ts {
            let pin = self.db.store.pin_visible();
            self.snapshot_ts = pin.ts;
            self._pin = Some(pin);
        }
    }

    /// Its own (virtual) transaction lock, which others waiting for it wait on: held by a
    /// transaction from its first lock or snapshot on.
    fn hold_own_xact_lock(&mut self) {
        if !self.xact_locks.contains(&self.id) {
            self.db.locks.hold_xact_lock(self.id, self.id, self.lock_proc(), self.xid);
            self.xact_locks.push(self.id);
        }
    }

    /// (Pinned atomically with the timestamp it reads: `Store::pin_visible`.)
    fn take_snapshot(&mut self) {
        self.xmin = Some(self.db.xids().register_snapshot_of(Some(self.id)));
        self.hold_own_xact_lock();
        if self.db.store.visible_ts() != self.snapshot_ts && !self.wrote {
            let pin = self.db.store.pin_visible();
            self.snapshot_ts = pin.ts;
            self.start_ts = pin.ts;
            self._pin = Some(pin);
        }
    }

    /// GetSerializableTransactionSnapshot: SSI tracks it from its snapshot on (a DEFERRABLE
    /// read-only one waits for a safe snapshot first: GetSafeSnapshot).
    fn register_serializable(&mut self) -> Result<()> {
        let pid = locks::proc_of(self.id, self.lock_peer) as u32 as i32;
        loop {
            self.sxact = self.db.ssi.register(self.id, pid, self.read_only, self.snapshot_ts);
            if !(self.sxact && self.read_only && self.deferrable) {
                return Ok(());
            }
            crate::exec::flush_live_notices();
            let safe = self.db.ssi.wait_safe(self.id, &self.cancel)?;
            self.sxact = false;
            if safe {
                return Ok(());
            }
            self.take_snapshot();
        }
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        match &self.catalog {
            Some(c) => c.clone(),
            None => {
                let base = self.db.catalog();
                if !self.stmt_snapshot || !base.has_detach_pending() {
                    return base;
                }
                self.db.detach_view(base, self.snapshot_ts, self.isolation.txn_snapshot())
            }
        }
    }

    /// The catalog as a fresh snapshot sees it: what a REPEATABLE READ foreign key check
    /// against a partitioned table plans with (ri_PerformCheck's latest snapshot, under
    /// which a partition whose detach is pending is left out).
    pub fn catalog_latest(&self) -> Arc<Catalog> {
        match &self.catalog {
            Some(c) => c.clone(),
            None => {
                let base = self.db.catalog();
                if !base.has_detach_pending() {
                    return base;
                }
                self.db.detach_view(base, u64::MAX - 1, false)
            }
        }
    }

    pub fn has_writes(&self) -> bool {
        self.wrote
    }

    /// Replace the catalog (DDL), made from `from` (the catalog the command started from):
    /// a first change records it as the base a commit merges from when other DDL committed
    /// meanwhile (which may have happened while the command ran).
    pub fn set_catalog_from(&mut self, c: Catalog, from: &Arc<Catalog>) -> Result<()> {
        let from = &from.with_detached();
        if self.catalog_base.is_none() && self.catalog.is_none() {
            self.catalog_base = Some(from.version);
            self.catalog_base_cat = Some(from.clone());
        }
        self.set_catalog(c)
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
            let base = self.stmt_catalog.take().unwrap_or_else(|| self.db.catalog());
            self.catalog_base = Some(base.version);
            self.catalog_base_cat = Some(base);
        }
        c.unview();
        c.bump();
        // (A stand-in: the commit writes the records of what changed, `catalog_writes`.)
        self.buffer(CATALOG_KEY.to_vec(), Some(Bytes::new()));
        c.freeze();
        self.catalog = Some(Arc::new(c));
        Ok(())
    }

    /// Replace the catalog by in-place updates (plain VACUUM's statistics), which give the
    /// transaction no id.
    pub fn set_catalog_inplace(&mut self, mut c: Catalog, from: &Arc<Catalog>) -> Result<()> {
        let from = &from.with_detached();
        c.unview();
        if self.catalog_base.is_none() && self.catalog.is_none() {
            self.catalog_base = Some(from.version);
            self.catalog_base_cat = Some(from.clone());
        }
        if self.read_only {
            bail!(
                READ_ONLY_SQL_TRANSACTION,
                "cannot execute DDL in a read-only transaction"
            );
        }
        if self.catalog_base.is_none() {
            let base = self.stmt_catalog.take().unwrap_or_else(|| self.db.catalog());
            self.catalog_base = Some(base.version);
            self.catalog_base_cat = Some(base);
        }
        c.bump();
        self.record(CATALOG_KEY.to_vec(), Some(Bytes::new()));
        self.wrote = true;
        c.freeze();
        self.catalog = Some(Arc::new(c));
        Ok(())
    }

    /// The catalog this transaction's DDL started from (None: it made none).
    pub fn catalog_base(&self) -> Option<Arc<Catalog>> {
        self.catalog_base_cat.clone()
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
            let cur_cat = self.db.catalog();
            let cur = cur_cat.version;
            self.catalog_base = Some(cur);
            self.catalog_base_cat = Some(cur_cat);
            // (A new version, unique as `bump` makes them: `cur + 1` may be one another
            // transaction's catalog already has, which caches keyed by the version take for
            // this one.)
            c.version = cur;
            c.bump();
        }
        c.unview();
        let cid = self.writes.get(CATALOG_KEY).and_then(|w| w.1);
        self.writes.insert(CATALOG_KEY.to_vec(), (Some(Bytes::new()), cid));
        c.freeze();
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
        let (prev, prev_cid) = match self.writes.insert(key.clone(), (value, Some(self.write_cid))) {
            Some((v, c)) => (Some(v), c),
            None => (None, None),
        };
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
        self.writes.get(key).and_then(|w| w.1)
    }

    /// This transaction's id, given it now if it has none (as writing or
    /// pg_current_xact_id() does).
    pub fn xid(&mut self) -> u64 {
        if self.xid == 0 {
            self.xid = self.db.xids().assign();
            // Its transaction lock, held until it ends (what waiting for it waits on).
            self.db.locks.hold_xact_lock(self.id, self.id, self.lock_proc(), self.xid);
            if self.sxact {
                self.db.ssi.set_xid(self.id, self.xid);
            }
            if !self.xact_locks.contains(&self.id) {
                self.xact_locks.push(self.id);
            }
        }
        self.xid
    }

    /// The id of the running (sub)transaction, which what it writes is stamped with
    /// (GetCurrentTransactionId): a subtransaction gets one of its own when it first
    /// writes, its parents (and the top level) theirs first.
    pub fn cur_xid(&mut self) -> u64 {
        let top = self.xid();
        // (Every level has its id already: the usual case, every write of a statement,
        // without touching the shared id registry.)
        if self.savepoints.iter().all(|sp| sp.xid != 0) {
            return self.savepoints.last().map_or(top, |sp| sp.xid);
        }
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

    /// `put` of an entry of an index this statement is building, which nothing can need to
    /// see past: no snapshot taken before could read an index that didn't exist, and with no
    /// savepoint open nothing can roll back to before it (a rollback of the whole
    /// transaction drops the buffer). So, unless a savepoint is open or the key was written
    /// already, it goes in without an undo record (an index build's entries otherwise doubled
    /// in memory by the journal).
    pub fn put_new_index_entry(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        if self.read_only
            || !self.savepoints.is_empty()
            || self.writes.contains_key(key.as_slice())
        {
            return self.put(key, value);
        }
        self.writes
            .insert(key, (Some(Bytes::from(value)), Some(self.write_cid)));
        self.wrote = true;
        self.cur_xid();
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
        if let Some((v, _)) = self.writes.get(key) {
            return Ok(v.clone());
        }
        self.db.store.get(key, self.snapshot_ts)
    }

    /// Newest committed version (ignoring our snapshot), for locking reads after a wait.
    pub fn get_latest(&self, key: &[u8]) -> Result<Option<(u64, Option<Bytes>)>> {
        if let Some((v, _)) = self.writes.get(key) {
            return Ok(Some((u64::MAX, v.clone())));
        }
        self.db.store.get_version(key, u64::MAX >> 1)
    }

    /// Read through this transaction's own writes over the data committed as of `ts`.
    pub fn get_at(&self, key: &[u8], ts: u64) -> Result<Option<Bytes>> {
        if let Some((v, _)) = self.writes.get(key) {
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
            .map(|(k, (v, _))| (k.clone(), v.clone()))
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
        let range = (Bound::Included(start), Bound::Excluded(end));
        let buffered: Vec<(Vec<u8>, Option<Bytes>)> = match own {
            Some(own) => own
                .range::<[u8], _>(range)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            None => self
                .writes
                .range::<[u8], _>(range)
                .map(|(k, (v, _))| (k.clone(), v.clone()))
                .collect(),
        };
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

    /// Scan [start, end) in descending key order, as of the snapshot `s` (None: the
    /// transaction's current one, with all its writes).
    pub fn scan_rev(&self, start: &[u8], end: &[u8], s: Option<&Snap>) -> TxnRevScan {
        let (ts, own) = match s {
            Some(s) => {
                self.catch_up(s);
                let st = s.st.borrow();
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
                (st.ts, own)
            }
            None => (self.snapshot_ts, self.writes_in(start, end)),
        };
        TxnRevScan {
            store: self.db.store.scan_rev(start, end, ts),
            store_head: None,
            buffered: own.into_iter().rev().collect::<Vec<_>>().into_iter().peekable(),
            store_done: false,
        }
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

    /// `get_snap` of each of `keys` (any order), the store's reads made together
    /// (`Store::get_versions`).
    pub fn get_many_snap(&self, keys: &[Vec<u8>], s: &Snap) -> Result<Vec<Option<Bytes>>> {
        self.catch_up(s);
        let st = s.st.borrow();
        let mut out: Vec<Option<Bytes>> = vec![None; keys.len()];
        // The keys the store answers: (key, its place in `keys`).
        let mut reads: Vec<(&[u8], usize)> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            match st.prev.get(key.as_slice()) {
                Some(Some(v)) => out[i] = v.clone(),
                Some(None) => reads.push((key, i)),
                None => match self.writes.get(key.as_slice()) {
                    Some((v, _)) => out[i] = v.clone(),
                    None => reads.push((key, i)),
                },
            }
        }
        if reads.is_empty() {
            return Ok(out);
        }
        reads.sort_unstable();
        let mut uniq: Vec<&[u8]> = reads.iter().map(|r| r.0).collect();
        uniq.dedup();
        let got = self.db.store.get_versions(&uniq, st.ts)?;
        let mut u = 0;
        for (key, i) in reads {
            while uniq[u] != key {
                u += 1;
            }
            out[i] = got[u].as_ref().and_then(|(_, v)| v.clone());
        }
        Ok(out)
    }

    /// Whether this transaction holds heavyweight lock `key` (in any mode).
    pub fn holds_lock(&self, key: &[u8]) -> bool {
        self.locks_held.keys().any(|(k, _)| k == key)
    }

    /// Whether this transaction holds heavyweight lock `key` in `mode`.
    pub fn holds_lock_mode(&self, key: &[u8], mode: LockMode) -> bool {
        self.locks_held.contains_key(&(key.to_vec(), mode))
    }

    /// The backend this transaction's locks belong to.
    pub fn lock_proc(&self) -> u64 {
        locks::proc_of(self.id, self.lock_peer)
    }

    /// The waits a lock request of this transaction allows.
    pub fn waits(&self, policy: WaitPolicy) -> Waits<'_> {
        Waits {
            policy,
            cancel: &self.cancel,
            lock_timeout: self.lock_timeout,
            deadline: self.deadline,
            deadlock_timeout: self.deadlock_timeout,
        }
    }

    /// Take heavyweight lock `key` in `mode` until the transaction ends (or the
    /// subtransaction taking it aborts). Ok(false) only for SKIP LOCKED.
    pub fn lock(
        &mut self,
        key: &[u8],
        mode: LockMode,
        policy: WaitPolicy,
        what: &dyn Fn() -> String,
    ) -> Result<bool> {
        let k = (key.to_vec(), mode);
        if self.locks_held.contains_key(&k) {
            return Ok(true);
        }
        let got = self.db.locks.acquire(
            key,
            self.id,
            self.lock_proc(),
            mode,
            &self.waits(policy),
            what,
        )?;
        if got {
            self.lock_journal.push(k.clone());
            self.locks_held.insert(k, self.savepoints.len());
            // Its own lock (the virtual transaction id's), which waiting for it waits on.
            if !self.xact_locks.contains(&self.id) {
                self.db.locks.hold_xact_lock(self.id, self.id, self.lock_proc(), self.xid);
                self.xact_locks.push(self.id);
            }
        }
        Ok(got)
    }

    /// WaitForLockers: wait until every transaction holding relation `rel`'s lock in a mode
    /// conflicting with `mode` has ended (the CONCURRENTLY commands' phases).
    pub fn wait_for_lockers(&mut self, rel: u32, mode: LockMode) -> Result<()> {
        let key = locks::relation_key(self.db.oid, rel);
        let proc = self.lock_proc();
        for owner in self.db.locks.conflicting_owners(&key, mode, proc) {
            let k = locks::xact_key(owner);
            self.db.locks.acquire(&k, self.id, proc, LockMode::Share, &self.waits(WaitPolicy::Block), &String::new)?;
            self.db.locks.release(&k, self.id, LockMode::Share);
        }
        Ok(())
    }

    /// WaitForOlderSnapshots: wait for every other transaction holding a snapshot as old as
    /// one taken now (a concurrent index build's last phase), each until it ends, unless it
    /// let go of every such snapshot meanwhile. While it waits, this transaction holds no
    /// snapshot others would wait for (Postgres's waiting backend has xmin 0).
    pub fn wait_for_older_snapshots(&mut self) -> Result<()> {
        let xids = self.db.xids();
        let limit = xids.current_xmin();
        let had = self.xmin.take().is_some();
        let mut r = Ok(());
        for owner in xids.older_snapshot_owners(limit, self.id) {
            if !xids.holds_snapshot_older(owner, limit) {
                continue;
            }
            r = self.wait_for_lock(&locks::xact_key(owner));
            if r.is_err() {
                break;
            }
        }
        if had {
            self.xmin = Some(xids.register_snapshot_of(Some(self.id)));
        }
        r
    }

    /// Whether this transaction builds an index on plain columns concurrently
    /// (PROC_IN_SAFE_IC: others' WaitForOlderSnapshots skip it).
    pub fn set_safe_index_build(&self, on: bool) {
        self.db.xids().set_safe_ic(self.id, on);
    }

    /// Where the heavyweight locks taken so far end (`release_locks_since`).
    pub fn lock_mark(&self) -> usize {
        self.lock_journal.len()
    }

    /// Release the heavyweight locks taken since `mark` (a utility statement's own
    /// transaction for each relation it processes, as VACUUM / ANALYZE commit after each).
    pub fn release_locks_since(&mut self, mark: usize) {
        while self.lock_journal.len() > mark {
            let k = self.lock_journal.pop().unwrap();
            self.locks_held.remove(&k);
            self.db.locks.release(&k.0, self.id, k.1);
        }
    }

    /// Lock the row of object `oid` in system catalog `class` in `mode` until the
    /// transaction ends (heap_lock_tuple, or heap_update / heap_delete of the row when
    /// `modifies`: others' in-place updates of it then wait for us).
    pub fn lock_catalog_row(&mut self, class: u32, oid: u32, mode: RowLockMode, modifies: bool) -> Result<()> {
        let key = locks::catalog_row_key(class, oid);
        let name = catalog_name(class);
        self.lock_row(class, &key, mode, WaitPolicy::Block, &|| name.to_string())?;
        if modifies {
            self.row_modified(&key);
        }
        Ok(())
    }

    /// heap_inplace_lock of the row of object `oid` in system catalog `class` (an in-place
    /// update: VACUUM's and index builds' statistics, datfrozenxid): its tuple lock, held
    /// while waiting for another transaction updating the row or locking it more strongly
    /// than FOR KEY SHARE, which is waited for over and over until none is left.
    pub fn inplace_lock_catalog_row(&mut self, class: u32, oid: u32) -> Result<()> {
        let key = locks::catalog_row_key(class, oid);
        let tuple = locks::tuple_key(self.db.oid, class, &key);
        let proc = self.lock_proc();
        loop {
            let name = catalog_name(class);
            self.db.locks.acquire(
                &tuple,
                self.id,
                proc,
                LockMode::Exclusive,
                &self.waits(WaitPolicy::Block),
                &|| format!("row in relation \"{name}\""),
            )?;
            let Some(&holder) = self.db.locks.row_conflicts(&key, self.id, RowLockMode::NoKeyUpdate).first() else {
                self.db.locks.release(&tuple, self.id, LockMode::Exclusive);
                return Ok(());
            };
            let r = self.wait_for_xact(holder);
            self.db.locks.release(&tuple, self.id, LockMode::Exclusive);
            r?;
        }
    }

    /// Release one heavyweight lock this transaction took (`lock`) before it ends.
    pub fn unlock(&mut self, key: &[u8], mode: LockMode) {
        let k = (key.to_vec(), mode);
        if self.locks_held.remove(&k).is_none() {
            return;
        }
        if let Some(i) = self.lock_journal.iter().rposition(|j| *j == k) {
            self.lock_journal.remove(i);
        }
        self.db.locks.release(key, self.id, mode);
    }

    /// Unique key lock `key` (just taken) now stands for this (sub)transaction's index
    /// entry for `row`: others' checks of the key wait for the running (sub)transaction,
    /// or for speculative insertion `token` while it is unfinished.
    pub fn hold_unique_key(
        &mut self,
        key: &[u8],
        token: Option<u32>,
        row: Option<Arc<Vec<crate::types::Value>>>,
    ) {
        let holder = self.holder();
        let xid = self.cur_xid();
        if holder != self.id {
            self.db.locks.hold_xact_lock(holder, self.id, self.lock_proc(), xid);
            if !self.xact_locks.contains(&holder) {
                self.xact_locks.push(holder);
            }
        }
        let kh = locks::KeyHolder { holder, xid, token, row };
        self.db.locks.set_key_holder(key, self.id, kh);
    }

    /// Wait for the writer of an in-progress index entry: its speculative insertion to
    /// finish (SpeculativeInsertionWait) or its (sub)transaction to end (XactLockTableWait).
    pub fn wait_for_key_holder(&mut self, kh: &locks::KeyHolder) -> Result<()> {
        let k = match kh.token {
            Some(token) => locks::spectoken_key(kh.xid, token),
            None => locks::xact_key(kh.holder),
        };
        self.wait_for_lock(&k)
    }

    /// Wait for (sub)transaction `holder` (by its lock id) to end (XactLockTableWait).
    pub fn wait_for_xact(&mut self, holder: u64) -> Result<()> {
        self.wait_for_lock(&locks::xact_key(holder))
    }

    fn wait_for_lock(&mut self, k: &[u8]) -> Result<()> {
        let proc = self.lock_proc();
        self.db
            .locks
            .acquire(k, self.id, proc, LockMode::Share, &self.waits(WaitPolicy::Block), &String::new)?;
        self.db.locks.release(k, self.id, LockMode::Share);
        Ok(())
    }

    /// Begin a speculative insertion (SpeculativeInsertionLockAcquire): its token, whose lock
    /// is held until `end_speculative`.
    pub fn begin_speculative(&mut self) -> Result<u32> {
        self.spec_token = self.spec_token.wrapping_add(1).max(1);
        let token = self.spec_token;
        let xid = self.xid();
        self.lock(&locks::spectoken_key(xid, token), LockMode::Exclusive, WaitPolicy::Block, &String::new)?;
        Ok(token)
    }

    /// SpeculativeInsertionLockRelease.
    pub fn end_speculative(&mut self, token: u32) {
        let xid = self.xid();
        self.unlock(&locks::spectoken_key(xid, token), LockMode::Exclusive);
    }

    /// Row `key` (locked by this transaction) is being updated or deleted by it.
    pub fn row_modified(&mut self, key: &[u8]) {
        let holder = self.holder();
        self.db.locks.mark_row_modified(key, self.id, holder);
    }

    /// The (sub)transaction (lock id) of another backend updating or deleting row `key`.
    pub fn row_modifier(&self, key: &[u8]) -> Option<u64> {
        self.db.locks.row_modifier(key, self.lock_proc())
    }

    /// The in-progress entry another backend holds unique key lock `key` for.
    pub fn unique_key_holder(&self, key: &[u8]) -> Option<locks::KeyHolder> {
        self.db.locks.key_holder(key, self.lock_proc())
    }

    /// Lock relation `rel` in `mode` (LockRelationOid) until the transaction ends.
    pub fn lock_relation(&mut self, rel: u32, mode: LockMode, policy: WaitPolicy, name: &dyn Fn() -> String) -> Result<bool> {
        let key = locks::relation_key(self.db.oid, rel);
        self.lock(&key, mode, policy, &|| format!("relation \"{}\"", name()))
    }

    /// The lock id of the running (sub)transaction (whose transaction lock its row locks
    /// are waited for through).
    fn holder(&self) -> u64 {
        self.savepoints.last().map_or(self.id, |sp| sp.lock_id)
    }

    /// Lock row `key` of relation `rel` in `mode` (heap_lock_tuple's wait for conflicting
    /// lockers); `what` names the relation. Ok(false) only for SKIP LOCKED.
    pub fn lock_row(
        &mut self,
        rel: u32,
        key: &[u8],
        mode: RowLockMode,
        policy: WaitPolicy,
        what: &dyn Fn() -> String,
    ) -> Result<bool> {
        let holder = self.holder();
        let xid = self.cur_xid();
        let tuple = locks::tuple_key(self.db.oid, rel, key);
        let got = self.db.locks.lock_row(
            key,
            &tuple,
            holder,
            self.id,
            self.lock_proc(),
            xid,
            mode,
            &self.waits(policy),
            what,
        )?;
        if got {
            if !self.xact_locks.contains(&holder) {
                self.xact_locks.push(holder);
            }
            self.row_locks.push((key.to_vec(), self.savepoints.len()));
        }
        Ok(got)
    }

    /// The strongest row lock this transaction holds on `key`.
    pub fn row_lock_held(&self, key: &[u8]) -> Option<RowLockMode> {
        self.db.locks.row_held(key, self.id)
    }

    /// Whether `key`'s latest change (committed at `ts`) moved it to another partition.
    pub fn moved_at(&self, key: &[u8], ts: u64) -> bool {
        self.db.moved.lock().get(key) == Some(&ts)
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
            lock_journal_len: self.lock_journal.len(),
            lock_id: self.db.next_txn.fetch_add(1, Ordering::Relaxed),
            lock_ids: Vec::new(),
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
        let journal_len = sp.lock_journal_len;
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
            match prev {
                Some(v) => {
                    self.writes.insert(k, (v, prev_cid));
                }
                None => {
                    self.writes.remove(&k);
                }
            }
        }
        self.catalog = catalog;
        if self.catalog.is_none() {
            self.catalog_base = None;
            self.catalog_base_cat = None;
        }
        // Locks taken after the savepoint are released, as Postgres does for subtransactions.
        self.release_locks_above(i, journal_len);
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
        self.savepoints[i].lock_id = self.db.next_txn.fetch_add(1, Ordering::Relaxed);
        self.savepoints[i].lock_ids.clear();
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
        // Their locks become the parent's: a later abort of it releases them.
        let ids: Vec<u64> = self.savepoints[i..]
            .iter()
            .flat_map(|sp| sp.lock_ids.iter().copied().chain([sp.lock_id]))
            .collect();
        if let Some(p) = i.checked_sub(1) {
            self.savepoints[p].lock_ids.extend(ids);
        }
        for d in self.locks_held.values_mut() {
            *d = (*d).min(i);
        }
        for (_, d) in &mut self.row_locks {
            *d = (*d).min(i);
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
        if self.sxact
            && let Err(e) = self.db.ssi.pre_commit(self.id)
        {
            self.ssi_finish(false, 0);
            return Err(e);
        }
        // (The undo log is done with: freed before the commit copies anything.)
        drop(std::mem::take(&mut self.journal));
        let mut writes: Vec<(Vec<u8>, Option<Bytes>)> = Vec::with_capacity(self.writes.len());
        let mut new_catalog = false;
        for (k, (v, _)) in std::mem::take(&mut self.writes) {
            if k.as_slice() == CATALOG_KEY {
                new_catalog = true;
            } else {
                writes.push((k, v));
            }
        }
        let catalog = RefCell::new(self.catalog.take());
        let base = self.catalog_base;
        let base_cat = self.catalog_base_cat.take();
        let start = self.start_ts;
        let strict = self.isolation.txn_snapshot();
        let keys: Vec<Vec<u8>> = if strict {
            writes.iter().map(|(k, _)| k.clone()).collect()
        } else {
            Vec::new()
        };
        let pending = catalog.borrow().clone();
        if new_catalog && let Some(c) = &pending {
            // The records of what changed since the catalog the DDL started from (the one
            // published now, unless other DDL committed since: `check` then writes the
            // merged catalog's over the current one instead).
            match self.catalog_writes(c) {
                Ok(w) => writes.extend(w),
                Err(e) => {
                    self.finish_xid(false);
                    self.release_locks();
                    return Err(e);
                }
            }
        }
        let db = self.db.clone();
        let check = |store: &Store, writes: &mut Vec<(Vec<u8>, Option<Bytes>)>| -> Result<()> {
            // Other DDL committed since this transaction's began: its catalog changes are
            // carried onto the current catalog (the commit lock is held, so it stays current).
            if let Some(base) = base
                && db.catalog().version != base
            {
                let cur = db.catalog();
                let (Some(b), Some(mine)) = (base_cat.as_deref(), catalog.borrow().clone()) else {
                    return Err(Error::new(
                        code::SERIALIZATION_FAILURE,
                        "could not serialize access due to concurrent schema change",
                    ));
                };
                let mut merged = catmerge::merge(b, &mine, &cur)?;
                merged.version = cur.version;
                merged.bump();
                merged.freeze();
                if new_catalog {
                    writes.retain(|w| !persist::is_catalog_key(&w.0));
                    writes.extend(persist::writes(&cur, &merged)?);
                }
                *catalog.borrow_mut() = Some(Arc::new(merged));
            }
            if strict {
                for k in &keys {
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
        } else if catalog.borrow().is_some() {
            let db2 = self.db.clone();
            let publish = self.publish;
            // A new catalog is published under the commit lock, with the data (`check` may
            // have merged it onto the current one).
            self.db.store.commit_with(writes, check, |_| {
                if let Some(c) = catalog.borrow_mut().take() {
                    if !publish {
                        db2.last_local.store(c.version, Ordering::Release);
                    }
                    *db2.catalog.write() = c;
                }
            })
        } else {
            self.db.store.commit(writes, |s| check(s, &mut Vec::new()))
        };
        self.finish_xid(result.is_ok());
        if let Ok(ts) = &result
            && !self.moved_out.is_empty()
        {
            let mut m = self.db.moved.lock();
            // (Kept bounded: what's waited on is recent.)
            if m.len() > 100_000 {
                m.clear();
            }
            for k in self.moved_out.drain(..) {
                m.insert(k, *ts);
            }
        }
        self.release_locks();
        match &result {
            Ok(ts) => self.ssi_finish(true, *ts),
            Err(_) => self.ssi_finish(false, 0),
        }
        result
    }

    /// The writes storing catalog `c` over the one this transaction's DDL started from
    /// (`persist::writes`).
    fn catalog_writes(&self, c: &Catalog) -> Result<Vec<(Vec<u8>, Option<Bytes>)>> {
        let cur = self.db.catalog();
        persist::writes(&cur, c)
    }

    pub fn rollback(mut self) {
        self.finish_xid(false);
        self.release_locks();
        self.ssi_finish(false, 0);
    }

    /// ReleasePredicateLocks, once.
    fn ssi_finish(&mut self, commit: bool, ts: u64) {
        if std::mem::take(&mut self.sxact) {
            self.db.ssi.finish(self.id, commit, ts);
        }
    }

    /// Release the locks of the subtransactions from savepoint `i` on (taken after the
    /// journal held `journal_len`): heavyweight ones, then their row locks and transaction
    /// locks.
    fn release_locks_above(&mut self, i: usize, journal_len: usize) {
        while self.lock_journal.len() > journal_len {
            let k = self.lock_journal.pop().unwrap();
            self.locks_held.remove(&k);
            self.db.locks.release(&k.0, self.id, k.1);
        }
        let holders: Vec<u64> = self.savepoints[i..]
            .iter()
            .flat_map(|sp| sp.lock_ids.iter().copied().chain([sp.lock_id]))
            .collect();
        let rows: Vec<Vec<u8>> = self
            .row_locks
            .iter()
            .filter(|(_, d)| *d > i)
            .map(|(k, _)| k.clone())
            .collect();
        self.row_locks.retain(|(_, d)| *d <= i);
        self.db.locks.release_subxact(self.id, &holders, &rows);
        self.xact_locks.retain(|h| !holders.contains(h));
    }

    /// An error aborted the running (sub)transaction (AbortTransaction /
    /// AbortSubTransaction): its locks go now, not when the block is rolled back.
    pub fn release_aborted_locks(&mut self) {
        match self.savepoints.len().checked_sub(1) {
            Some(i) => {
                let journal_len = self.savepoints[i].lock_journal_len;
                self.release_locks_above(i, journal_len);
            }
            None => self.release_locks(),
        }
    }

    fn release_locks(&mut self) {
        if self.locks_held.is_empty() && self.row_locks.is_empty() && self.xact_locks.is_empty() {
            return;
        }
        let mut keys: Vec<Vec<u8>> = self.locks_held.drain().map(|((k, _), _)| k).collect();
        keys.sort();
        keys.dedup();
        keys.extend(self.xact_locks.drain(..).map(locks::xact_key));
        self.lock_journal.clear();
        let rows: Vec<Vec<u8>> = self.row_locks.drain(..).map(|(k, _)| k).collect();
        self.db.locks.release_with_rows(self.id, keys, rows);
    }

    /// pg_export_snapshot(): export this transaction's current snapshot under a new
    /// identifier (`%08X-%08X-%d`: the backend, the transaction, the count of exports so far
    /// + 1) for SET TRANSACTION SNAPSHOT in other sessions, until this transaction ends.
    pub fn export_snapshot(&mut self, backend: i32) -> Result<String> {
        if self.in_savepoint() {
            bail!(
                ACTIVE_SQL_TRANSACTION,
                "cannot export a snapshot from a subtransaction"
            );
        }
        let xids = self.db.xids();
        let n = xids.exported_by(self.db.instance, self.id) + 1;
        let id = format!("{:08X}-{:08X}-{n}", backend as u32, self.id as u32);
        xids.export(
            id.clone(),
            xids::Exported {
                db: self.db.instance,
                owner: self.id,
                db_oid: self.db.oid,
                ts: self.snapshot_ts,
                serializable: self.isolation == Isolation::Serializable,
                read_only: self.read_only,
                pin: self.db.store.pin(self.snapshot_ts),
            },
        );
        self.exported = true;
        Ok(id)
    }

    /// SET TRANSACTION SNAPSHOT (ImportSnapshot): run on the exported snapshot `id`. The
    /// caller checked this is a fresh REPEATABLE READ / SERIALIZABLE transaction.
    pub fn import_snapshot(&mut self, id: &str) -> Result<()> {
        let Some((db_oid, ts, serializable, read_only)) = self.db.xids().exported(id) else {
            bail!(UNDEFINED_OBJECT, "snapshot \"{id}\" does not exist");
        };
        if self.isolation == Isolation::Serializable {
            if !serializable {
                bail!(
                    FEATURE_NOT_SUPPORTED,
                    "a serializable transaction cannot import a snapshot from a non-serializable transaction"
                );
            }
            if read_only && !self.read_only {
                bail!(
                    FEATURE_NOT_SUPPORTED,
                    "a non-read-only serializable transaction cannot import a snapshot from a read-only transaction"
                );
            }
        }
        if db_oid != self.db.oid {
            bail!(
                FEATURE_NOT_SUPPORTED,
                "cannot import a snapshot from a different database"
            );
        }
        self._pin = Some(self.db.store.pin(ts));
        self.xmin = Some(self.db.xids().register_snapshot_of(Some(self.id)));
        self.snapshot_ts = ts;
        self.start_ts = ts;
        // (The transaction's snapshot is this one now: SSI tracks it from here.)
        self.snapshot_fixed = true;
        if self.isolation == Isolation::Serializable {
            self.register_serializable()?;
        }
        Ok(())
    }

    /// Whether a statement took the transaction's snapshot already (FirstSnapshotSet).
    pub fn snapshot_taken(&self) -> bool {
        self.snapshot_fixed
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

/// The name of a system catalog whose rows are locked (lock waits' messages).
fn catalog_name(class: u32) -> &'static str {
    match class {
        1259 => "pg_class",
        1260 => "pg_authid",
        1261 => "pg_auth_members",
        1262 => "pg_database",
        1255 => "pg_proc",
        2615 => "pg_namespace",
        1247 => "pg_type",
        _ => "pg_catalog",
    }
}

impl Drop for Txn {
    fn drop(&mut self) {
        self.ssi_finish(false, 0);
        self.finish_xid(false);
        self.release_locks();
        if std::mem::take(&mut self.exported) {
            self.db.xids().unexport(self.db.instance, self.id);
        }
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

/// `TxnScan` in descending key order.
pub struct TxnRevScan {
    store: crate::storage::RevScan,
    store_head: Option<(Vec<u8>, Bytes)>,
    buffered: std::iter::Peekable<std::vec::IntoIter<(Vec<u8>, Option<Bytes>)>>,
    store_done: bool,
}

impl Iterator for TxnRevScan {
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
                (Some((sk, _)), Some((bk, _))) if sk > bk => {
                    return self.store_head.take().map(Ok);
                }
                (s, Some(_)) => {
                    if s.is_some_and(|(sk, _)| Some(sk) == self.buffered.peek().map(|b| &b.0)) {
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
