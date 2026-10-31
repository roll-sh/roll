//! Databases and transactions: snapshots over the MVCC store, buffered writes, row locks,
//! savepoints, sequences, and transactional catalog changes.

pub(crate) mod catmerge;
mod bulklocks;
pub mod locks;
pub(crate) mod spill;
pub mod ssi;
pub mod toast;
pub mod twophase;
pub mod watch;
pub mod xids;

use crate::catalog::{Catalog, Sequence, persist};
use crate::error::{Error, Result, code};
use crate::storage::{SnapshotPin, Store};
use crate::{bail, err};
use bytes::Bytes;
use locks::{LockManager, LockMode, RowLockMode, WaitPolicy, Waits};
use parking_lot::{Mutex, RwLock};
use spill::{Ent, LATEST, Runs};
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

/// Write buffers spilled to disk so far (`txn::spill`), for tests.
pub static TXN_SPILLS: AtomicUsize = AtomicUsize::new(0);

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
    /// Writes to tables structures are derived from, logged as they commit (`watch`).
    pub watch: watch::Watch,
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
    /// Prepared transactions (the engine's registry, shared by every database).
    twophase: RwLock<Arc<twophase::TwoPhase>>,
    /// The modelled on-disk layout of tables and indexes (`exec::sizing`).
    pub relfiles: RwLock<crate::exec::sizing::RelFiles>,
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
    /// The catalog of the last commit appended to the WAL and not published yet, with its
    /// commit timestamp (`Store::commit_grouped`): what the next DDL commit checks against
    /// and merges onto (`latest_catalog`), published once that commit is durable.
    pending: Mutex<Option<(u64, Arc<Catalog>)>>,
    /// The commit timestamp of the catalog published last (an earlier commit's, published
    /// after it, doesn't replace it).
    published_ts: AtomicU64,
    /// The change stream's state (`repl::log`).
    pub log: crate::repl::log::DbLog,
    /// The cluster's replication state (WAL positions, slots), when it has one.
    repl: RwLock<Option<Arc<crate::repl::Repl>>>,
}

type DetachViewEntry = (u64, Vec<(u32, u32)>, bool, Arc<Catalog>);

impl Database {
    pub fn open(name: &str, oid: u32, store: Arc<Store>, owner: &str) -> Result<Arc<Database>> {
        let ts = store.visible_ts();
        let catalog = match store.get(CATALOG_KEY, ts)? {
            Some(b) => {
                let stored = persist::load(&b, |from, to| {
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
            watch: Default::default(),
            seqs: Mutex::new(HashMap::new()),
            next_txn: AtomicU64::new(1),
            sessions: AtomicUsize::new(0),
            temp_cleanups: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            notify: Default::default(),
            lo_oid: Mutex::new(0),
            last_local: AtomicU64::new(version),
            xids: RwLock::new(Arc::new(xids::Xids::local())),
            twophase: RwLock::new(Arc::new(twophase::TwoPhase::local())),
            relfiles: RwLock::new(Default::default()),
            pgstat: Mutex::new(Default::default()),
            sys_rows: Mutex::new(Default::default()),
            detach_marks: Mutex::new(HashMap::new()),
            detach_views: Mutex::new(Vec::new()),
            pending: Mutex::new(None),
            published_ts: AtomicU64::new(0),
            log: Default::default(),
            repl: RwLock::new(None),
        }))
    }

    /// Take part in the cluster's WAL positions and slots (`repl`).
    pub fn set_repl(&self, r: Arc<crate::repl::Repl>) {
        *self.repl.write() = Some(r);
    }

    pub fn repl(&self) -> Option<Arc<crate::repl::Repl>> {
        self.repl.read().clone()
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

    /// The catalog a commit's DDL is checked against and merged onto (under the store's
    /// commit lock): the last one appended to the WAL, published or not.
    pub(crate) fn latest_catalog(&self) -> Arc<Catalog> {
        let pending = self.pending.lock().as_ref().map(|(_, c)| c.clone());
        pending.unwrap_or_else(|| self.catalog())
    }

    /// Catalog `c` of the commit at `ts` published, unless a later commit's is already.
    fn publish_catalog(&self, ts: u64, c: Arc<Catalog>) {
        {
            let mut w = self.catalog.write();
            if self.published_ts.load(Ordering::Acquire) <= ts {
                self.published_ts.store(ts, Ordering::Release);
                *w = c;
            }
        }
        let mut p = self.pending.lock();
        if p.as_ref().is_some_and(|(t, _)| *t <= ts) {
            *p = None;
        }
    }

    /// Hand out transaction ids from the cluster's counter.
    pub fn set_xids(&self, x: Arc<xids::Xids>) {
        *self.xids.write() = x;
    }

    pub fn xids(&self) -> Arc<xids::Xids> {
        self.xids.read().clone()
    }

    /// Take part in the cluster's prepared transactions.
    pub fn set_twophase(&self, t: Arc<twophase::TwoPhase>) {
        *self.twophase.write() = t;
    }

    pub fn twophase(&self) -> Arc<twophase::TwoPhase> {
        self.twophase.read().clone()
    }

    /// The prepared transactions whose state records this database's store holds, made again
    /// (`twophase::recover`) and registered; how many. Run when the database opens, after it
    /// joined the cluster's transaction ids and registry.
    /// `reset`: the unlogged relations reset as this open began (after a crash); prepared
    /// transactions' writes to them are dropped, from their state records too.
    pub fn recover_prepared(self: &Arc<Self>, reset: &[u32]) -> Result<usize> {
        let (from, to) = twophase::record_range();
        let ts = self.store.visible_ts();
        let records: Vec<(Vec<u8>, Bytes)> = self.store.scan(from, to, ts).collect::<Result<_>>()?;
        let tp = self.twophase();
        let mut n = 0;
        let reset: std::collections::HashSet<u32> = reset.iter().copied().collect();
        // (A key's first 4 bytes are its relation's oid, as `index_prefix` writes it.)
        let dropped = |k: &[u8]| k.get(..4).is_some_and(|o| reset.contains(&u32::from_be_bytes(o.try_into().expect("4 bytes"))));
        for (key, v) in &records {
            let filtered = if reset.is_empty() { None } else { twophase::without_writes(v, dropped)? };
            if let Some(f) = &filtered {
                self.store.commit(vec![(key.clone(), Some(Bytes::copy_from_slice(f)))], |_| Ok(()))?;
            }
            let v = filtered.as_deref().unwrap_or(&v[..]);
            if let Some((info, mut txn)) = twophase::recover(self, v)? {
                crate::repl::log::recovered(self, &mut txn, &info.gid)?;
                tp.recovered(info, txn);
                n += 1;
            }
        }
        Ok(n)
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
            derived: HashMap::new(),
            journal_base: 0,
            runs: None,
            buf_bytes: 0,
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
            read_catalog: None,
            locks_held: HashMap::new(),
            lock_journal: Vec::new(),
            row_locks: Vec::new(),
            unique_keys_taken: 0,
            xact_locks: Vec::new(),
            spec_token: 0,
            savepoints: Vec::new(),
            cancel,
            lock_timeout: None,
            deadlock_timeout: Duration::from_secs(1),
            deadline: None,
            txn_deadline: None,
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
            async_commit: None,
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
            prepared: false,
            log: None,
            entry_deletes: Vec::new(),
            entry_deletes_bytes: 0,
            entry_deletes_direct: None,
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

    /// ResetUnloggedRelations, when the database first opens after a crash. Every unlogged
    /// table loses its rows and index entries, and unlogged sequences start over, which is
    /// what copying their init forks does. Runs before any session can see the database.
    /// Returns the relations reset.
    pub fn reset_unlogged(&self) -> Result<Vec<u32>> {
        let cat = self.catalog();
        let tables: Vec<u32> = cat.tables.values().filter(|t| t.unlogged && !t.temporary).map(|t| t.oid).collect();
        let seqs: Vec<u32> = (cat.sequences.values())
            .filter(|s| {
                let owner = || s.owned_by.and_then(|(t, _)| cat.tables.get(&t)).is_some_and(|t| t.unlogged);
                s.unlogged.unwrap_or_else(owner)
            })
            .map(|s| s.oid)
            .collect();
        for &oid in &tables {
            let start = crate::storage::keys::index_prefix(oid, 0);
            // (Rows, index entries and out-of-line values: `storage::toast`.)
            let end = crate::storage::keys::prefix_end(&crate::storage::toast::prefix(oid));
            loop {
                let ts = self.store.visible_ts();
                let batch: Vec<(Vec<u8>, Option<Bytes>)> = (self.store.scan_raw(&start, &end, ts))
                    .take(10_000)
                    .map(|e| e.map(|(k, _)| (k, None)))
                    .collect::<Result<_>>()?;
                if batch.is_empty() {
                    break;
                }
                self.store.commit(batch, |_| Ok(()))?;
            }
        }
        for &oid in &seqs {
            self.reset_seq(oid, None)?;
        }
        Ok(tables.into_iter().chain(seqs).collect())
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

/// A heavyweight lock a transaction holds: its key and mode.
pub type HeldLock = (Vec<u8>, LockMode);

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
    /// Buffered writes: each key's latest (`Ent`), since the last spill.
    writes: BTreeMap<Vec<u8>, Ent>,
    /// Undo log: (key, what the buffer held before), for savepoints and snapshots, since
    /// the last spill. A write's place in it (`journal_base` on) is its journal position.
    journal: Vec<(Vec<u8>, Option<Ent>)>,
    /// The journal position of `journal[0]`: writes before it were spilled to `runs`.
    journal_base: usize,
    /// Structures over this transaction's own writes (`exec::search`), by index: dropped by
    /// a savepoint rollback or a spill, past which the journal positions they read to mean
    /// nothing.
    pub derived: HashMap<u32, Box<dyn std::any::Any + Send>>,
    /// The writes spilled so far (`spill`), every version (None: none yet).
    runs: Option<Box<Runs>>,
    /// About how many bytes `writes` and `journal` hold.
    buf_bytes: usize,
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
    /// The catalog the running statement reads (`catalog`, while it has none of its own):
    /// `stmt_catalog`, then the database's as it is whenever the statement takes a new
    /// lock. (Never older than `stmt_catalog`, which stays the base of its DDL.)
    read_catalog: Option<Arc<Catalog>>,
    /// Heavyweight locks held, by key and mode, with the subtransaction depth that took them.
    locks_held: HashMap<HeldLock, usize>,
    /// The order they were taken in (released by ROLLBACK TO in reverse).
    lock_journal: Vec<HeldLock>,
    /// Rows locked (or written) here, with the depth that locked them.
    row_locks: Vec<(Vec<u8>, usize)>,
    /// Unique key locks taken the usual way (past `bulklocks::compact_after()`, plain ones go
    /// compact).
    unique_keys_taken: usize,
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
    /// When the transaction times out (transaction_timeout, from its start or from the SET
    /// that turned it on: the session ends).
    pub txn_deadline: Option<Instant>,
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
    /// Commit without waiting for the WAL fsync (`synchronous_commit = off`), the WAL
    /// writer's delay in ms (wal_writer_delay): `Store::commit_async`. None: synchronous.
    pub async_commit: Option<u64>,
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
    /// A prepared transaction (PREPARE TRANSACTION): its commit checks ran when it prepared.
    prepared: bool,
    /// Its change stream log, while it logs its changes (`repl::log`): decided when it gets
    /// its transaction id.
    pub(crate) log: Option<Box<crate::repl::log::TxnLog>>,
    /// Index entries the running statement deleted, going in together at its end
    /// (`defer_entry_delete`), with about how many bytes they hold.
    entry_deletes: Vec<Vec<u8>>,
    entry_deletes_bytes: usize,
    /// How the statement's earlier batch of them went in (true: as a run).
    entry_deletes_direct: Option<bool>,
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
        // (What an earlier statement deferred is the next one's to see.)
        self.flush_entry_deletes()?;
        if self.catalog.is_none() {
            let c = self.db.catalog();
            self.read_catalog = Some(c.clone());
            self.stmt_catalog = Some(c);
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
            let safe = match self.db.ssi.wait_safe(self.id, &self.waits(WaitPolicy::Block)) {
                Ok(s) => s,
                Err(e) => {
                    // No snapshot was set: the next statement that needs one waits again
                    // (Parse binding a query twice must not lose the error).
                    self.sxact = false;
                    self.snapshot_fixed = false;
                    return Err(e);
                }
            };
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
                // The catalog as the statement began (or as it was when the statement last
                // took a lock, AcceptInvalidationMessages): every read of one statement sees
                // one version, however other sessions' DDL commits meanwhile (a scan of
                // pg_class and one of pg_attribute read different versions).
                let base = self.read_catalog.clone().unwrap_or_else(|| self.db.catalog());
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
        let (cid, ts) = match self.writes.get(CATALOG_KEY) {
            Some(e) => (e.cid, e.ts),
            None => (
                self.runs_get(CATALOG_KEY, LATEST)?.and_then(|e| e.1),
                self.position() as u64,
            ),
        };
        self.writes.insert(
            CATALOG_KEY.to_vec(),
            Ent {
                v: Some(Bytes::new()),
                cid,
                ts,
            },
        );
        c.freeze();
        self.catalog = Some(Arc::new(c));
        Ok(())
    }

    fn buffer(&mut self, key: Vec<u8>, value: Option<Bytes>) {
        self.record(key, value);
        self.wrote = true;
        self.cur_xid();
    }

    /// The journal position the next write takes.
    fn position(&self) -> usize {
        self.journal_base + self.journal.len()
    }

    /// Put a write in the buffer, stamped with the current command id, and journal it.
    fn record(&mut self, key: Vec<u8>, value: Option<Bytes>) {
        self.buf_bytes += 2 * key.len() + value.as_ref().map_or(0, |v| v.len()) + 120;
        let ent = Ent {
            v: value,
            cid: Some(self.write_cid),
            ts: self.position() as u64 + 1,
        };
        let prev = self.writes.insert(key.clone(), ent);
        if let Some(p) = &prev {
            self.buf_bytes += p.v.as_ref().map_or(0, |v| v.len());
        }
        self.journal.push((key, prev));
    }

    /// Spill the buffer to a run once it holds more than `txn_spill_bytes`
    /// (`txn::spill`): every version it has, each with its journal place, so snapshots and
    /// savepoints read what they read before.
    fn maybe_spill(&mut self) -> Result<()> {
        if self.buf_bytes < self.db.store.txn_spill_bytes() {
            return Ok(());
        }
        self.spill()
    }

    fn spill(&mut self) -> Result<()> {
        self.derived.clear();
        TXN_SPILLS.fetch_add(1, Ordering::Relaxed);
        let mut older: Vec<(Vec<u8>, Ent)> = self
            .journal
            .iter()
            .filter_map(|(k, prev)| prev.clone().map(|e| (k.clone(), e)))
            .collect();
        older.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.ts.cmp(&a.1.ts)));
        let dir = self.db.store.spill_dir().join(self.id.to_string());
        let runs = self.runs.get_or_insert_with(|| Box::new(Runs::new(dir)));
        // Each key's newest version (the buffer's) before its older ones (the journal's).
        let mut cur = self.writes.iter().peekable();
        let mut old = older.into_iter().peekable();
        let mut versions = std::iter::from_fn(|| {
            let take_cur = match (cur.peek(), old.peek()) {
                (None, None) => return None,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some((ck, _)), Some((ok, _))) => ck.as_slice() <= ok.as_slice(),
            };
            if take_cur {
                cur.next().map(|(k, e)| (k.clone(), e.clone()))
            } else {
                old.next()
            }
        });
        runs.write(&mut versions)?;
        self.journal_base = self.position();
        // (Its room kept: the buffer fills again as it did.)
        self.journal.clear();
        self.writes = BTreeMap::new();
        self.buf_bytes = 0;
        Ok(())
    }

    /// The spilled version of `key` at journal mark `mark`: (value, command id).
    fn runs_get(&self, key: &[u8], mark: u64) -> Result<Option<(Option<Bytes>, Option<u32>)>> {
        match &self.runs {
            Some(r) => r.get(key, mark),
            None => Ok(None),
        }
    }

    /// This transaction's latest write of `key`: (value, command id); None: it wrote none.
    fn buf_get(&self, key: &[u8]) -> Result<Option<(Option<Bytes>, Option<u32>)>> {
        match self.writes.get(key) {
            Some(e) => Ok(Some((e.v.clone(), e.cid))),
            None => self.runs_get(key, LATEST),
        }
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
        match self.buf_get(key) {
            Ok(w) => w.and_then(|w| w.1),
            Err(e) => {
                tracing::warn!(error = %e, "reading a transaction's spilled writes failed");
                None
            }
        }
    }

    /// This transaction's id, given it now if it has none (as writing or
    /// pg_current_xact_id() does).
    pub fn xid(&mut self) -> u64 {
        if self.xid == 0 {
            self.xid = self.db.xids().assign();
            // (After the id is running: a slot being made either sees this transaction
            // running, and waits for it, or this transaction sees the slot.)
            if self.db.log.capture.load(Ordering::SeqCst) {
                self.log = Some(Box::default());
            }
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

    /// The xmin of the snapshot it holds (pg_stat_activity's backend_xmin): a READ
    /// COMMITTED statement's while it runs, a transaction snapshot's from its first
    /// statement on.
    pub fn snapshot_xmin(&self) -> Option<u64> {
        if !self.isolation.txn_snapshot() && !self.stmt_snapshot {
            return None;
        }
        self.xmin.as_ref().map(|x| x.xmin())
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

    /// A change stream record (`repl::log`), in a read-only transaction too.
    pub fn put_log(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        self.record(key, Some(Bytes::from(value)));
        self.wrote = true;
        self.maybe_spill()
    }

    /// The subtransaction ids a commit now commits along (its released savepoints').
    pub fn committed_subxids(&self) -> Vec<u64> {
        self.sub_committed
            .iter()
            .copied()
            .chain(self.savepoints.iter().flat_map(|sp| sp.committed.iter().copied().chain([sp.xid])))
            .filter(|&x| x != 0)
            .collect()
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        if self.read_only && !self.temp_key(&key) {
            bail!(
                READ_ONLY_SQL_TRANSACTION,
                "cannot execute this statement in a read-only transaction"
            );
        }
        self.buffer(key, Some(Bytes::from(value)));
        self.maybe_spill()
    }

    /// Replace the bytes of a row this transaction wrote with the same row stored with its
    /// heap TID (`sizing::stamp_tids`, once its batch is modelled): in place while the
    /// current command's version is the newest, in the write buffer (no reader can tell the
    /// two apart, so there is nothing to journal), else a `put`.
    pub fn restamp(&mut self, key: &[u8], value: Vec<u8>) -> Result<()> {
        if let Some(e) = self.writes.get_mut(key)
            && e.cid == Some(self.write_cid)
            && let Some(old) = &e.v
        {
            self.buf_bytes = (self.buf_bytes + value.len()).saturating_sub(old.len());
            e.v = Some(Bytes::from(value));
            return Ok(());
        }
        self.put(key.to_vec(), value)
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
            || self.runs_get(&key, LATEST)?.is_some()
        {
            return self.put(key, value);
        }
        self.buf_bytes += key.len() + value.len() + 80;
        let ent = Ent {
            v: Some(Bytes::from(value)),
            cid: Some(self.write_cid),
            ts: self.position() as u64,
        };
        self.writes.insert(key, ent);
        self.wrote = true;
        self.cur_xid();
        self.maybe_spill()
    }

    /// `put_new_index_entry` of many entries of one new index at once, all in [start,
    /// end): when this transaction has written nothing there (and no savepoint is open),
    /// they go to disk sorted, as a run of the transaction's spilled writes (`txn::spill`),
    /// without passing through the write buffer (an index build's sorted runs). `direct`:
    /// how an earlier batch of the same build went in (its runs are the range's only
    /// writes); returns how this one did (true: as a run).
    pub fn put_new_index_entries(
        &mut self,
        mut entries: Vec<(Vec<u8>, Vec<u8>)>,
        start: &[u8],
        end: &[u8],
        direct: Option<bool>,
    ) -> Result<bool> {
        let direct = match direct {
            Some(d) => d,
            None => {
                let written = self
                    .writes
                    .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
                    .next()
                    .is_some()
                    || match &self.runs {
                        Some(r) => r.scan(start, end, LATEST).next().transpose()?.is_some(),
                        None => false,
                    };
                !(self.read_only || !self.savepoints.is_empty() || written)
            }
        };
        if !direct {
            for (k, v) in entries {
                self.put_new_index_entry(k, v)?;
            }
            return Ok(false);
        }
        sort_by_key_after(&mut entries, start.len(), |e| e.0.as_slice());
        let (cid, ts) = (Some(self.write_cid), self.position() as u64);
        let dir = self.db.store.spill_dir().join(self.id.to_string());
        let runs = self.runs.get_or_insert_with(|| Box::new(Runs::new(dir)));
        runs.write(&mut entries.into_iter().map(|(k, v)| {
            let v = Some(Bytes::from(v));
            (k, Ent { v, cid, ts })
        }))?;
        self.wrote = true;
        self.cur_xid();
        Ok(true)
    }

    /// `delete` of an index entry by a statement nothing in which can read the index
    /// through this transaction's writes before it ends (`exec::dml` decides: a DELETE
    /// without BEFORE ROW triggers or anything volatile, whose own snapshot doesn't see its
    /// writes): it waits in a list and goes in with the statement's others
    /// (`flush_entry_deletes`), sorted, as a run of the spilled writes when the transaction
    /// has written nothing else in those indexes, instead of a buffer write and its undo
    /// record each. (Postgres's DELETE leaves index entries alone; the engine's go with
    /// their rows.)
    pub fn defer_entry_delete(&mut self, key: Vec<u8>) -> Result<()> {
        if self.read_only || !self.savepoints.is_empty() {
            return self.delete(key);
        }
        self.entry_deletes_bytes += key.len() + 32;
        self.entry_deletes.push(key);
        let run_bytes = (self.db.store.txn_spill_bytes() / 2).min(32 << 20);
        if self.entry_deletes_bytes >= run_bytes {
            self.put_entry_deletes()?;
        }
        Ok(())
    }

    /// The statement's deferred index entry deletes go in (`defer_entry_delete`): it ends,
    /// or something is about to read what it wrote (its AFTER triggers).
    pub fn flush_entry_deletes(&mut self) -> Result<()> {
        let r = self.put_entry_deletes();
        self.entry_deletes_direct = None;
        r
    }

    fn put_entry_deletes(&mut self) -> Result<()> {
        let mut keys = std::mem::take(&mut self.entry_deletes);
        self.entry_deletes_bytes = 0;
        if keys.is_empty() {
            return Ok(());
        }
        sort_by_key_after(&mut keys, 0, |k| k.as_slice());
        let direct = match self.entry_deletes_direct {
            Some(d) => d,
            // (Each index's entries start with its table's and its own id: 8 bytes.)
            None => {
                self.savepoints.is_empty() && !keys.chunk_by(|a, b| a.get(..8) == b.get(..8)).any(|g| {
                    let start = &g[0][..g[0].len().min(8)];
                    let end = crate::storage::keys::prefix_end(start);
                    self.writes
                        .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end.as_slice())))
                        .next()
                        .is_some()
                        || self.runs.as_ref().is_some_and(|r| r.scan(start, &end, LATEST).next().is_some())
                })
            }
        };
        self.entry_deletes_direct = Some(direct);
        if !direct {
            for k in keys {
                self.delete(k)?;
            }
            return Ok(());
        }
        let (cid, ts) = (Some(self.write_cid), self.position() as u64);
        let dir = self.db.store.spill_dir().join(self.id.to_string());
        let runs = self.runs.get_or_insert_with(|| Box::new(Runs::new(dir)));
        runs.write(&mut keys.into_iter().map(|k| (k, Ent { v: None, cid, ts })))?;
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
        self.maybe_spill()
    }

    /// Our own write `v` of `key` (as snapshot `st` sees our writes; None: as they are now)
    /// with a row's out-of-line values put back, read over the store at `ts`.
    fn resolve_own(&self, key: &[u8], v: Option<Bytes>, st: Option<&SnapState>, ts: u64) -> Result<Option<Bytes>> {
        let v = match v {
            Some(v) if crate::storage::toast::has_external(&v) && crate::storage::toast::is_row_key(key) => v,
            other => return Ok(other),
        };
        let view = self.db.store.view();
        let fetch = &mut |k: &[u8]| {
            let own = match st {
                Some(st) => self.snap_get(k, st)?,
                None => self.buf_get(k)?.map(|e| e.0),
            };
            match own {
                Some(Some(b)) => Ok(b),
                _ => crate::storage::toast::read_value(&view, k, ts),
            }
        };
        Ok(Some(Bytes::from(crate::storage::toast::splice(key, &v, None, fetch)?)))
    }

    /// Read through this transaction's own writes.
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        if let Some((v, _)) = self.buf_get(key)? {
            return self.resolve_own(key, v, None, self.snapshot_ts);
        }
        self.db.store.get(key, self.snapshot_ts)
    }

    /// `get`'s value as it is stored (a row's out-of-line values' pointers), for a writer
    /// that changes the row's bytes and stores them again.
    pub fn get_raw(&self, key: &[u8]) -> Result<Option<Bytes>> {
        if let Some((v, _)) = self.buf_get(key)? {
            return Ok(v);
        }
        Ok(self.db.store.get_version_raw(key, self.snapshot_ts)?.and_then(|(_, v)| v))
    }

    /// Newest committed version (ignoring our snapshot), for locking reads after a wait.
    pub fn get_latest(&self, key: &[u8]) -> Result<Option<(u64, Option<Bytes>)>> {
        if let Some((v, _)) = self.buf_get(key)? {
            return Ok(Some((u64::MAX, self.resolve_own(key, v, None, u64::MAX >> 1)?)));
        }
        self.db.store.get_version(key, u64::MAX >> 1)
    }

    /// `get_latest`'s value as it is stored (a row's out-of-line values' pointers): the
    /// version a writer holding the row's lock replaces.
    pub fn get_latest_raw(&self, key: &[u8]) -> Result<Option<Bytes>> {
        if let Some((v, _)) = self.buf_get(key)? {
            return Ok(v);
        }
        Ok(self.db.store.get_version_raw(key, u64::MAX >> 1)?.and_then(|(_, v)| v))
    }

    /// Read through this transaction's own writes over the data committed as of `ts`.
    pub fn get_at(&self, key: &[u8], ts: u64) -> Result<Option<Bytes>> {
        if let Some((v, _)) = self.buf_get(key)? {
            return self.resolve_own(key, v, None, ts);
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

    /// The journal position the next write takes.
    pub fn journal_pos(&self) -> usize {
        self.journal_base + self.journal.len()
    }

    /// Each write at journal position `from` or later to a key in [start, end): the key,
    /// its value now and the one the write replaced (both this transaction's; None:
    /// deleted, or none). None: the journal before `from` was spilled.
    #[allow(clippy::type_complexity)]
    pub fn written_since(&self, start: &[u8], end: &[u8], from: usize) -> Option<Vec<(Vec<u8>, Option<Bytes>, Option<Bytes>)>> {
        if from < self.journal_base || self.runs.is_some() {
            return None;
        }
        let at = (from - self.journal_base).min(self.journal.len());
        Some(
            self.journal[at..]
                .iter()
                .filter(|(k, _)| k.as_slice() >= start && k.as_slice() < end)
                .map(|(k, prev)| (k.clone(), self.writes.get(k).and_then(|e| e.v.clone()), prev.as_ref().and_then(|p| p.v.clone())))
                .collect(),
        )
    }

    /// Our writes in [start, end), as they are now (the spilled ones too).
    pub fn writes_in(&self, start: &[u8], end: &[u8]) -> Result<BTreeMap<Vec<u8>, Option<Bytes>>> {
        let mut out = BTreeMap::new();
        if let Some(r) = &self.runs {
            for e in r.scan(start, end, LATEST) {
                let (k, v) = e?;
                out.insert(k, v);
            }
        }
        out.extend(self.mem_in(start, end));
        Ok(out)
    }

    /// The buffer's writes in [start, end) (none of the spilled ones).
    fn mem_in<'a>(
        &'a self,
        start: &[u8],
        end: &[u8],
    ) -> impl Iterator<Item = (Vec<u8>, Option<Bytes>)> + 'a {
        self.writes
            .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
            .map(|(k, e)| (k.clone(), e.v.clone()))
    }

    /// What lies under the buffer in [start, end): the spilled writes as of journal mark
    /// `mark`, over the store at `ts` (rows as stored: `TxnScan` puts their out-of-line
    /// values back).
    fn lower(&self, start: &[u8], end: &[u8], ts: u64, mark: u64) -> Lower {
        let store = self.db.store.scan_raw(start, end, ts);
        match &self.runs {
            Some(r) => Lower::Runs(Box::new(Overlay::new(r.scan(start, end, mark), store, false))),
            None => Lower::Store(store),
        }
    }

    /// `lower` in descending key order.
    fn lower_rev(&self, start: &[u8], end: &[u8], ts: u64, mark: u64) -> RevLower {
        let store = self.db.store.scan_rev_raw(start, end, ts);
        match &self.runs {
            Some(r) => RevLower::Runs(Box::new(Overlay::new(r.scan_rev(start, end, mark), store, true))),
            None => RevLower::Store(store),
        }
    }

    /// Where a reader at `ts` of the rows in [start, end) gets their out-of-line values
    /// (`toast::Source`): our writes as `own` says (our spilled ones as of `mark`) over the
    /// store's. None when no table row can be in the range.
    fn toast_source(&self, start: &[u8], end: &[u8], ts: u64, mark: u64, own: OwnVals<'_>) -> Option<Arc<toast::Source>> {
        use crate::storage::toast as t;
        if !t::may_hold_rows(start, end) {
            return None;
        }
        let own: Vec<(Vec<u8>, Option<Bytes>)> = if self.writes.is_empty() && !matches!(own, OwnVals::Map(_)) {
            Vec::new()
        } else {
            // (A range of one table's rows: its values' range; else every table's values.)
            let range = t::value_range(start, end);
            let (lo, hi) = range.clone().unwrap_or_else(|| (Vec::new(), vec![0xFF; 9]));
            let keep = |k: &[u8]| range.is_some() || t::is_value_key(k);
            match own {
                OwnVals::Runs => Vec::new(),
                OwnVals::Now => self.mem_in(&lo, &hi).filter(|(k, _)| keep(k)).collect(),
                OwnVals::Snap(st) => self.snap_own(&lo, &hi, st).into_iter().filter(|(k, _)| keep(k)).collect(),
                OwnVals::Map(m) => m
                    .range::<[u8], _>((Bound::Included(&lo[..]), Bound::Excluded(&hi[..])))
                    .filter(|(k, _)| keep(k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            }
        };
        let runs = self.runs.as_ref().and_then(|r| r.view()).map(|v| (v, mark));
        Some(Arc::new(toast::Source { own, runs, store: self.db.store.clone(), view: Default::default(), ts }))
    }

    /// Scan [start, end) at `ts`, merging `own` (all of our writes, captured earlier by
    /// `writes_in`; None: our current writes) over it.
    pub fn scan_overlay(
        &self,
        start: &[u8],
        end: &[u8],
        ts: u64,
        own: Option<&BTreeMap<Vec<u8>, Option<Bytes>>>,
    ) -> TxnScan {
        match own {
            Some(own) => {
                let range = (Bound::Included(start), Bound::Excluded(end));
                let src = self.toast_source(start, end, ts, LATEST, OwnVals::Map(own));
                let own: Vec<_> = own
                    .range::<[u8], _>(range)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                TxnScan::new(own, Lower::Store(self.db.store.scan_raw(start, end, ts)), src)
            }
            None => TxnScan::new(
                self.mem_in(start, end).collect(),
                self.lower(start, end, ts, LATEST),
                self.toast_source(start, end, ts, LATEST, OwnVals::Now),
            ),
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
            mark: self.position(),
            seen: self.position(),
            base: self.journal_base,
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
        let pos = self.position();
        if s.rollbacks != self.rollbacks || pos < s.seen {
            // Writes were undone: redo the record from the snapshot's mark.
            s.prev.clear();
            s.mark = s.mark.min(pos);
            s.seen = s.mark;
            s.rollbacks = self.rollbacks;
        }
        if s.base != self.journal_base {
            // The buffer was spilled: what it held at the mark is in the runs now.
            s.prev.clear();
            s.base = self.journal_base;
            s.seen = s.mark.max(self.journal_base);
        }
        // (A mark before the buffer's first write reads the runs as of the mark instead.)
        if s.mark < self.journal_base || s.seen >= pos {
            return;
        }
        let from = s.seen - self.journal_base;
        for (k, prev) in &self.journal[from..] {
            s.prev
                .entry(k.clone())
                .or_insert_with(|| prev.as_ref().map(|e| e.v.clone()));
        }
        s.seen = pos;
    }

    /// Our write of `key` as the snapshot `st` sees it: Some(value; None: deleted), None:
    /// not written then.
    fn snap_get(&self, key: &[u8], st: &SnapState) -> Result<Option<Option<Bytes>>> {
        if st.mark < self.journal_base {
            return Ok(self.runs_get(key, st.mark as u64)?.map(|e| e.0));
        }
        match st.prev.get(key) {
            Some(Some(v)) => Ok(Some(v.clone())),
            // (Nothing in the buffer then: a spilled write, or none. The runs as of the mark:
            // a run written since, a statement's deferred entry deletes, isn't seen.)
            Some(None) => Ok(self.runs_get(key, st.mark as u64)?.map(|e| e.0)),
            None => match self.writes.get(key) {
                Some(e) => Ok(Some(e.v.clone())),
                None => Ok(self.runs_get(key, st.mark as u64)?.map(|e| e.0)),
            },
        }
    }

    /// The buffer's writes in [start, end) as the snapshot `st` sees them (one whose mark
    /// is in the buffer's journal).
    fn snap_own(&self, start: &[u8], end: &[u8], st: &SnapState) -> BTreeMap<Vec<u8>, Option<Bytes>> {
        let mut own: BTreeMap<_, _> = self.mem_in(start, end).collect();
        for (k, prev) in st
            .prev
            .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
        {
            match prev {
                Some(v) => own.insert(k.clone(), v.clone()),
                None => own.remove(k),
            };
        }
        own
    }

    /// Scan [start, end) as of the snapshot `s`.
    pub fn scan_snap(&self, start: &[u8], end: &[u8], s: &Snap) -> TxnScan {
        self.catch_up(s);
        let st = s.st.borrow();
        if st.mark < self.journal_base {
            // Taken before the last spill: none of the buffer, the runs as they were.
            let src = self.toast_source(start, end, st.ts, st.mark as u64, OwnVals::Runs);
            return TxnScan::new(Vec::new(), self.lower(start, end, st.ts, st.mark as u64), src);
        }
        // (The runs as of the mark, as `snap_get` reads them.)
        let mark = st.mark as u64;
        if st.prev.is_empty() {
            let src = self.toast_source(start, end, st.ts, mark, OwnVals::Now);
            return TxnScan::new(self.mem_in(start, end).collect(), self.lower(start, end, st.ts, mark), src);
        }
        let own = self.snap_own(start, end, &st);
        let src = self.toast_source(start, end, st.ts, mark, OwnVals::Snap(&st));
        TxnScan::new(own.into_iter().collect(), self.lower(start, end, st.ts, mark), src)
    }

    /// Scan [start, end) in descending key order, as of the snapshot `s` (None: the
    /// transaction's current one, with all its writes).
    pub fn scan_rev(&self, start: &[u8], end: &[u8], s: Option<&Snap>) -> TxnRevScan {
        let (ts, own, mark, src) = match s {
            Some(s) => {
                self.catch_up(s);
                let st = s.st.borrow();
                if st.mark < self.journal_base {
                    let src = self.toast_source(start, end, st.ts, st.mark as u64, OwnVals::Runs);
                    (st.ts, BTreeMap::new(), st.mark as u64, src)
                } else {
                    let src = self.toast_source(start, end, st.ts, st.mark as u64, OwnVals::Snap(&st));
                    (st.ts, self.snap_own(start, end, &st), st.mark as u64, src)
                }
            }
            None => {
                let src = self.toast_source(start, end, self.snapshot_ts, LATEST, OwnVals::Now);
                (self.snapshot_ts, self.mem_in(start, end).collect(), LATEST, src)
            }
        };
        TxnRevScan {
            inner: Overlay::new(own.into_iter().rev().map(Ok as BufFn), self.lower_rev(start, end, ts, mark), true),
            src,
        }
    }

    /// The store and the time snapshot `s` reads it at, when this transaction has written
    /// nothing (its snapshot reads are then the store's at that time): what readers on other
    /// threads (a parallel scan's workers) read through, as `get_snap` and `scan_snap` would.
    pub fn store_reader(&self, s: &Snap) -> Option<(Arc<Store>, u64)> {
        if self.wrote || self.runs.is_some() || !self.writes.is_empty() {
            return None;
        }
        self.catch_up(s);
        let st = s.st.borrow();
        (st.prev.is_empty() && st.mark >= self.journal_base).then(|| (self.db.store.clone(), st.ts))
    }

    /// Read `key` as of the snapshot `s`.
    pub fn get_snap(&self, key: &[u8], s: &Snap) -> Result<Option<Bytes>> {
        self.catch_up(s);
        let st = s.st.borrow();
        match self.snap_get(key, &st)? {
            Some(v) => self.resolve_own(key, v, Some(&st), st.ts),
            None => self.db.store.get(key, st.ts),
        }
    }

    /// `get_snap` of each of `keys` (any order), the store's reads made together
    /// (`Store::get_versions`).
    pub fn get_many_snap(&self, keys: &[Vec<u8>], s: &Snap) -> Result<Vec<Option<Bytes>>> {
        self.get_many_snap_with(keys, s, true)
    }

    /// `get_many_snap` with rows as they are stored (their out-of-line values' pointers: a
    /// reader decoding some columns reads their values itself, from the source of a scan at
    /// `s`, `TxnScan::source`).
    pub fn get_many_snap_raw(&self, keys: &[Vec<u8>], s: &Snap) -> Result<Vec<Option<Bytes>>> {
        self.get_many_snap_with(keys, s, false)
    }

    fn get_many_snap_with(&self, keys: &[Vec<u8>], s: &Snap, resolve: bool) -> Result<Vec<Option<Bytes>>> {
        self.catch_up(s);
        let st = s.st.borrow();
        let mut out: Vec<Option<Bytes>> = vec![None; keys.len()];
        // The keys the store answers: (key, its place in `keys`).
        let mut reads: Vec<(&[u8], usize)> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            match self.snap_get(key, &st)? {
                Some(v) if resolve => out[i] = self.resolve_own(key, v, Some(&st), st.ts)?,
                Some(v) => out[i] = v,
                None => reads.push((key, i)),
            }
        }
        if reads.is_empty() {
            return Ok(out);
        }
        reads.sort_unstable();
        let mut uniq: Vec<&[u8]> = reads.iter().map(|r| r.0).collect();
        uniq.dedup();
        let got = match resolve {
            true => self.db.store.get_versions(&uniq, st.ts)?,
            false => self.db.store.get_versions_raw(&uniq, st.ts)?,
        };
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
            txn_deadline: self.txn_deadline,
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
            // A new lock may have waited for DDL that then committed: the statement reads the
            // catalog as it is now (LockRelationOid's AcceptInvalidationMessages).
            self.accept_catalog();
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
        self.accept_catalog();
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

    /// The heavyweight locks taken since `mark`, out of the order ROLLBACK TO releases in
    /// (still held): a parallel worker part's, released with `release_held` when it ends.
    pub fn take_locks_since(&mut self, mark: usize) -> Vec<HeldLock> {
        if self.lock_journal.len() <= mark {
            return Vec::new();
        }
        self.lock_journal.split_off(mark)
    }

    /// Release locks `take_locks_since` took out (one the transaction asked for again
    /// meanwhile, finding it held, goes too).
    pub fn release_held(&mut self, held: Vec<HeldLock>) {
        for k in held {
            if self.locks_held.remove(&k).is_some() {
                self.db.locks.release(&k.0, self.id, k.1);
            }
        }
    }

    /// Lock the row of object `oid` in system catalog `class` in `mode` until the
    /// transaction ends (heap_lock_tuple, or heap_update / heap_delete of the row when
    /// `modifies`: others' in-place updates of it then wait for us).
    pub fn lock_catalog_row(&mut self, class: u32, oid: u32, mode: RowLockMode, modifies: bool) -> Result<()> {
        let key = locks::catalog_row_key(class, oid);
        let name = catalog_name(class);
        self.lock_row(class, &key, mode, WaitPolicy::Block, &|| name.to_string(), &|| None)?;
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
                self.accept_catalog();
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

    /// Take unique key lock `key` (Exclusive, with `policy`) for this (sub)transaction's
    /// index entry for `row` (`lock`, then `hold_unique_key`). Past a few thousand of them
    /// the next plain ones (no speculative token, no row whose index expressions others
    /// evaluate) go into the transaction's compact set while nobody else holds or wants them
    /// (`bulklocks`): released with the (sub)transaction, not listed here.
    pub fn lock_unique_key(
        &mut self,
        key: &[u8],
        token: Option<u32>,
        row: Option<Arc<Vec<crate::types::Value>>>,
        policy: WaitPolicy,
        what: &dyn Fn() -> String,
        row_key: Option<&[u8]>,
    ) -> Result<bool> {
        if token.is_none()
            && row.is_none()
            && self.unique_keys_taken >= bulklocks::compact_after()
            && !self.locks_held.contains_key(&(key.to_vec(), LockMode::Exclusive))
        {
            let holder = self.holder();
            let xid = self.cur_xid();
            let proc = self.lock_proc();
            if self.db.locks.hold_compact_unique_key(key, self.id, proc, holder, xid, row_key) {
                // (As `lock` and `hold_unique_key` do: the transaction's own lock, which
                // waiting for it waits on, and its subtransaction's.)
                if !self.xact_locks.contains(&self.id) {
                    self.db.locks.hold_xact_lock(self.id, self.id, proc, self.xid);
                    self.xact_locks.push(self.id);
                }
                if !self.xact_locks.contains(&holder) {
                    self.xact_locks.push(holder);
                }
                return Ok(true);
            }
        }
        let got = self.lock(key, LockMode::Exclusive, policy, what)?;
        if got {
            self.unique_keys_taken += 1;
            self.hold_unique_key(key, token, row, row_key);
        }
        Ok(got)
    }

    /// PREPARE TRANSACTION: the compact locks move into the lock manager's main tables
    /// (`LockManager::expand_compact`) and into this transaction's lists, so they are
    /// recorded, moved and restored as any.
    pub fn expand_compact_locks(&mut self) {
        let (keys, rows) = self.db.locks.expand_compact(self.id);
        for k in keys {
            let k = (k, LockMode::Exclusive);
            if !self.locks_held.contains_key(&k) {
                self.lock_journal.push(k.clone());
                self.locks_held.insert(k, 0);
            }
        }
        self.row_locks.extend(rows.into_iter().map(|r| (r, 0)));
    }

    /// Unique key lock `key` (just taken) now stands for this (sub)transaction's index
    /// entry for `row`: others' checks of the key wait for the running (sub)transaction,
    /// or for speculative insertion `token` while it is unfinished.
    pub fn hold_unique_key(
        &mut self,
        key: &[u8],
        token: Option<u32>,
        row: Option<Arc<Vec<crate::types::Value>>>,
        row_key: Option<&[u8]>,
    ) {
        let holder = self.holder();
        let xid = self.cur_xid();
        if holder != self.id {
            self.db.locks.hold_xact_lock(holder, self.id, self.lock_proc(), xid);
            if !self.xact_locks.contains(&holder) {
                self.xact_locks.push(holder);
            }
        }
        let kh = locks::KeyHolder { holder, xid, token, row, row_key: row_key.map(<[u8]>::to_vec) };
        self.db.locks.set_key_holder(key, self.id, kh);
    }

    /// Row `row_key` (values `row`) enters exclusion constraint `con`: others' checks see
    /// the entry and wait for the running (sub)transaction (a dirty snapshot's in-progress
    /// index tuple), until it ends. (Its lock goes with the subtransaction if that aborts.)
    /// `token`: a speculative insertion's, until `settle_exclusion_entry`.
    pub fn hold_exclusion_entry(
        &mut self,
        con: u32,
        row_key: &[u8],
        row: Arc<Vec<crate::types::Value>>,
        token: Option<u32>,
    ) -> Result<()> {
        let key = locks::excl_entry_key(con, row_key);
        self.lock(&key, LockMode::Exclusive, WaitPolicy::Block, &String::new)?;
        let holder = self.holder();
        let xid = self.cur_xid();
        if holder != self.id {
            self.db.locks.hold_xact_lock(holder, self.id, self.lock_proc(), xid);
            if !self.xact_locks.contains(&holder) {
                self.xact_locks.push(holder);
            }
        }
        let kh = locks::KeyHolder { holder, xid, token, row: Some(row), row_key: Some(row_key.to_vec()) };
        self.db.locks.add_excl_entry(con, &key, row_key, self.id, kh);
        Ok(())
    }

    /// The speculative insertion that put row `row_key`'s entry into exclusion constraint
    /// `con` is complete.
    pub fn settle_exclusion_entry(&self, con: u32, row_key: &[u8]) {
        self.db.locks.settle_excl_entry(con, &locks::excl_entry_key(con, row_key));
    }

    /// The entries of exclusion constraint `con` other backends are writing: (row key,
    /// writer).
    pub fn exclusion_entries(&self, con: u32) -> Vec<(Vec<u8>, locks::KeyHolder)> {
        self.db.locks.excl_entries(con, self.lock_proc())
    }

    /// Wait for the writer of an in-progress index entry: its speculative insertion to
    /// finish (SpeculativeInsertionWait) or its (sub)transaction to end (XactLockTableWait,
    /// whose failure carries `xact_ctx`'s CONTEXT line).
    pub fn wait_for_key_holder(&mut self, kh: &locks::KeyHolder, xact_ctx: Option<String>) -> Result<()> {
        match kh.token {
            Some(token) => self.wait_for_lock(&locks::spectoken_key(kh.xid, token)),
            None => self.wait_for_lock(&locks::xact_key(kh.holder)).map_err(|e| e.inner_context(xact_ctx)),
        }
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
        // (What the transaction waited for may have committed DDL.)
        self.accept_catalog();
        Ok(())
    }

    /// The running statement reads the database's catalog as it is now, after a lock or a
    /// wait (AcceptInvalidationMessages): `catalog`'s `read_catalog`.
    pub fn accept_catalog(&mut self) {
        if self.catalog.is_none() && self.read_catalog.is_some() {
            self.read_catalog = Some(self.db.catalog());
        }
    }

    /// Begin a speculative insertion (SpeculativeInsertionLockAcquire): its token, whose lock
    /// is held until `end_speculative`.
    pub fn begin_speculative(&mut self) -> Result<u32> {
        self.spec_token = self.spec_token.wrapping_add(1).max(1);
        let token = self.spec_token;
        let xid = self.cur_xid(); // (the (sub)transaction's, as entries record it)
        self.lock(&locks::spectoken_key(xid, token), LockMode::Exclusive, WaitPolicy::Block, &String::new)?;
        Ok(token)
    }

    /// SpeculativeInsertionLockRelease.
    pub fn end_speculative(&mut self, token: u32) {
        let xid = self.cur_xid();
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
    /// lockers); `what` names the relation, `xact_ctx` is the CONTEXT line of a failed wait
    /// for a holder's transaction. Ok(false) only for SKIP LOCKED.
    pub fn lock_row(
        &mut self,
        rel: u32,
        key: &[u8],
        mode: RowLockMode,
        policy: WaitPolicy,
        what: &dyn Fn() -> String,
        xact_ctx: &dyn Fn() -> Option<String>,
    ) -> Result<bool> {
        let holder = self.holder();
        let xid = self.cur_xid();
        let tuple = locks::tuple_key(self.db.oid, rel, key);
        // Past a few thousand row locks the next ones may go compact (`bulklocks`): released
        // with the (sub)transaction, not listed here.
        let mut compacted = false;
        let compact = (self.row_locks.len() >= bulklocks::compact_after()).then_some(&mut compacted);
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
            compact,
            xact_ctx,
        )?;
        if got {
            if !self.xact_locks.contains(&holder) {
                self.xact_locks.push(holder);
            }
            if !compacted {
                self.row_locks.push((key.to_vec(), self.savepoints.len()));
            }
        }
        // (A row lock may have waited for a transaction that committed DDL: a catalog row's
        // updater.)
        self.accept_catalog();
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
        // (Nothing committed since the snapshot: no row can have changed.)
        if self.db.store.last_assigned_ts() <= self.snapshot_ts || self.buf_get(key)?.is_some() {
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
            journal_len: self.position(),
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
        self.derived.clear();
        let i = self.find_savepoint(name)?;
        if let Some(l) = &mut self.log {
            l.rolled_back();
        }
        let target = self.savepoints[i].journal_len;
        if target < self.journal_base {
            // Back past a spill: the runs lose what was written after the savepoint (first,
            // so an error leaves everything as it was), the buffer all of it.
            if let Some(r) = &mut self.runs {
                r.truncate(target as u64)?;
            }
            self.writes = BTreeMap::new();
            self.journal = Vec::new();
            self.journal_base = target;
            self.buf_bytes = 0;
        }
        let sp = &self.savepoints[i];
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
        while self.position() > target {
            let (k, prev) = self.journal.pop().unwrap();
            self.buf_bytes = self.buf_bytes.saturating_sub(2 * k.len() + 120);
            match prev {
                Some(e) => {
                    self.writes.insert(k, e);
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
        // (A statement's deferred index entry deletes go in at its end at the latest.)
        self.flush_entry_deletes()?;
        if self.sxact
            && !self.prepared
            && let Err(e) = self.db.ssi.pre_commit(self.id)
        {
            self.ssi_finish(false, 0);
            return Err(e);
        }
        // (The undo log is done with: freed before the commit copies anything.)
        drop(std::mem::take(&mut self.journal));
        let mut writes: Vec<(Vec<u8>, Option<Bytes>)> = Vec::new();
        let mut new_catalog = false;
        // A transaction that spilled streams its writes into the commit (`Store::
        // commit_sorted`: the buffer's over the runs'); `writes` then gets the catalog's only.
        let mut spilled: Option<(usize, SpilledWrites)> = None;
        match self.runs.take() {
            None => {
                writes.reserve(self.writes.len());
                for (k, e) in std::mem::take(&mut self.writes) {
                    if k.as_slice() == CATALOG_KEY {
                        new_catalog = true;
                    } else {
                        writes.push((k, e.v));
                    }
                }
            }
            Some(runs) => {
                new_catalog = self.writes.contains_key(CATALOG_KEY)
                    || match runs.get(CATALOG_KEY, LATEST) {
                        Ok(w) => w.is_some(),
                        Err(e) => {
                            self.finish_xid(false);
                            self.release_locks();
                            return Err(e);
                        }
                    };
                let keys = self.writes.len() + runs.entries();
                spilled = Some((keys, spilled_writes(std::mem::take(&mut self.writes), runs)));
            }
        }
        let catalog = RefCell::new(self.catalog.take());
        let base = self.catalog_base;
        let base_cat = self.catalog_base_cat.take();
        let start = self.start_ts;
        // (A prepared transaction checked its writes when it prepared; its row locks kept
        // them since.)
        let strict = self.isolation.txn_snapshot() && !self.prepared;
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
        // The change stream's commit record (`repl::log`): a placeholder until `check`
        // seals it under the commit lock.
        let xid = self.xid;
        // (A transaction that logged no change and changed no catalog decodes to nothing,
        // as one PostgreSQL's reorder buffer never heard of: no commit record.)
        let log = if xid != 0 { self.log.take().filter(|l| l.seq > 0 || new_catalog) } else { None };
        let repl = if log.is_some() { self.db.repl() } else { None };
        let sealed = std::cell::Cell::new(None);
        // (Decoders read a commit once it is visible: one they decode waits for its fsync, as
        // PostgreSQL's walsenders send only flushed WAL.)
        let decoded = repl.is_some();
        let placeholder = if repl.is_some() { crate::repl::log::placeholder_key() } else { Vec::new() };
        if repl.is_some() {
            writes.push((placeholder.clone(), None));
        }
        let check = |store: &Store, writes: &mut Vec<(Vec<u8>, Option<Bytes>)>| -> Result<()> {
            // Other DDL committed since this transaction's began: its catalog changes are
            // carried onto the current catalog (the commit lock is held, so it stays current).
            if let Some(base) = base
                && db.latest_catalog().version != base
            {
                let cur = db.latest_catalog();
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
            if let (Some(l), Some(r)) = (&log, &repl) {
                let (rec, lsn) = crate::repl::log::seal(&db.log, &r.wal, crate::repl::log::commit_marker(xid, l), l.bytes);
                sealed.set(Some(lsn));
                match writes.iter_mut().rev().find(|w| w.0 == placeholder) {
                    Some(w) => *w = rec,
                    None => writes.push(rec),
                }
            }
            // (Under the commit lock, before the writes are visible: `watch`.)
            db.watch.committing(writes);
            Ok(())
        };
        let db2 = self.db.clone();
        let publish = self.publish;
        // A new catalog is published under the commit lock, with the data (`check` may have
        // merged it onto the current one).
        let publish_catalog = |ts| {
            if let Some(c) = catalog.borrow_mut().take() {
                if !publish {
                    db2.last_local.fetch_max(c.version, Ordering::AcqRel);
                }
                db2.publish_catalog(ts, c);
            }
        };
        let result = if let Some((keys, mut stream)) = spilled {
            // (A snapshot transaction's conflict check, key by key as they are written.)
            let store_watch = self.db.clone();
            let store_watch = &store_watch.watch;
            let each = |store: &Store, k: &[u8]| -> Result<()> {
                if strict && store.latest_version_ts(k)?.is_some_and(|ts| ts > start) {
                    return Err(Error::new(
                        code::SERIALIZATION_FAILURE,
                        "could not serialize access due to concurrent update",
                    ));
                }
                // (Too many to log: the structures over its tables are dropped, `watch`.)
                store_watch.committing_unlogged(k);
                Ok(())
            };
            let mut catalog_records = std::mem::take(&mut writes);
            self.db.store.commit_sorted(
                keys,
                &mut stream,
                |s, extra| {
                    *extra = std::mem::take(&mut catalog_records);
                    check(s, extra)
                },
                &each,
                publish_catalog,
            )
        } else if writes.is_empty() {
            Ok(self.db.store.visible_ts())
        } else if catalog.borrow().is_some() {
            // (Group-committed: the next DDL commit merges onto this one's catalog, which is
            // published once its fsync is done.)
            let db3 = self.db.clone();
            let appended = |ts| {
                if let Some(c) = catalog.borrow().clone() {
                    *db3.pending.lock() = Some((ts, c));
                }
            };
            self.db.store.commit_grouped(writes, check, appended, publish_catalog)
        } else if let Some(delay) = self.async_commit.filter(|_| !self.prepared && !decoded) {
            // (RecordTransactionCommit: asynchronous unless it changed the catalog; COMMIT
            // PREPARED always waits.)
            self.db.store.commit_async(writes, check, delay)
        } else {
            self.db.store.commit_grouped(writes, check, |_| {}, |_| {})
        };
        // (Other commits move the WAL position by what the statistics count them writing:
        // `exec::pgstat`.)
        if let Some(lsn) = sealed.get() {
            crate::repl::log::committed(&self.db.log, lsn);
        }
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
        if self.prepared {
            self.db.locks.forget_prepared(self.id);
        }
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

    /// Take the store state `ts` as this fresh REPEATABLE READ transaction's snapshot (a
    /// logical slot's consistent point: CREATE_REPLICATION_SLOT ... (SNAPSHOT 'use')).
    pub fn use_snapshot(&mut self, ts: u64) {
        self._pin = Some(self.db.store.pin(ts));
        self.xmin = Some(self.db.xids().register_snapshot_of(Some(self.id)));
        self.snapshot_ts = ts;
        self.start_ts = ts;
        self.snapshot_fixed = true;
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

    /// The session waits for its client between statements: a READ COMMITTED transaction
    /// lets go of its last statement's snapshot (its xmin no longer holds back pruning or
    /// WaitForOlderSnapshots: Postgres's backend then has xmin 0). The caller knows no cursor
    /// or suspended portal still reads with it.
    pub fn release_statement_snapshot(&mut self) {
        if !self.isolation.txn_snapshot() {
            self.xmin = None;
        }
    }

    /// CHECK_FOR_INTERRUPTS: a cancel, or the transaction's timeout (which ends the session).
    pub fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::new(
                code::QUERY_CANCELED,
                "canceling statement due to user request",
            ));
        }
        if self.txn_deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(locks::transaction_timeout());
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

impl Snap {
    /// The store time its committed reads are at.
    pub fn ts(&self) -> u64 {
        self.st.borrow().ts
    }
}

struct SnapState {
    txn: u64,
    ts: u64,
    _pin: Option<SnapshotPin>,
    /// The journal position when taken, and how far `prev` covers the journal.
    mark: usize,
    seen: usize,
    /// The journal's base `prev` was made against (a spill moves it).
    base: usize,
    rollbacks: u64,
    /// Keys written since, with what the write buffer held for them then (None: nothing).
    prev: BTreeMap<Vec<u8>, Option<Option<Bytes>>>,
}

type BufFn = fn((Vec<u8>, Option<Bytes>)) -> Result<(Vec<u8>, Option<Bytes>)>;
type Buffered<I> = std::iter::Map<I, BufFn>;

type SpilledWrites = Box<dyn Iterator<Item = Result<(Vec<u8>, Option<Bytes>)>>>;

/// Sort items by their keys (`key`), every key sharing its first `skip` bytes (one index's):
/// by the 16 bytes after them first, with the items' places, then runs of equal ones by the
/// whole key. (The keys aren't read while they differ there: a sort of plain numbers.)
fn sort_by_key_after<T>(items: &mut Vec<T>, skip: usize, key: impl Fn(&T) -> &[u8]) {
    let word = |k: &[u8]| {
        let mut b = [0u8; 16];
        let rest = k.get(skip..).unwrap_or(&[]);
        let n = rest.len().min(16);
        b[..n].copy_from_slice(&rest[..n]);
        u128::from_be_bytes(b)
    };
    let mut order: Vec<(u128, usize)> = items.iter().enumerate().map(|(i, e)| (word(key(e)), i)).collect();
    order.sort_unstable_by_key(|o| o.0);
    for run in order.chunk_by_mut(|a, b| a.0 == b.0) {
        if run.len() > 1 {
            run.sort_unstable_by(|a, b| key(&items[a.1]).cmp(key(&items[b.1])));
        }
    }
    let mut taken: Vec<Option<T>> = std::mem::take(items).into_iter().map(Some).collect();
    items.extend(order.into_iter().map(|(_, i)| taken[i].take().expect("each item once")));
}

/// A spilled transaction's writes for its commit, in key order: each key's last, the
/// buffer's over the runs' (the catalog's stand-in left out).
fn spilled_writes(mem: BTreeMap<Vec<u8>, Ent>, runs: Box<Runs>) -> SpilledWrites {
    let mut lower = runs.scan(&[], &[], LATEST);
    let mut lower_head: Option<(Vec<u8>, Option<Bytes>)> = None;
    let mut lower_done = false;
    let mut upper = mem.into_iter().peekable();
    Box::new(std::iter::from_fn(move || {
        // (The runs' files live as long as the stream.)
        let _runs = &runs;
        loop {
            if lower_head.is_none() && !lower_done {
                match lower.next() {
                    Some(Ok(e)) => lower_head = Some(e),
                    Some(Err(e)) => return Some(Err(e)),
                    None => lower_done = true,
                }
            }
            let ord = match (upper.peek(), &lower_head) {
                (None, None) => return None,
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some((uk, _)), Some((lk, _))) => uk.cmp(lk),
            };
            let (k, v) = match ord {
                std::cmp::Ordering::Greater => lower_head.take().unwrap(),
                ord => {
                    if ord == std::cmp::Ordering::Equal {
                        lower_head = None;
                    }
                    let (k, e) = upper.next().unwrap();
                    (k, e.v)
                }
            };
            if k.as_slice() != CATALOG_KEY {
                return Some(Ok((k, v)));
            }
        }
    }))
}

/// The entries of `upper` (None: deleted) over those of `lower`, merged in key order
/// (descending when `desc`); `upper` wins a key both have.
struct Overlay<U, L> {
    upper: U,
    upper_head: Option<(Vec<u8>, Option<Bytes>)>,
    upper_done: bool,
    lower: L,
    lower_head: Option<(Vec<u8>, Bytes)>,
    lower_done: bool,
    desc: bool,
}

impl<U, L> Overlay<U, L> {
    fn new(upper: U, lower: L, desc: bool) -> Self {
        Overlay {
            upper,
            upper_head: None,
            upper_done: false,
            lower,
            lower_head: None,
            lower_done: false,
            desc,
        }
    }
}

impl<U, L> Iterator for Overlay<U, L>
where
    U: Iterator<Item = Result<(Vec<u8>, Option<Bytes>)>>,
    L: Iterator<Item = Result<(Vec<u8>, Bytes)>>,
{
    type Item = Result<(Vec<u8>, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        // (No entries of `upper` left: `lower`'s, as they come.)
        if self.upper_done && self.upper_head.is_none() {
            return match self.lower_head.take() {
                Some(e) => Some(Ok(e)),
                None if self.lower_done => None,
                None => {
                    let r = self.lower.next();
                    self.lower_done = r.is_none();
                    r
                }
            };
        }
        loop {
            if self.lower_head.is_none() && !self.lower_done {
                match self.lower.next() {
                    Some(Ok(e)) => self.lower_head = Some(e),
                    Some(Err(e)) => return Some(Err(e)),
                    None => self.lower_done = true,
                }
            }
            if self.upper_head.is_none() && !self.upper_done {
                match self.upper.next() {
                    Some(Ok(e)) => self.upper_head = Some(e),
                    Some(Err(e)) => return Some(Err(e)),
                    None => self.upper_done = true,
                }
            }
            let ord = match (&self.upper_head, &self.lower_head) {
                (None, None) => return None,
                (None, Some(_)) => return self.lower_head.take().map(Ok),
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some((uk, _)), Some((lk, _))) if self.desc => lk.cmp(uk),
                (Some((uk, _)), Some((lk, _))) => uk.cmp(lk),
            };
            match ord {
                std::cmp::Ordering::Greater => return self.lower_head.take().map(Ok),
                std::cmp::Ordering::Equal => self.lower_head = None,
                std::cmp::Ordering::Less => {}
            }
            let (k, v) = self.upper_head.take().unwrap();
            if let Some(v) = v {
                return Some(Ok((k, v)));
            }
        }
    }
}

/// What a scan merges the buffer over: the store, or a transaction's spilled writes over it.
enum Lower {
    Store(crate::storage::Scan),
    Runs(Box<Overlay<spill::RunScan, crate::storage::Scan>>),
}

impl Iterator for Lower {
    type Item = Result<(Vec<u8>, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Lower::Store(s) => s.next(),
            Lower::Runs(o) => o.next(),
        }
    }
}

/// `Lower` in descending key order.
enum RevLower {
    Store(crate::storage::RevScan),
    Runs(Box<Overlay<spill::RunRevScan, crate::storage::RevScan>>),
}

impl Iterator for RevLower {
    type Item = Result<(Vec<u8>, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            RevLower::Store(s) => s.next(),
            RevLower::Runs(o) => o.next(),
        }
    }
}

/// `TxnScan` in descending key order.
pub struct TxnRevScan {
    inner: Overlay<Buffered<std::iter::Rev<std::collections::btree_map::IntoIter<Vec<u8>, Option<Bytes>>>>, RevLower>,
    src: Option<Arc<toast::Source>>,
}

impl Iterator for TxnRevScan {
    type Item = Result<(Vec<u8>, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next()?;
        Some(match &self.src {
            Some(s) => item.and_then(|(k, v)| s.resolve(&k, v).map(|v| (k, v))),
            None => item,
        })
    }
}

/// Which of a transaction's writes a reader's out-of-line values come from
/// (`Txn::toast_source`).
enum OwnVals<'a> {
    /// None of the buffer (a snapshot from before the last spill: the runs as of its mark).
    Runs,
    /// The buffer as it is now.
    Now,
    /// The buffer as snapshot `st` sees it.
    Snap(&'a SnapState),
    /// Writes captured earlier (`writes_in`).
    Map(&'a BTreeMap<Vec<u8>, Option<Bytes>>),
}

/// Merge of buffered writes over a store scan (and the spilled writes); buffered entries
/// win. Rows come with their out-of-line values put back, unless `raw`.
pub struct TxnScan {
    inner: Overlay<Buffered<std::vec::IntoIter<(Vec<u8>, Option<Bytes>)>>, Lower>,
    /// Where the rows' out-of-line values come from (None: no row in the range).
    src: Option<Arc<toast::Source>>,
    resolve: bool,
    /// The values read so far (made at the first row that has some).
    reader: Option<toast::Reader>,
}

impl TxnScan {
    fn new(buffered: Vec<(Vec<u8>, Option<Bytes>)>, lower: Lower, src: Option<Arc<toast::Source>>) -> TxnScan {
        TxnScan {
            inner: Overlay::new(buffered.into_iter().map(Ok as BufFn), lower, false),
            src,
            resolve: true,
            reader: None,
        }
    }

    /// Rows as they are stored: a reader that decodes some columns puts back their values
    /// itself, from `source`.
    pub fn raw(mut self) -> TxnScan {
        self.resolve = false;
        self
    }

    /// Where the rows' out-of-line values come from (None: no row in the range).
    pub fn source(&self) -> Option<&Arc<toast::Source>> {
        self.src.as_ref()
    }
}

impl Iterator for TxnScan {
    type Item = Result<(Vec<u8>, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next()?;
        if !self.resolve {
            return Some(item);
        }
        // (The rows' values in their order, through one cursor: `toast::Reader`.)
        Some(item.and_then(|(k, v)| match &self.src {
            Some(s) if crate::storage::toast::has_external(&v) && crate::storage::toast::is_row_key(&k) => {
                let r = self.reader.get_or_insert_with(|| toast::Reader::new(s.clone()));
                r.splice(&k, &v, None).map(|v| (k, v))
            }
            _ => Ok((k, v)),
        }))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sort_by_key_after_sorts_as_the_keys() {
        // Keys sharing a 3-byte prefix, then tails short, long, equal for 8 bytes, padded.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = (0..5000u32)
            .map(|i| {
                let mut k = b"pfx".to_vec();
                let len = (next() % 14) as usize;
                k.extend((0..len).map(|_| (next() % 3) as u8));
                (k, i.to_be_bytes().to_vec())
            })
            .collect();
        let mut want = entries.clone();
        want.sort();
        super::sort_by_key_after(&mut entries, 3, |e| e.0.as_slice());
        let keys = |v: &[(Vec<u8>, Vec<u8>)]| v.iter().map(|e| e.0.clone()).collect::<Vec<_>>();
        assert_eq!(keys(&entries), keys(&want));
        entries.sort();
        assert_eq!(entries, want);
    }
}
