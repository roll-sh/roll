//! The change stream's capture side. While a database has logical replication slots, every
//! transaction that gets a transaction id there logs its row changes as it makes them:
//! records in its own write buffer under `PREFIX` (so they spill, roll back to savepoints
//! and commit atomically with the data, as the rest of its writes do), plus, at commit, a
//! commit record holding the transaction's WAL positions (`seal`), taken from the cluster's
//! counter under the store's commit lock so a database's commit records are in commit
//! order. Decoding (`decode`) reads commit records in position order and each one's change
//! records; what every slot has confirmed is removed (`gc_writes`). Without a slot nothing
//! is logged, and a commit only moves the WAL position on.

use crate::catalog::Table;
use crate::error::Result;
use crate::exec::ExecCtx;
use crate::storage::row::encode_row;
use crate::storage::sst::put_varint;
use crate::types::{DataType, Value};
use bytes::Bytes;
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

/// Reserved keyspace (table oid 0) of the change stream.
pub const PREFIX: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0x30];
pub const PREFIX_END: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0x31];
/// Change records, by top-level xid and order: `PREFIX 'X' xid seq`.
const CHANGES: u8 = b'X';
/// Commit (and prepare, message) records, by commit position: `PREFIX 'C' lsn`.
const COMMITS: u8 = b'C';
/// What a commit record takes of the WAL after the changes (a commit record's size).
pub const COMMIT_SIZE: u64 = 48;

pub fn change_key(xid: u64, seq: u32) -> Vec<u8> {
    let mut k = PREFIX.to_vec();
    k.push(CHANGES);
    k.extend_from_slice(&xid.to_be_bytes());
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

pub fn changes_range(xid: u64) -> (Vec<u8>, Vec<u8>) {
    let mut a = PREFIX.to_vec();
    a.push(CHANGES);
    a.extend_from_slice(&xid.to_be_bytes());
    let mut b = a.clone();
    b.extend_from_slice(&[0xff; 5]);
    (a, b)
}

pub fn is_log_key(k: &[u8]) -> bool {
    k.starts_with(PREFIX)
}

pub fn commit_key(lsn: u64) -> Vec<u8> {
    let mut k = PREFIX.to_vec();
    k.push(COMMITS);
    k.extend_from_slice(&lsn.to_be_bytes());
    k
}

/// The key a transaction's commit record has until its position is known (`seal`).
pub fn placeholder_key() -> Vec<u8> {
    commit_key(0)
}

/// The WAL a change record of `len` bytes takes (a record header and the data, aligned).
pub fn record_size(len: usize) -> u64 {
    super::lsn::align(len as u64 + 24)
}

/// A relation as the change stream describes it (what a decoding plugin learns from the
/// relation cache with a historic snapshot): logged before a transaction's first change of
/// it, and again when its definition changed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RelDesc {
    pub oid: u32,
    pub nsp: String,
    pub name: String,
    /// relreplident: 'd', 'n', 'f' or 'i'.
    pub replident: char,
    pub cols: Vec<ColDesc>,
    /// The partition's ancestors, nearest first (publish_via_partition_root).
    #[serde(default)]
    pub ancestors: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ColDesc {
    pub name: String,
    pub ty: DataType,
    pub type_oid: u32,
    pub typmod: i32,
    /// format_type_be of the type (test_decoding's `[type]`).
    pub typname: String,
    /// The type's schema and pg_type name (pgoutput's Type message).
    #[serde(default)]
    pub type_nsp: String,
    #[serde(default)]
    pub type_name: String,
    pub dropped: bool,
    /// attgenerated: '\0', 's' or 'v'.
    pub generated: char,
    /// Part of the replica identity (every column with REPLICA IDENTITY FULL).
    pub key: bool,
}

/// A logged change, decoded.
#[derive(Clone, Debug)]
pub enum Rec {
    Rel(Arc<RelDesc>),
    Insert { rel: u32, new: Vec<Value> },
    Update { rel: u32, old: Option<(Vec<Value>, bool)>, new: Vec<Value>, unchanged_toast: Vec<usize> },
    Delete { rel: u32, old: Option<(Vec<Value>, bool)> },
    Truncate { cascade: bool, restart_seqs: bool, rels: Vec<u32> },
    Message { transactional: bool, prefix: String, content: Vec<u8> },
}

/// A transaction's logging state (in `Txn`).
#[derive(Default)]
pub struct TxnLog {
    /// The next change record's number.
    pub seq: u32,
    /// The WAL the records written so far take (an upper bound once some rolled back).
    pub bytes: u64,
    /// The relations described so far, with their definitions' hashes.
    rels: HashMap<u32, u64>,
    /// The replication origin the session set up (pg_replication_origin_xact_setup).
    pub origin: u16,
    pub origin_lsn: u64,
    /// It prepared (PREPARE TRANSACTION): what its commit record repeats.
    pub prepared: Option<Prepared>,
}

/// A prepared transaction's PREPARE, as its COMMIT / ROLLBACK PREPARED record repeats it.
#[derive(Clone, Debug, Default)]
pub struct Prepared {
    pub gid: String,
    pub time: i64,
    pub end_lsn: u64,
}

pub fn now() -> i64 {
    crate::types::datetime::Clock::system().now
}

/// The commit record of a transaction ending with `log` (COMMIT PREPARED for a prepared one).
pub fn commit_marker(xid: u64, log: &TxnLog) -> Marker {
    let kind = if log.prepared.is_some() { Kind::CommitPrepared } else { Kind::Commit };
    let mut m = Marker::new(kind, xid, now());
    m.origin = log.origin;
    m.origin_lsn = log.origin_lsn;
    if let Some(p) = &log.prepared {
        m.gid = p.gid.clone();
        m.prepare_time = p.time;
        m.prepare_end_lsn = p.end_lsn;
    }
    m
}

impl TxnLog {
    /// Back past a savepoint: relation descriptions written since may be gone.
    pub fn rolled_back(&mut self) {
        self.rels.clear();
    }
}

/// What a commit record says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Commit = b'C' as isize,
    Prepare = b'P' as isize,
    CommitPrepared = b'K' as isize,
    RollbackPrepared = b'A' as isize,
    /// A non-transactional message (its record holds it).
    Message = b'M' as isize,
}

#[derive(Clone, Debug)]
pub struct Marker {
    pub kind: Kind,
    pub xid: u64,
    /// The first change record's position.
    pub first_lsn: u64,
    /// The commit record's position (its key).
    pub commit_lsn: u64,
    /// Its end (where the next transaction can start).
    pub end_lsn: u64,
    /// Commit (or prepare) time, timestamptz microseconds.
    pub time: i64,
    pub origin: u16,
    pub origin_lsn: u64,
    pub gid: String,
    /// When the transaction prepared (CommitPrepared / RollbackPrepared), and its end.
    pub prepare_time: i64,
    pub prepare_end_lsn: u64,
    /// The message of a `Message` record.
    pub message: Option<Vec<u8>>,
}

impl Marker {
    pub fn new(kind: Kind, xid: u64, time: i64) -> Marker {
        Marker {
            kind,
            xid,
            first_lsn: 0,
            commit_lsn: 0,
            end_lsn: 0,
            time,
            origin: 0,
            origin_lsn: 0,
            gid: String::new(),
            prepare_time: 0,
            prepare_end_lsn: 0,
            message: None,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![self.kind as u8];
        for v in [self.xid, self.first_lsn, self.commit_lsn, self.end_lsn, self.time as u64] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.extend_from_slice(&self.origin.to_be_bytes());
        b.extend_from_slice(&self.origin_lsn.to_be_bytes());
        b.extend_from_slice(&(self.prepare_time as u64).to_be_bytes());
        b.extend_from_slice(&self.prepare_end_lsn.to_be_bytes());
        put_varint(&mut b, self.gid.len() as u64);
        b.extend_from_slice(self.gid.as_bytes());
        if let Some(m) = &self.message {
            b.extend_from_slice(m);
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Marker> {
        let bad = || crate::error::internal("bad change stream commit record");
        let u64_at = |p: usize| -> Result<u64> {
            Ok(u64::from_be_bytes(b.get(p..p + 8).ok_or_else(bad)?.try_into().unwrap()))
        };
        let kind = match b.first() {
            Some(b'C') => Kind::Commit,
            Some(b'P') => Kind::Prepare,
            Some(b'K') => Kind::CommitPrepared,
            Some(b'A') => Kind::RollbackPrepared,
            Some(b'M') => Kind::Message,
            _ => return Err(bad()),
        };
        let origin = u16::from_be_bytes(b.get(41..43).ok_or_else(bad)?.try_into().unwrap());
        let mut p = 67;
        let n = crate::storage::sst::get_varint(b, &mut p)? as usize;
        let gid = String::from_utf8_lossy(b.get(p..p + n).ok_or_else(bad)?).into_owned();
        p += n;
        Ok(Marker {
            kind,
            xid: u64_at(1)?,
            first_lsn: u64_at(9)?,
            commit_lsn: u64_at(17)?,
            end_lsn: u64_at(25)?,
            time: u64_at(33)? as i64,
            origin,
            origin_lsn: u64_at(43)?,
            prepare_time: u64_at(51)? as i64,
            prepare_end_lsn: u64_at(59)?,
            gid,
            message: (kind == Kind::Message).then(|| b[p..].to_vec()),
        })
    }
}

/// A database's change stream state.
#[derive(Default)]
pub struct DbLog {
    /// Transactions log their changes (the database has logical slots, or one is being
    /// made).
    pub capture: AtomicBool,
    /// track_commit_timestamp is on (`commit_ts`): the one thing a commit checks for it.
    pub commit_ts: AtomicBool,
    /// Commit positions taken whose commits aren't visible yet.
    pending: Mutex<BTreeSet<u64>>,
    /// Commits logged so far (for decoders waiting for more), signalled on each.
    commits: Mutex<u64>,
    committed: Condvar,
    /// Relation descriptions made, by relation: (definition hash, record).
    descs: Mutex<HashMap<u32, (u64, Arc<Vec<u8>>)>>,
    /// Records ending at or before this position were removed (`gc_writes`).
    pub gc_upto: AtomicU64,
    /// What the database's decoders share (`decode::Shared`).
    pub decoded: super::decode::Shared,
}

impl DbLog {
    /// Positions below this belong to commits that are visible (or to none of this
    /// database's): how far a decoder may read.
    pub fn safe_end(&self, wal: &super::lsn::WalPos) -> u64 {
        let p = self.pending.lock();
        p.first().copied().unwrap_or_else(|| wal.current())
    }

    /// Wait up to `timeout` for a commit after the `seen`th; the count now.
    pub fn wait_commit(&self, seen: u64, timeout: std::time::Duration) -> u64 {
        let mut n = self.commits.lock();
        if *n == seen {
            self.committed.wait_for(&mut n, timeout);
        }
        *n
    }

    pub fn commit_count(&self) -> u64 {
        *self.commits.lock()
    }

    /// Wake decoders (a slot was dropped or the database is closing, too).
    pub fn wake(&self) {
        let mut n = self.commits.lock();
        *n += 1;
        self.committed.notify_all();
    }
}

/// Seal a transaction's log: its commit record, at positions taken now (under the store's
/// commit lock, so in this database's commit order). `committed` must follow, whatever
/// becomes of the commit. Returns the record and its position.
pub fn seal(log: &DbLog, wal: &super::lsn::WalPos, mut m: Marker, bytes: u64) -> ((Vec<u8>, Option<Bytes>), u64) {
    let mut p = log.pending.lock();
    let extra = m.message.as_ref().map_or(0, |b| b.len() as u64);
    let base = wal.alloc(bytes + COMMIT_SIZE + extra);
    m.first_lsn = base;
    m.commit_lsn = base + bytes;
    m.end_lsn = super::lsn::align(m.commit_lsn + COMMIT_SIZE + extra);
    p.insert(m.commit_lsn);
    ((commit_key(m.commit_lsn), Some(Bytes::from(m.encode()))), m.commit_lsn)
}

/// The commit sealed at `lsn` is visible (or failed): decoders may read past it.
pub fn committed(log: &DbLog, lsn: u64) {
    log.pending.lock().remove(&lsn);
    log.wake();
}

/// Whether changes to `t` are logged (RelationIsLogicallyLogged): a permanent user table.
fn logged(t: &Table) -> bool {
    !t.temporary
        && !t.unlogged
        && t.foreign.is_none()
        && t.oid >= crate::catalog::FIRST_USER_OID
        && t.partkey.is_none()
}

fn hash_def(t: &Table, cat: &crate::catalog::Catalog) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    t.name.hash(&mut h);
    t.schema.hash(&mut h);
    t.replident.hash(&mut h);
    for c in &t.columns {
        c.name.hash(&mut h);
        c.ty.hash(&mut h);
        c.dropped.hash(&mut h);
        c.domain.hash(&mut h);
        c.generated.is_some().hash(&mut h);
        c.virtual_gen.hash(&mut h);
    }
    cat.identity_key(t).hash(&mut h);
    t.inherits.hash(&mut h);
    h.finish()
}

fn describe(t: &Table, ctx: &ExecCtx) -> RelDesc {
    describe_in(t, &ctx.catalog(), &ctx.search_path)
}

/// A relation's description under catalog `cat` (`search_path`: how test_decoding's type
/// names are qualified).
pub fn describe_in(t: &Table, cat: &crate::catalog::Catalog, search_path: &[String]) -> RelDesc {
    let key = cat.identity_key(t).unwrap_or_default();
    let cols = t
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let plain = crate::storage::row::with_typmod(c.ty.clone(), -1);
            let type_oid = c.domain.unwrap_or_else(|| crate::catalog::system::type_oid(cat, &c.ty));
            let (type_nsp, type_name) = match cat.user_type_name(type_oid) {
                Some((nsp, name, array)) => {
                    (cat.schema_name(nsp).to_string(), if array { format!("_{name}") } else { name.to_string() })
                }
                None => ("pg_catalog".to_string(), plain.typname()),
            };
            ColDesc {
                name: c.name.clone(),
                type_oid,
                typmod: c.ty.typmod(),
                typname: cat.format_type(&plain, c.domain, search_path),
                type_nsp,
                type_name,
                ty: c.ty.clone(),
                dropped: c.dropped,
                generated: if c.virtual_gen {
                    'v'
                } else if c.generated.is_some() {
                    's'
                } else {
                    '\0'
                },
                key: t.replident == Some('f') || key.contains(&i),
            }
        })
        .collect();
    let mut ancestors = Vec::new();
    let mut cur = t;
    while cur.partbound.is_some() {
        let Some(p) = cur.inherits.first().and_then(|p| cat.tables.get(p)) else { break };
        ancestors.push(p.oid);
        cur = p;
    }
    RelDesc {
        oid: t.oid,
        nsp: cat.schema_name(t.schema).to_string(),
        name: t.name.clone(),
        replident: t.replident.unwrap_or('d'),
        cols,
        ancestors,
    }
}

/// Append a change record (`body`; empty: only `rel`'s description, when it needs one).
fn put(ctx: &ExecCtx, rel: Option<&Table>, body: Vec<u8>) -> Result<()> {
    // (The description is made before the transaction is borrowed: the catalog reads it.)
    let desc = match rel {
        Some(t) => {
            let cat = ctx.catalog();
            let h = hash_def(t, &cat);
            let known = ctx.txn.borrow().log.as_ref().is_some_and(|l| l.rels.get(&t.oid) == Some(&h));
            if known {
                None
            } else {
                let mut descs = ctx.sess.db.log.descs.lock();
                let rec = match descs.get(&t.oid) {
                    Some((dh, r)) if *dh == h => r.clone(),
                    _ => {
                        let mut b = vec![b'R'];
                        b.extend_from_slice(&t.oid.to_be_bytes());
                        b.extend_from_slice(&serde_json::to_vec(&describe(t, ctx)).unwrap_or_default());
                        let r = Arc::new(b);
                        descs.insert(t.oid, (h, r.clone()));
                        r
                    }
                };
                Some((t.oid, h, rec))
            }
        }
        None => None,
    };
    let mut txn = ctx.txn.borrow_mut();
    let xid = txn.xid();
    let Some(mut log) = txn.log.take() else { return Ok(()) };
    let r = (|| {
        if let Some((oid, h, rec)) = desc {
            log.bytes += record_size(rec.len());
            txn.put_log(change_key(xid, log.seq), rec.as_ref().clone())?;
            log.seq += 1;
            log.rels.insert(oid, h);
        }
        if !body.is_empty() {
            log.bytes += record_size(body.len());
            txn.put_log(change_key(xid, log.seq), body)?;
            log.seq += 1;
        }
        Ok(())
    })();
    txn.log = Some(log);
    r
}

fn capturing(ctx: &ExecCtx) -> bool {
    ctx.txn.borrow().log.is_some()
}

/// The row as logged: virtual generated and dropped columns are NULL (they aren't stored).
fn stored(t: &Table, row: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = row
        .iter()
        .zip(&t.columns)
        .map(|(v, c)| if c.virtual_gen || c.dropped { Value::Null } else { v.clone() })
        .collect();
    out.resize(t.columns.len(), Value::Null);
    out
}

fn head(kind: u8, t: &Table) -> Vec<u8> {
    let mut b = vec![kind];
    b.extend_from_slice(&t.oid.to_be_bytes());
    b
}

/// Binary equality, as heap_update compares a column's old and new datums.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => encode_row(std::slice::from_ref(a)) == encode_row(std::slice::from_ref(b)),
    }
}

/// The same datum, not only an equal one: what an UPDATE didn't assign (its value is the
/// old row's, shared).
fn same_datum(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Text(x), Value::Text(y)) | (Value::Json(x), Value::Json(y)) => Arc::ptr_eq(x, y),
        (Value::Bytea(x), Value::Bytea(y)) => Arc::ptr_eq(x, y),
        (Value::Array(x), Value::Array(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

/// The old row an UPDATE or DELETE logs (ExtractReplicaIdentity): the whole row with
/// REPLICA IDENTITY FULL, the identity key's columns (the others NULL) when the key is
/// needed (an UPDATE: when it changed), nothing without an identity.
fn old_tuple(t: &Table, old: &[Value], new: Option<&[Value]>, ext: &[bool], ctx: &ExecCtx) -> Option<(Vec<Value>, bool)> {
    let old = stored(t, old);
    match t.replident {
        Some('n') => None,
        Some('f') => Some((old, false)),
        _ => {
            let key = ctx.catalog().identity_key(t)?;
            // (A key stored out of line is logged even when unchanged.)
            if let Some(new) = new
                && key.iter().all(|&i| same(&old[i], new.get(i).unwrap_or(&Value::Null)))
                && !key.iter().any(|&i| ext.get(i).copied().unwrap_or(false))
            {
                return None;
            }
            let row = old
                .iter()
                .enumerate()
                .map(|(i, v)| if key.contains(&i) { v.clone() } else { Value::Null })
                .collect();
            Some((row, true))
        }
    }
}

fn put_old(b: &mut Vec<u8>, old: &Option<(Vec<Value>, bool)>) {
    match old {
        None => b.push(0),
        Some((row, key)) => {
            b.push(if *key { 3 } else { 1 });
            let r = encode_row(row);
            put_varint(b, r.len() as u64);
            b.extend_from_slice(&r);
        }
    }
}

pub fn insert(t: &Table, row: &[Value], ctx: &ExecCtx) -> Result<()> {
    if !capturing(ctx) || !logged(t) {
        return Ok(());
    }
    let mut b = head(b'I', t);
    b.extend_from_slice(&encode_row(&stored(t, row)));
    put(ctx, Some(t), b)
}

/// A row a table rewrite wrote into the table's new storage (heap_insert into the transient
/// relation: decoded as an INSERT only for plugins asking for rewrites).
pub fn rewrite_insert(t: &Table, row: &[Value], ctx: &ExecCtx) -> Result<()> {
    if !capturing(ctx) || !logged(t) {
        return Ok(());
    }
    let mut b = head(b'W', t);
    b.extend_from_slice(&encode_row(&stored(t, row)));
    put(ctx, Some(t), b)
}

pub fn update(t: &Table, old: &[Value], new: &[Value], ctx: &ExecCtx) -> Result<()> {
    if !capturing(ctx) || !logged(t) {
        return Ok(());
    }
    // Unchanged values PostgreSQL keeps out of line: the new version points at the old
    // one's TOAST data, which a decoder doesn't see again.
    // (A column the UPDATE left alone still holds the old row's very datum.)
    let ext = crate::exec::sizing::external_columns(t, old, ctx).unwrap_or_default();
    let toast: Vec<usize> = ext
        .iter()
        .enumerate()
        .filter(|(i, e)| **e && old.get(*i).zip(new.get(*i)).is_some_and(|(a, b)| same_datum(a, b)))
        .map(|(i, _)| i)
        .collect();
    let mut b = head(b'U', t);
    put_old(&mut b, &old_tuple(t, old, Some(new), &ext, ctx));
    put_varint(&mut b, toast.len() as u64);
    for i in &toast {
        put_varint(&mut b, *i as u64);
    }
    b.extend_from_slice(&encode_row(&stored(t, new)));
    put(ctx, Some(t), b)
}

pub fn delete(t: &Table, old: &[Value], ctx: &ExecCtx) -> Result<()> {
    if !capturing(ctx) || !logged(t) {
        return Ok(());
    }
    let mut b = head(b'D', t);
    put_old(&mut b, &old_tuple(t, old, None, &[], ctx));
    put(ctx, Some(t), b)
}

/// TRUNCATE of these tables (those logged).
pub fn truncate(tables: &[&Table], cascade: bool, restart_seqs: bool, ctx: &ExecCtx) -> Result<()> {
    // (TRUNCATE writes no rows: it takes its transaction id here, as heap_truncate's WAL
    // record does.)
    if ctx.sess.db.log.capture.load(std::sync::atomic::Ordering::SeqCst) && tables.iter().any(|t| logged(t)) {
        ctx.txn.borrow_mut().xid();
    }
    if !capturing(ctx) {
        return Ok(());
    }
    let tables: Vec<&&Table> = tables.iter().filter(|t| logged(t)).collect();
    if tables.is_empty() {
        return Ok(());
    }
    for t in &tables {
        put(ctx, Some(t), Vec::new())?;
    }
    let mut b = vec![b'T', cascade as u8 | ((restart_seqs as u8) << 1)];
    put_varint(&mut b, tables.len() as u64);
    for t in &tables {
        b.extend_from_slice(&t.oid.to_be_bytes());
    }
    put(ctx, None, b)
}

/// A transactional pg_logical_emit_message.
pub fn message(prefix: &str, content: &[u8], ctx: &ExecCtx) -> Result<()> {
    put(ctx, None, message_body(prefix, content))
}

/// A message record's bytes (also a non-transactional message's commit record's tail).
pub fn message_body(prefix: &str, content: &[u8]) -> Vec<u8> {
    let mut b = vec![b'M'];
    put_varint(&mut b, prefix.len() as u64);
    b.extend_from_slice(prefix.as_bytes());
    b.extend_from_slice(content);
    b
}

/// A change record, decoded.
pub fn decode_rec(b: &[u8]) -> Result<Rec> {
    use crate::storage::row::decode_row;
    use crate::storage::sst::get_varint;
    let bad = || crate::error::internal("bad change stream record");
    let rel = || -> Result<u32> { Ok(u32::from_be_bytes(b.get(1..5).ok_or_else(bad)?.try_into().unwrap())) };
    let old_at = |p: &mut usize| -> Result<Option<(Vec<Value>, bool)>> {
        let f = *b.get(*p).ok_or_else(bad)?;
        *p += 1;
        if f == 0 {
            return Ok(None);
        }
        let n = get_varint(b, p)? as usize;
        let row = decode_row(b.get(*p..*p + n).ok_or_else(bad)?)?;
        *p += n;
        Ok(Some((row, f & 2 != 0)))
    };
    Ok(match b.first() {
        Some(b'R') => {
            let d: RelDesc = serde_json::from_slice(b.get(5..).ok_or_else(bad)?).map_err(|_| bad())?;
            Rec::Rel(Arc::new(d))
        }
        Some(b'I' | b'W') => Rec::Insert { rel: rel()?, new: decode_row(&b[5..])? },
        Some(b'U') => {
            let mut p = 5;
            let old = old_at(&mut p)?;
            let n = get_varint(b, &mut p)? as usize;
            let mut unchanged_toast = Vec::with_capacity(n);
            for _ in 0..n {
                unchanged_toast.push(get_varint(b, &mut p)? as usize);
            }
            Rec::Update { rel: rel()?, old, new: decode_row(&b[p..])?, unchanged_toast }
        }
        Some(b'D') => {
            let mut p = 5;
            Rec::Delete { rel: rel()?, old: old_at(&mut p)? }
        }
        Some(b'T') => {
            let f = *b.get(1).ok_or_else(bad)?;
            let mut p = 2;
            let n = get_varint(b, &mut p)? as usize;
            let mut rels = Vec::with_capacity(n);
            for _ in 0..n {
                rels.push(u32::from_be_bytes(b.get(p..p + 4).ok_or_else(bad)?.try_into().unwrap()));
                p += 4;
            }
            Rec::Truncate { cascade: f & 1 != 0, restart_seqs: f & 2 != 0, rels }
        }
        Some(b'M') => {
            let mut p = 1;
            let n = get_varint(b, &mut p)? as usize;
            let prefix = String::from_utf8_lossy(b.get(p..p + n).ok_or_else(bad)?).into_owned();
            Rec::Message { transactional: true, prefix, content: b[p + n..].to_vec() }
        }
        _ => return Err(bad()),
    })
}

/// Writes removing what every slot of the database has confirmed: commit records ending
/// at or before `upto`, and the change records of the transactions they end. (Records of
/// transactions that wrote while a slot was being dropped have no slot to read them and
/// go once a later commit is confirmed, or with the last slot.)
pub fn gc_writes(store: &crate::storage::Store, upto: u64) -> Result<Vec<(Vec<u8>, Option<Bytes>)>> {
    let ts = store.visible_ts();
    let from = commit_key(1);
    let to = commit_key(upto.saturating_add(1));
    let mut out = Vec::new();
    for r in store.scan(&from, &to, ts) {
        let (k, v) = r?;
        let m = Marker::decode(&v)?;
        if m.end_lsn > upto {
            continue;
        }
        // (A prepared transaction's records stay for its COMMIT / ROLLBACK PREPARED.)
        if !matches!(m.kind, Kind::Message | Kind::Prepare) {
            let (a, b) = changes_range(m.xid);
            for c in store.scan(&a, &b, ts) {
                out.push((c?.0, None));
            }
        }
        out.push((k, None));
    }
    Ok(out)
}

/// Commit `writes` with a commit record of `m` sealed under the commit lock (`bytes`: what
/// the transaction's change records take); its end position.
fn commit_sealed(db: &crate::txn::Database, mut m: Marker, bytes: u64, writes: Vec<(Vec<u8>, Option<Bytes>)>) -> Result<u64> {
    let Some(r) = db.repl() else {
        db.store.commit(writes, |_| Ok(()))?;
        return Ok(0);
    };
    let sealed = std::cell::Cell::new(0);
    let res = db.store.commit_grouped(writes, |_, w| {
        let m = std::mem::replace(&mut m, Marker::new(Kind::Commit, 0, 0));
        let (rec, lsn) = seal(&db.log, &r.wal, m, bytes);
        sealed.set(lsn);
        w.push(rec);
        Ok(())
    }, |_| {}, |_| {});
    if sealed.get() != 0 {
        committed(&db.log, sealed.get());
    }
    res?;
    Ok(super::lsn::align(sealed.get() + COMMIT_SIZE))
}

/// EndPrepare's commit of a prepared transaction's state record (`writes`): with the
/// transaction's change records and a PREPARE record when it logs its changes, which two-phase
/// decoding reads at PREPARE. (Its COMMIT PREPARED writes the change records again.)
pub fn prepare(
    db: &crate::txn::Database,
    txn: &mut crate::txn::Txn,
    info: &crate::txn::twophase::Info,
    mut writes: Vec<(Vec<u8>, Option<Bytes>)>,
) -> Result<()> {
    if txn.log.as_ref().is_some_and(|l| l.seq == 0) && txn.changed_catalog().is_none() {
        txn.log = None;
    }
    let Some(log) = txn.log.as_ref() else {
        db.store.commit(writes, |_| Ok(()))?;
        return Ok(());
    };
    let (bytes, origin, origin_lsn) = (log.bytes, log.origin, log.origin_lsn);
    writes.extend(txn.writes_in(PREFIX, PREFIX_END)?);
    let mut m = Marker::new(Kind::Prepare, info.xid, info.prepared_at);
    m.gid = info.gid.clone();
    m.origin = origin;
    m.origin_lsn = origin_lsn;
    let end = commit_sealed(db, m, bytes, writes)?;
    if let Some(l) = txn.log.as_mut() {
        l.prepared = Some(Prepared { gid: info.gid.clone(), time: info.prepared_at, end_lsn: end });
    }
    Ok(())
}

/// ROLLBACK PREPARED's commit (`writes`: the state record's removal), with its record.
pub fn rollback_prepared(db: &crate::txn::Database, txn: &crate::txn::Txn, writes: Vec<(Vec<u8>, Option<Bytes>)>) -> Result<u64> {
    let Some((log, p)) = txn.log.as_ref().and_then(|l| Some((l, l.prepared.as_ref()?))) else {
        return db.store.commit(writes, |_| Ok(()));
    };
    let mut m = Marker::new(Kind::RollbackPrepared, txn.xid_if_assigned().unwrap_or(0), now());
    m.gid = p.gid.clone();
    m.prepare_time = p.time;
    m.prepare_end_lsn = p.end_lsn;
    m.origin = log.origin;
    commit_sealed(db, m, 0, writes)
}

/// A prepared transaction rebuilt after a restart logs its changes if it did before: its
/// log, from its PREPARE record.
pub fn recovered(db: &crate::txn::Database, txn: &mut crate::txn::Txn, gid: &str) -> Result<()> {
    if txn.writes_in(PREFIX, PREFIX_END)?.is_empty() {
        return Ok(());
    }
    let ts = db.store.visible_ts();
    let mut prepared = None;
    for r in db.store.scan(&commit_key(1), &commit_key(u64::MAX), ts) {
        let m = Marker::decode(&r?.1)?;
        if m.kind == Kind::Prepare && m.gid == gid {
            prepared = Some(Prepared { gid: m.gid.clone(), time: m.time, end_lsn: m.end_lsn });
        }
    }
    let bytes = txn.writes_in(PREFIX, PREFIX_END)?.values().map(|v| record_size(v.as_ref().map_or(0, |b| b.len()))).sum();
    txn.log = Some(Box::new(TxnLog {
        seq: u32::MAX / 2,
        bytes,
        prepared: Some(prepared.unwrap_or(Prepared { gid: gid.to_string(), time: now(), end_lsn: 0 })),
        ..Default::default()
    }));
    Ok(())
}
