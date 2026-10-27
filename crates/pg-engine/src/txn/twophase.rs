//! Two-phase commit (twophase.c): PREPARE TRANSACTION detaches a transaction from its session
//! as a prepared transaction, which keeps its transaction id running, its locks (moved to a
//! dummy backend without a pid, `LockManager::transfer`) and its writes until COMMIT PREPARED
//! or ROLLBACK PREPARED from any session of its database.
//!
//! The registry is the cluster's (gids are unique across databases, pg_prepared_xacts lists
//! all of them), in Postgres's array order. A prepared transaction's state record (its writes,
//! catalog changes, locks, predicate locks) is stored in its database's store under
//! `PREFIX` when it prepares, and removed in the commit of COMMIT PREPARED's writes or by
//! ROLLBACK PREPARED, so it survives a restart or crash: `Database::recover_prepared` rebuilds
//! the transaction from it, its locks taken again before any session can connect.

use super::locks::{LockMode, RowLockMode, WaitPolicy, Waits};
use super::{Database, Isolation, Txn};
use crate::catalog::persist;
use crate::error::{Error, Result, code, internal};
use bytes::Bytes;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// GIDSIZE: a gid is shorter than this, in bytes.
pub const GIDSIZE: usize = 200;

/// Reserved keyspace (table oid 0) of prepared transactions' state records, by gid.
pub const PREFIX: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0x20];
const PREFIX_END: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0x21];

pub fn record_key(gid: &str) -> Vec<u8> {
    let mut k = PREFIX.to_vec();
    k.extend_from_slice(gid.as_bytes());
    k
}

pub fn record_range() -> (&'static [u8], &'static [u8]) {
    (PREFIX, PREFIX_END)
}

/// A prepared transaction as pg_prepared_xact() shows it.
#[derive(Clone, Debug)]
pub struct Info {
    pub xid: u64,
    pub gid: String,
    /// When it prepared (timestamptz microseconds).
    pub prepared_at: i64,
    /// The role that prepared it (GetUserId()).
    pub owner: u32,
    pub db: u32,
}

struct Gxact {
    info: Info,
    /// Marked prepared (gxact->valid): PREPARE finished.
    valid: bool,
    /// Locked by a backend finishing it, or still being prepared (locking_backend).
    busy: bool,
    txn: Option<Txn>,
}

/// The cluster's prepared transactions (TwoPhaseState).
pub struct TwoPhase {
    /// max_prepared_transactions.
    max: usize,
    list: Mutex<Vec<Gxact>>,
    /// Called with a database's oid before its first state record is stored (the engine
    /// records which databases to recover at startup).
    on_prepare: Box<dyn Fn(u32) + Send + Sync>,
}

impl TwoPhase {
    pub fn new(max: usize, on_prepare: impl Fn(u32) + Send + Sync + 'static) -> TwoPhase {
        TwoPhase { max, list: Mutex::new(Vec::new()), on_prepare: Box::new(on_prepare) }
    }

    /// A database's own (outside an engine): disabled, as the default setting is.
    pub fn local() -> TwoPhase {
        TwoPhase::new(0, |_| {})
    }

    pub fn max(&self) -> usize {
        self.max
    }

    /// MarkAsPreparing: reserve `info.gid` for a transaction about to prepare.
    pub fn reserve(&self, info: Info) -> Result<()> {
        if info.gid.len() >= GIDSIZE {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("transaction identifier \"{}\" is too long", info.gid),
            ));
        }
        if self.max == 0 {
            return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "prepared transactions are disabled")
                .hint("Set \"max_prepared_transactions\" to a nonzero value."));
        }
        let mut l = self.list.lock();
        if l.iter().any(|g| g.info.gid == info.gid) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("transaction identifier \"{}\" is already in use", info.gid),
            ));
        }
        if l.len() >= self.max {
            return Err(Error::new(code::OUT_OF_MEMORY, "maximum number of prepared transactions reached")
                .hint(format!("Increase \"max_prepared_transactions\" (currently {}).", self.max)));
        }
        let db = info.db;
        l.push(Gxact { info, valid: false, busy: true, txn: None });
        drop(l);
        (self.on_prepare)(db);
        Ok(())
    }

    /// MarkAsPrepared and PostPrepare_Twophase: `gid` is prepared, held by nobody.
    pub fn prepared(&self, gid: &str, txn: Txn) {
        let mut l = self.list.lock();
        if let Some(g) = l.iter_mut().find(|g| g.info.gid == gid) {
            g.valid = true;
            g.busy = false;
            g.txn = Some(txn);
        }
    }

    /// RemoveGXact: the entry goes, the last one taking its place.
    pub fn remove(&self, gid: &str) {
        let mut l = self.list.lock();
        if let Some(i) = l.iter().position(|g| g.info.gid == gid) {
            l.swap_remove(i);
        }
    }

    /// LockGXact: take prepared transaction `gid` to finish it, as role `user` (`superuser`:
    /// whether it is one) of database `db`.
    pub fn lock(&self, gid: &str, user: u32, superuser: bool, db: u32) -> Result<(Info, Txn)> {
        let mut l = self.list.lock();
        let Some(g) = l.iter_mut().find(|g| g.valid && g.info.gid == gid) else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("prepared transaction with identifier \"{gid}\" does not exist"),
            ));
        };
        if g.busy {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("prepared transaction with identifier \"{gid}\" is busy"),
            ));
        }
        if user != g.info.owner && !superuser {
            return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "permission denied to finish prepared transaction")
                .hint("Must be superuser or the user that prepared the transaction."));
        }
        if db != g.info.db {
            return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "prepared transaction belongs to another database")
                .hint("Connect to the database where the transaction was prepared to finish it."));
        }
        let txn = g.txn.take().ok_or_else(|| internal("prepared transaction without its state"))?;
        g.busy = true;
        Ok((g.info.clone(), txn))
    }

    /// A recovered prepared transaction (at startup).
    pub fn recovered(&self, info: Info, txn: Txn) {
        let mut l = self.list.lock();
        l.retain(|g| g.info.gid != info.gid);
        l.push(Gxact { info, valid: true, busy: false, txn: Some(txn) });
    }

    /// pg_prepared_xact(): the prepared ones, in the registry's order.
    pub fn list(&self) -> Vec<Info> {
        self.list.lock().iter().filter(|g| g.valid).map(|g| g.info.clone()).collect()
    }

    /// How many prepared transactions database `db` has (CountOtherDBBackends'
    /// nprepared).
    pub fn in_database(&self, db: u32) -> usize {
        self.list.lock().iter().filter(|g| g.info.db == db).count()
    }

    /// The server stops: the prepared transactions go from memory (their state records stay).
    pub fn clear(&self) {
        let gone = std::mem::take(&mut *self.list.lock());
        drop(gone);
    }
}

/// Little-endian length-prefixed encoding of a state record.
#[derive(Default)]
pub struct Enc(pub Vec<u8>);

impl Enc {
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.u64(b.len() as u64);
        self.0.extend_from_slice(b);
    }
    pub fn opt_bytes(&mut self, b: Option<&[u8]>) {
        match b {
            Some(b) => {
                self.u8(1);
                self.bytes(b);
            }
            None => self.u8(0),
        }
    }
}

pub struct Dec<'a> {
    b: &'a [u8],
    at: usize,
}

fn corrupt() -> Error {
    internal("corrupt prepared transaction state")
}

impl<'a> Dec<'a> {
    pub fn new(b: &'a [u8]) -> Dec<'a> {
        Dec { b, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self.b.get(self.at..self.at + n).ok_or_else(corrupt)?;
        self.at += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u64()? as usize;
        self.take(n)
    }
    pub fn opt_bytes(&mut self) -> Result<Option<&'a [u8]>> {
        Ok(match self.u8()? {
            0 => None,
            _ => Some(self.bytes()?),
        })
    }
    pub fn str(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| corrupt())
    }
}

const MAGIC: &[u8] = b"R2PC\x01";

/// Past every key (a key's first bytes are its relation's oid).
const ALL_END: &[u8] = &[0xff; 16];

impl Txn {
    /// Whether this is a prepared transaction.
    pub fn is_prepared(&self) -> bool {
        self.prepared
    }

    /// It exported snapshots (XactHasExportedSnapshots).
    pub fn has_exported_snapshots(&self) -> bool {
        self.exported
    }

    /// COMMIT PREPARED: its state record goes in the commit of its writes.
    pub fn remove_state_record(&mut self, gid: &str) {
        self.record(record_key(gid), None);
    }

    /// The checks a commit makes that a prepared transaction's commit mustn't fail on, run
    /// as PREPARE does: PreCommit_CheckForSerializationFailure, and REPEATABLE READ /
    /// SERIALIZABLE's check that no write of it was overtaken by a concurrent commit.
    pub fn prepare_checks(&mut self) -> Result<()> {
        // (Compact locks are recorded and moved as the rest: `expand_compact_locks`.)
        self.expand_compact_locks();
        if self.sxact
            && let Err(e) = self.db.ssi.pre_commit(self.id)
        {
            self.ssi_finish(false, 0);
            return Err(e);
        }
        if self.isolation.txn_snapshot() {
            for k in self.writes_in(&[], ALL_END)?.keys().filter(|k| k.as_slice() != super::CATALOG_KEY) {
                if self.db.store.latest_version_ts(k)?.is_some_and(|ts| ts > self.start_ts) {
                    return Err(Error::new(
                        code::SERIALIZATION_FAILURE,
                        "could not serialize access due to concurrent update",
                    ));
                }
            }
        }
        Ok(())
    }

    /// The transaction ids of its subtransactions (they end as it does).
    fn sub_xids(&self) -> Vec<u64> {
        self.sub_committed
            .iter()
            .copied()
            .chain(self.savepoints.iter().flat_map(|sp| sp.committed.iter().copied().chain([sp.xid])))
            .filter(|&x| x != 0)
            .collect()
    }

    /// The state record of this transaction prepared as `info` (EndPrepare's state file).
    pub fn prepared_record(&self, info: &Info) -> Result<Vec<u8>> {
        let mut e = Enc::default();
        e.0.extend_from_slice(MAGIC);
        e.bytes(info.gid.as_bytes());
        e.u64(info.xid);
        e.u64(info.prepared_at as u64);
        e.u32(info.owner);
        e.u32(info.db);
        e.bytes(self.isolation.name().as_bytes());
        e.u8(self.read_only as u8);
        let subs = self.sub_xids();
        e.u64(subs.len() as u64);
        subs.iter().for_each(|x| e.u64(*x));
        // (Its spilled writes too.)
        let all = self.writes_in(&[], ALL_END)?;
        let writes: Vec<_> = all.iter().filter(|(k, _)| k.as_slice() != super::CATALOG_KEY).collect();
        e.u64(writes.len() as u64);
        for (k, v) in writes {
            e.bytes(k);
            e.opt_bytes(v.as_deref());
        }
        e.u64(self.locks_held.len() as u64);
        for (k, m) in self.locks_held.keys() {
            e.bytes(k);
            e.u8(*m as u8);
        }
        let rows = self.db.locks.rows_of(self.id);
        e.u64(rows.len() as u64);
        for (row, mode, modifies) in rows {
            e.bytes(&row);
            e.u8(mode as u8);
            e.u8(modifies as u8);
        }
        let keys = self.db.locks.key_holders_of(self.id);
        e.u64(keys.len() as u64);
        for (k, xid) in keys {
            e.bytes(&k);
            e.u64(xid);
        }
        let json = |v: &serde_json::Map<String, serde_json::Value>| {
            serde_json::to_vec(v).map_err(|e| internal(e.to_string()))
        };
        match (&self.catalog_base_cat, &self.catalog) {
            (Some(base), Some(mine)) if all.contains_key(super::CATALOG_KEY) => {
                e.u8(1);
                e.bytes(&json(&persist::header_fields(base)?)?);
                e.bytes(&json(&persist::header_fields(mine)?)?);
                let changes = persist::record_changes(base, mine)?;
                e.u64(changes.len() as u64);
                for (tag, oid, v) in changes {
                    e.u8(tag);
                    e.u32(oid);
                    e.opt_bytes(v.as_deref());
                }
            }
            _ => e.u8(0),
        }
        match self.sxact.then(|| self.db.ssi.prepared_state(self.id)).flatten() {
            Some((locks, writes)) => {
                e.u8(1);
                e.u64(locks.len() as u64);
                for (rel, page, tuple) in locks {
                    e.u32(rel);
                    e.opt_bytes(page.map(u32::to_le_bytes).as_ref().map(|b| &b[..]));
                    e.opt_bytes(tuple.map(u16::to_le_bytes).as_ref().map(|b| &b[..]));
                }
                e.u64(writes.len() as u64);
                for (rel, key) in writes {
                    e.u32(rel);
                    e.bytes(&key);
                }
            }
            None => e.u8(0),
        }
        Ok(e.0)
    }

    /// PostPrepare: the transaction belongs to no session any more. Its locks move to its
    /// own dummy backend, its snapshot goes (a prepared transaction holds none), and its
    /// commit later skips the checks `prepare_checks` made.
    pub fn detach_prepared(&mut self) {
        self.prepared = true;
        self.db.locks.transfer(self.id, self.id);
        self.lock_peer = None;
        if self.sxact {
            self.db.ssi.detach(self.id);
        }
        self._pin = None;
        self.xmin = None;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.lock_timeout = None;
        self.deadline = None;
        self.lo_descs.clear();
        self.stmt_catalog = None;
        drop(std::mem::take(&mut self.journal));
    }
}

/// RecoverPreparedTransactions: the prepared transaction state record `b` describes, made
/// again on `db` (just opened, before any session): its id running, its writes and catalog
/// changes buffered, its locks and predicate locks held by its dummy backend. None: the
/// record is another database's (a copy of its files).
pub fn recover(db: &Arc<Database>, b: &[u8]) -> Result<Option<(Info, Txn)>> {
    let mut d = Dec::new(b);
    if d.take(MAGIC.len())? != MAGIC {
        return Err(corrupt());
    }
    let info = Info {
        gid: d.str()?,
        xid: d.u64()?,
        prepared_at: d.u64()? as i64,
        owner: d.u32()?,
        db: d.u32()?,
    };
    if info.db != db.oid {
        return Ok(None);
    }
    let iso = Isolation::parse(&d.str()?).ok_or_else(corrupt)?;
    let read_only = d.u8()? != 0;
    let mut t = db.begin(iso, read_only, Arc::new(AtomicBool::new(false)));
    t._pin = None;
    t.xid = info.xid;
    t.prepared = true;
    t.wrote = true;
    let subs: Vec<u64> = (0..d.u64()?).map(|_| d.u64()).collect::<Result<_>>()?;
    db.xids().recover_running(info.xid, &subs);
    t.sub_committed = subs;
    for _ in 0..d.u64()? {
        let k = d.bytes()?.to_vec();
        let v = d.opt_bytes()?.map(Bytes::copy_from_slice);
        t.record(k, v);
    }
    let id = t.id;
    let none = AtomicBool::new(false);
    let nowait = Waits::new(WaitPolicy::NoWait, &none);
    for _ in 0..d.u64()? {
        let k = d.bytes()?.to_vec();
        let mode = LockMode::from_number(d.u8()? as i32).ok_or_else(corrupt)?;
        db.locks.acquire(&k, id, id, mode, &nowait, &String::new)?;
        t.lock_journal.push((k.clone(), mode));
        t.locks_held.insert((k, mode), 0);
    }
    db.locks.hold_xact_lock(id, id, id, info.xid);
    t.xact_locks.push(id);
    for _ in 0..d.u64()? {
        let row = d.bytes()?.to_vec();
        let mode = match d.u8()? {
            0 => RowLockMode::KeyShare,
            1 => RowLockMode::Share,
            2 => RowLockMode::NoKeyUpdate,
            _ => RowLockMode::Update,
        };
        let modifies = d.u8()? != 0;
        db.locks.restore_row(&row, id, id, mode, modifies);
        t.row_locks.push((row, 0));
    }
    for _ in 0..d.u64()? {
        let k = d.bytes()?.to_vec();
        let xid = d.u64()?;
        db.locks.set_key_holder(&k, id, super::locks::KeyHolder { holder: id, xid, token: None, row: None, row_key: None });
    }
    db.locks.transfer(id, id);
    if d.u8()? != 0 {
        let map = |b: &[u8]| -> Result<serde_json::Map<String, serde_json::Value>> {
            serde_json::from_slice(b).map_err(|_| corrupt())
        };
        let hb = map(d.bytes()?)?;
        let hm = map(d.bytes()?)?;
        let mut changes = Vec::new();
        for _ in 0..d.u64()? {
            let tag = d.u8()?;
            let oid = d.u32()?;
            changes.push((tag, oid, d.opt_bytes()?.map(Bytes::copy_from_slice)));
        }
        let cur = db.catalog();
        let mut mine = super::catmerge::reapply(&cur, &hb, &hm, changes)?;
        mine.bump();
        mine.freeze();
        t.catalog = Some(Arc::new(mine));
        t.catalog_base = Some(cur.version);
        t.catalog_base_cat = Some(cur);
        t.record(super::CATALOG_KEY.to_vec(), Some(Bytes::new()));
    }
    if d.u8()? != 0 {
        let mut locks = Vec::new();
        for _ in 0..d.u64()? {
            let rel = d.u32()?;
            let page = d.opt_bytes()?.map(|b| u32::from_le_bytes(b.try_into().unwrap_or([0; 4])));
            let tuple = d.opt_bytes()?.map(|b| u16::from_le_bytes(b.try_into().unwrap_or([0; 2])));
            locks.push((rel, page, tuple));
        }
        let mut writes = Vec::new();
        for _ in 0..d.u64()? {
            let rel = d.u32()?;
            writes.push((rel, d.bytes()?.to_vec()));
        }
        if iso == Isolation::Serializable {
            db.ssi.recover_prepared(id, info.xid, locks.clone(), writes);
            t.sxact = true;
            t.si_locks = locks;
        }
    }
    Ok(Some((info, t)))
}
