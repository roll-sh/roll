//! Heavyweight locks and row locks, as Postgres's lock manager and heap_lock_tuple behave.
//!
//! - Heavyweight locks (`acquire`): Postgres's eight modes and conflict table, per-backend
//!   holds (a backend's session and transaction never conflict with each other), FIFO wait
//!   queues with JoinWaitQueue's rules (a request conflicting with an earlier waiter queues
//!   behind it; one already holding what a waiter wants goes ahead of that waiter, or fails at
//!   once when the two block each other), ProcLockWakeup's grants on release, and
//!   DeadLockCheck after the waiter's `deadlock_timeout`: a hard cycle fails the waiter that
//!   checks ("deadlock detected" with its DETAIL), a soft one is broken by reordering wait
//!   queues (deadlock.c's search, ported).
//! - Row locks (`lock_row`): the four strengths (FOR KEY SHARE … FOR UPDATE; an UPDATE holds
//!   NO KEY UPDATE or UPDATE, a DELETE UPDATE) held by (sub)transactions. A request
//!   conflicting with other transactions' holds takes the heavyweight tuple lock (its place in
//!   line), then waits for each conflicting holder's transaction lock, as heap_lock_tuple
//!   does; one not conflicting is granted whoever waits.
//! - Every (sub)transaction that locks rows holds its transaction lock (ExclusiveLock on
//!   `xact_key`) until it ends; waiting for a transaction is a ShareLock request on it.
//! - The table is partitioned by key hash (lock.c's partitions): a lock, a row's holds and the
//!   backends waiting for a key are in one partition, under its mutex alone; deadlock checks,
//!   pg_locks and PREPARE take every partition, in order.

use super::bulklocks::{BulkLocks, row_tag, untag};
use crate::error::{Error, Result, code};
use crate::exec::fasthash::HashMap as FastMap;
use parking_lot::{Condvar, Mutex, MutexGuard};
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A heavyweight lock mode (lockdefs.h), in Postgres's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockMode {
    AccessShare = 1,
    RowShare = 2,
    RowExclusive = 3,
    ShareUpdateExclusive = 4,
    Share = 5,
    ShareRowExclusive = 6,
    Exclusive = 7,
    AccessExclusive = 8,
}

const fn bits(modes: &[u8]) -> u16 {
    let mut m = 0;
    let mut i = 0;
    while i < modes.len() {
        m |= 1 << modes[i];
        i += 1;
    }
    m
}

/// lock.c's LockConflicts.
const CONFLICTS: [u16; 9] = [
    0,
    bits(&[8]),
    bits(&[7, 8]),
    bits(&[5, 6, 7, 8]),
    bits(&[4, 5, 6, 7, 8]),
    bits(&[3, 4, 6, 7, 8]),
    bits(&[3, 4, 5, 6, 7, 8]),
    bits(&[2, 3, 4, 5, 6, 7, 8]),
    bits(&[1, 2, 3, 4, 5, 6, 7, 8]),
];

impl LockMode {
    const ALL: [LockMode; 8] = [
        LockMode::AccessShare,
        LockMode::RowShare,
        LockMode::RowExclusive,
        LockMode::ShareUpdateExclusive,
        LockMode::Share,
        LockMode::ShareRowExclusive,
        LockMode::Exclusive,
        LockMode::AccessExclusive,
    ];

    /// The mode numbered as in lockdefs.h (LOCK TABLE's `mode`).
    pub fn from_number(n: i32) -> Option<LockMode> {
        Self::ALL.get((n as usize).checked_sub(1)?).copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            LockMode::AccessShare => "AccessShareLock",
            LockMode::RowShare => "RowShareLock",
            LockMode::RowExclusive => "RowExclusiveLock",
            LockMode::ShareUpdateExclusive => "ShareUpdateExclusiveLock",
            LockMode::Share => "ShareLock",
            LockMode::ShareRowExclusive => "ShareRowExclusiveLock",
            LockMode::Exclusive => "ExclusiveLock",
            LockMode::AccessExclusive => "AccessExclusiveLock",
        }
    }

    fn bit(self) -> u16 {
        1 << self as u8
    }

    fn conflicts(self) -> u16 {
        CONFLICTS[self as usize]
    }

    pub fn conflicts_with(self, other: LockMode) -> bool {
        self.conflicts() & other.bit() != 0
    }
}

/// A row lock's strength (LockTupleMode).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowLockMode {
    KeyShare,
    Share,
    NoKeyUpdate,
    Update,
}

impl RowLockMode {
    /// The heavyweight mode of its tuple lock (tupleLockExtraInfo); the strengths conflict
    /// as these do.
    pub fn heavy(self) -> LockMode {
        match self {
            RowLockMode::KeyShare => LockMode::AccessShare,
            RowLockMode::Share => LockMode::RowShare,
            RowLockMode::NoKeyUpdate => LockMode::Exclusive,
            RowLockMode::Update => LockMode::AccessExclusive,
        }
    }

    pub fn conflicts_with(self, other: RowLockMode) -> bool {
        self.heavy().conflicts_with(other.heavy())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitPolicy {
    Block,
    NoWait,
    SkipLocked,
}

/// How a wait may end other than by the lock being granted.
pub struct Waits<'a> {
    pub policy: WaitPolicy,
    pub cancel: &'a AtomicBool,
    pub lock_timeout: Option<Duration>,
    /// When the statement times out (statement_timeout).
    pub deadline: Option<Instant>,
    /// When the transaction times out (transaction_timeout: the session ends).
    pub txn_deadline: Option<Instant>,
    pub deadlock_timeout: Duration,
}

/// transaction_timeout passed: ProcessInterrupts ends the session.
pub fn transaction_timeout() -> Error {
    Error::new(code::TRANSACTION_TIMEOUT, "terminating connection due to transaction timeout").fatal()
}

impl<'a> Waits<'a> {
    pub fn new(policy: WaitPolicy, cancel: &'a AtomicBool) -> Self {
        Waits {
            policy,
            cancel,
            lock_timeout: None,
            deadline: None,
            txn_deadline: None,
            deadlock_timeout: Duration::from_secs(1),
        }
    }

    fn with_policy(&self, policy: WaitPolicy) -> Waits<'a> {
        Waits {
            policy,
            cancel: self.cancel,
            lock_timeout: self.lock_timeout,
            deadline: self.deadline,
            txn_deadline: self.txn_deadline,
            deadlock_timeout: self.deadlock_timeout,
        }
    }
}

/// The 8-byte prefixes of lock keys that aren't data keys.
pub const ADVISORY: u8 = 3;
pub const RELATION: u8 = 6;
pub const TUPLE: u8 = 7;
pub const TRANSACTION: u8 = 8;
pub const OBJECT: u8 = 9;
pub const SPECTOKEN: u8 = 10;
/// A parallel apply lock (LOCKTAG_APPLY_TRANSACTION, `apply_transaction_key`).
pub const APPLY_TRANSACTION: u8 = 13;
/// A system catalog row's key for row locks (`catalog_row_key`).
pub const CATALOG_ROW: u8 = 11;
/// A unique index key's lock (the engine's in-progress index entry, `unique_key_lock`).
pub const UNIQUE_KEY: u8 = 4;
/// An exclusion constraint's entry in progress (`excl_entry_key`): what a dirty snapshot
/// sees of another writer's index entry, held by the writer until its (sub)transaction ends.
pub const EXCL_ENTRY: u8 = 12;

/// A system catalog's unique index entry of name `name` in namespace `ns` (`index`: the
/// catalog index's oid, which no user table's index prefix starts with): the lock a
/// transaction creating or renaming an object to that name holds until it ends, as
/// PostgreSQL's in-progress entry of the uncommitted catalog row makes another transaction
/// inserting the same key wait for it (`Ddl::reserve_names`).
pub fn catalog_name_key(index: u32, ns: u32, name: &str) -> Vec<u8> {
    let mut k = prefixed(UNIQUE_KEY);
    k.extend_from_slice(&index.to_be_bytes());
    k.extend_from_slice(&ns.to_be_bytes());
    k.extend_from_slice(name.as_bytes());
    k
}

/// The lock standing for row `row`'s entry in exclusion constraint `con` while its writer
/// runs (`Txn::hold_exclusion_entry`).
pub fn excl_entry_key(con: u32, row: &[u8]) -> Vec<u8> {
    let mut k = prefixed(EXCL_ENTRY);
    k.extend_from_slice(&con.to_be_bytes());
    k.extend_from_slice(row);
    k
}

/// A logical replication parallel apply lock (LOCKTAG_APPLY_TRANSACTION) on remote
/// transaction `xid` of subscription `sub`: `objid` 0 the leader's stream lock, 1 the
/// parallel apply worker's transaction lock (applyparallelworker.c's PARALLEL_APPLY_LOCK_*).
pub fn apply_transaction_key(db: u32, sub: u32, xid: u32, objid: u16) -> Vec<u8> {
    let mut k = prefixed(APPLY_TRANSACTION);
    k.extend_from_slice(&db.to_be_bytes());
    k.extend_from_slice(&sub.to_be_bytes());
    k.extend_from_slice(&xid.to_be_bytes());
    k.extend_from_slice(&objid.to_be_bytes());
    k
}

fn prefixed(kind: u8) -> Vec<u8> {
    vec![0, 0, 0, 0, 0, 0, 0, kind]
}

/// The row of object `oid` in system catalog `class` (pg_class, pg_database, ...), as row
/// locks name it: the engine keeps catalogs as objects, Postgres as heap tuples other
/// transactions lock, update and update in place.
pub fn catalog_row_key(class: u32, oid: u32) -> Vec<u8> {
    let mut k = prefixed(CATALOG_ROW);
    k.extend_from_slice(&class.to_be_bytes());
    k.extend_from_slice(&oid.to_be_bytes());
    k
}

/// A speculative insertion's token lock (LOCKTAG_SPECULATIVE_TOKEN): held by the inserter
/// until its tuple is either completed or killed, waited for by others meeting the tuple.
pub fn spectoken_key(xid: u64, token: u32) -> Vec<u8> {
    let mut k = prefixed(SPECTOKEN);
    k.extend_from_slice(&xid.to_be_bytes());
    k.extend_from_slice(&token.to_be_bytes());
    k
}

/// What a unique key's lock holder has put in the index, as another writer's check of the
/// key finds it (Postgres's dirty snapshot seeing an in-progress index entry): the
/// (sub)transaction to wait for, its speculative insertion's token while that is
/// unfinished, and the row (whose index expressions the check evaluates).
#[derive(Clone, Debug)]
pub struct KeyHolder {
    pub holder: u64,
    pub xid: u64,
    pub token: Option<u32>,
    pub row: Option<Arc<Vec<crate::types::Value>>>,
    /// The row's storage key (its heap tuple, which a wait for it names in its CONTEXT).
    pub row_key: Option<Vec<u8>>,
}

/// The lock on relation `rel` of database `db` (LOCKTAG_RELATION).
pub fn relation_key(db: u32, rel: u32) -> Vec<u8> {
    relation_key_bytes(db, rel).to_vec()
}

/// `relation_key`, on the stack.
pub fn relation_key_bytes(db: u32, rel: u32) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[7] = RELATION;
    k[8..12].copy_from_slice(&db.to_be_bytes());
    k[12..].copy_from_slice(&rel.to_be_bytes());
    k
}

/// The heavyweight lock of a row (LOCKTAG_TUPLE), by its data key.
pub fn tuple_key(db: u32, rel: u32, row: &[u8]) -> Vec<u8> {
    let mut k = prefixed(TUPLE);
    k.extend_from_slice(&db.to_be_bytes());
    k.extend_from_slice(&rel.to_be_bytes());
    k.extend_from_slice(row);
    k
}

/// A (sub)transaction's own lock (LOCKTAG_TRANSACTION), by its lock id.
pub fn xact_key(id: u64) -> Vec<u8> {
    let mut k = prefixed(TRANSACTION);
    k.extend_from_slice(&id.to_be_bytes());
    k
}

/// The transaction whose lock `key` is (an `xact_key`), if it is one.
fn xact_key_owner(key: &[u8]) -> Option<u64> {
    (key.len() == 16 && key[..8] == [0, 0, 0, 0, 0, 0, 0, TRANSACTION]).then(|| u64::from_be_bytes(key[8..].try_into().unwrap()))
}

/// A lock on a database object (LOCKTAG_OBJECT).
pub fn object_key(db: u32, class: u32, obj: u32, sub: u16) -> Vec<u8> {
    let mut k = prefixed(OBJECT);
    k.extend_from_slice(&db.to_be_bytes());
    k.extend_from_slice(&class.to_be_bytes());
    k.extend_from_slice(&obj.to_be_bytes());
    k.extend_from_slice(&sub.to_be_bytes());
    k
}

/// What a lock key names, for pg_locks and deadlock reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Tag {
    Relation { db: u32, rel: u32 },
    Tuple { db: u32, rel: u32, row: Vec<u8> },
    /// A transaction's lock, with the transaction id it shows (0: none assigned).
    Transaction { id: u64, xid: u64 },
    Object { db: u32, class: u32, obj: u32, sub: u16 },
    Advisory(Vec<u8>),
    /// A speculative insertion's token.
    SpecToken { xid: u64, token: u32 },
    /// A parallel apply lock on a remote transaction of a subscription.
    ApplyTransaction { db: u32, sub: u32, xid: u32, objid: u16 },
    /// The engine's own locks (unique keys, exclusion constraints, large objects).
    Other(Vec<u8>),
}

fn be32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_be_bytes(s.try_into().unwrap()))
}

pub fn tag_of(key: &[u8], xid: u64) -> Tag {
    if key.len() < 8 || key[..7] != [0; 7] {
        return Tag::Other(key.to_vec());
    }
    let b = &key[8..];
    match key[7] {
        RELATION => Tag::Relation { db: be32(b, 0), rel: be32(b, 4) },
        TUPLE => Tag::Tuple {
            db: be32(b, 0),
            rel: be32(b, 4),
            row: b.get(8..).unwrap_or_default().to_vec(),
        },
        TRANSACTION => Tag::Transaction {
            id: b.get(..8).map_or(0, |s| u64::from_be_bytes(s.try_into().unwrap())),
            xid,
        },
        OBJECT => Tag::Object {
            db: be32(b, 0),
            class: be32(b, 4),
            obj: be32(b, 8),
            sub: b.get(12..14).map_or(0, |s| u16::from_be_bytes(s.try_into().unwrap())),
        },
        ADVISORY => Tag::Advisory(key.to_vec()),
        SPECTOKEN => Tag::SpecToken {
            xid: b.get(..8).map_or(0, |s| u64::from_be_bytes(s.try_into().unwrap())),
            token: be32(b, 8),
        },
        APPLY_TRANSACTION => Tag::ApplyTransaction {
            db: be32(b, 0),
            sub: be32(b, 4),
            xid: be32(b, 8),
            objid: b.get(12..14).map_or(0, |s| u16::from_be_bytes(s.try_into().unwrap())),
        },
        _ => Tag::Other(key.to_vec()),
    }
}

/// A lock on a relation of a database (not a shared catalog): what the fast path takes.
fn is_local_relation(key: &[u8]) -> bool {
    key.len() >= 16 && key[..7] == [0; 7] && key[7] == RELATION && be32(&key[8..], 0) != 0
}

/// DescribeLockTag.
fn describe(tag: &Tag, db: u32) -> String {
    match tag {
        Tag::Relation { db, rel } => format!("relation {rel} of database {db}"),
        Tag::Tuple { db, rel, .. } => format!("tuple of relation {rel} of database {db}"),
        Tag::Transaction { xid, .. } if *xid != 0 => format!("transaction {xid}"),
        Tag::Transaction { id, .. } => format!("virtual transaction {}", super::procs::vxid_of_owner(*id).unwrap_or_default()),
        Tag::Object { db, class, obj, .. } => {
            format!("object {obj} of class {class} of database {db}")
        }
        Tag::Advisory(k) => {
            let b = &k[8..];
            let (f2, f3, f4) = match b.first() {
                Some(1) => {
                    let v = b.get(1..9).map_or(0, |s| i64::from_be_bytes(s.try_into().unwrap()));
                    ((v >> 32) as u32, v as u32, 1)
                }
                _ => (be32(b, 1), be32(b, 5), 2),
            };
            format!("advisory lock [{db},{f2},{f3},{f4}]")
        }
        Tag::SpecToken { xid, token } => {
            format!("speculative token {token} of transaction {xid}")
        }
        Tag::ApplyTransaction { db, sub, xid, .. } => {
            format!("remote transaction {xid} of subscription {sub} of database {db}")
        }
        Tag::Other(_) => "a row".to_string(),
    }
}

/// One backend's exclusion constraint entries in progress (`LockManager::excl`): (lock key, row
/// key, writer), and the length at which its ended ones are dropped next.
#[derive(Default)]
struct ExclList {
    entries: Vec<(Vec<u8>, Vec<u8>, KeyHolder)>,
    prune_at: usize,
}

/// A granted hold: one owner's (a transaction's or a session's) hold of one mode.
#[derive(Clone, Debug)]
struct Hold {
    owner: u64,
    proc: u64,
    mode: LockMode,
    count: u32,
    /// Taken through the fast path (a weak relation lock while no strong one was held or
    /// asked for), as pg_locks shows.
    fastpath: bool,
    /// When it was taken (`next_seq`): holds moved in from the fast path go in that order.
    seq: u64,
}

/// A process-wide order of holds.
fn next_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

impl Hold {
    fn new(owner: u64, proc: u64, mode: LockMode, fastpath: bool) -> Hold {
        Hold { owner, proc, mode, count: 1, fastpath, seq: next_seq() }
    }
}

/// A backend's weak relation locks taken on the fast path (PGPROC's fpRelId / fpLockBits):
/// AccessShare, RowShare and RowExclusive locks of relations of the database, taken while no
/// backend holds or waits for a lock of a stronger mode on any relation of the key's
/// partition (`FastPath::strong`), without the lock table's mutex. A strong request first
/// counts itself there, then moves every backend's fast-path holds of its relation into the
/// lock table (FastPathTransferRelationLocks); a backend checks the count under its own
/// slot's mutex, so it either sees it or had its hold moved. The rare readers of every hold
/// (pg_locks, prepared transactions, WaitForLockers) move them all first, still shown as
/// fast-path locks.
pub struct FastSlot {
    proc: u64,
    holds: Mutex<FastHolds>,
    /// The transaction whose own lock is held here (`hold_xact_fast`), 0 for none: a request
    /// for that lock moves it to the main table first (VirtualXactLock's transfer).
    xact: AtomicU64,
}

#[derive(Default)]
struct FastHolds {
    holds: Vec<FastHold>,
    /// Owners with holds transferred to the main table, protected by the same mutex as
    /// transfer and release so another transaction on this backend cannot clear them.
    moved: HashSet<u64>,
}

struct FastHold {
    key: Vec<u8>,
    owner: u64,
    mode: LockMode,
    count: u32,
    seq: u64,
}

const FAST_PARTITIONS: usize = 1024;

struct FastPath {
    /// Per partition of relation keys, how many keys have a strong lock held or awaited
    /// (FastPathStrongRelationLocks): kept by the lock table (`Part::strong_keys`).
    strong: Arc<[AtomicU32]>,
    slots: parking_lot::RwLock<HashMap<u64, Arc<FastSlot>>>,
}

impl Default for FastPath {
    fn default() -> Self {
        FastPath {
            strong: (0..FAST_PARTITIONS).map(|_| AtomicU32::new(0)).collect(),
            slots: Default::default(),
        }
    }
}

fn fast_partition(key: &[u8]) -> usize {
    be32(&key[12..], 0) as usize % FAST_PARTITIONS
}

/// Whether a lock of `mode` on `key` conflicts with the fast path's (ConflictsWithRelationFastPath).
fn strong_relation_lock(key: &[u8], mode: LockMode) -> bool {
    mode > LockMode::ShareUpdateExclusive && is_local_relation(key)
}

#[derive(Default)]
struct Entry {
    holds: Vec<Hold>,
    /// Waiting backends, in queue order.
    queue: Vec<u64>,
    /// A transaction lock's transaction id (pg_locks, deadlock reports).
    xid: u64,
    /// A unique key lock's in-progress entry: (the top-level transaction holding it, what
    /// it put there). Stale once that transaction holds it no longer.
    key_holder: Option<(u64, KeyHolder)>,
}

impl Entry {
    /// The modes held by backends other than `proc`.
    fn others_mask(&self, proc: u64) -> u16 {
        self.holds
            .iter()
            .filter(|h| h.proc != proc)
            .fold(0, |m, h| m | h.mode.bit())
    }

    fn held_mask(&self, proc: u64) -> u16 {
        self.holds
            .iter()
            .filter(|h| h.proc == proc)
            .fold(0, |m, h| m | h.mode.bit())
    }

    /// LockCheckConflicts.
    fn conflicts(&self, proc: u64, mode: LockMode) -> bool {
        mode.conflicts() & self.others_mask(proc) != 0
    }

    fn grant(&mut self, owner: u64, proc: u64, mode: LockMode, key: &[u8]) {
        match self
            .holds
            .iter_mut()
            .find(|h| h.owner == owner && h.proc == proc && h.mode == mode)
        {
            Some(h) => h.count += 1,
            None => {
                let fastpath = mode <= LockMode::RowExclusive
                    && is_local_relation(key)
                    && !self.holds.iter().any(|h| h.mode > LockMode::ShareUpdateExclusive);
                self.holds.push(Hold::new(owner, proc, mode, fastpath))
            }
        }
    }

    /// The backends holding it, in the order they first took it.
    fn holder_procs(&self) -> Vec<u64> {
        let mut v: Vec<u64> = Vec::new();
        for h in &self.holds {
            if !v.contains(&h.proc) {
                v.push(h.proc);
            }
        }
        v
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Waiting,
    Granted,
}

struct Waiter {
    key: Vec<u8>,
    owner: u64,
    mode: LockMode,
    /// What the backend held of the lock when it joined the queue (PGPROC.heldLocks).
    held: u16,
    status: Status,
    since: Instant,
    /// When it was granted (`LATE_GRANTS`).
    granted_at: Option<Instant>,
    /// What the waiting backend sleeps on (its PGPROC semaphore): signalled when the lock
    /// is granted, so a release wakes the backends it lets in, not every waiter.
    cv: Arc<Condvar>,
}

/// A row lock held by a (sub)transaction.
#[derive(Clone, Debug)]
struct RowHold {
    /// The lock id of the (sub)transaction holding it (its transaction lock).
    holder: u64,
    /// The top-level transaction.
    owner: u64,
    proc: u64,
    mode: RowLockMode,
    /// The lock is an update's or a delete's (the tuple's xmax isn't lock-only).
    modifies: bool,
}

/// The holds of one row, by (sub)transaction (most rows have one).
type RowHolds = SmallVec<[RowHold; 1]>;

/// How many partitions the lock table has (NUM_LOCK_PARTITIONS): a key's heavyweight lock,
/// its row locks and the backends waiting for it live in the partition its hash picks, under
/// that partition's mutex alone, so transactions locking different keys don't meet.
const PARTS: usize = 32;

/// The partition of lock or row key `key`. (From hash bits the partitions' own maps don't use
/// for their buckets or tags.)
fn part_of(key: &[u8]) -> usize {
    use std::hash::BuildHasher;
    (crate::exec::fasthash::FastState::default().hash_one(key) >> 40) as usize % PARTS
}

/// One partition of the lock table.
#[derive(Default)]
struct Part {
    /// (Keyed with the fast hasher: a transaction lock is taken by every writing transaction.)
    locks: FastMap<Vec<u8>, Entry>,
    /// Backends waiting for a lock of this partition (a backend waits for one lock at a time).
    waiters: HashMap<u64, Waiter>,
    /// Row locks of the rows of this partition.
    rows: FastMap<Vec<u8>, RowHolds>,
    /// Relation keys of this partition with a strong lock held or awaited, counted in the fast
    /// path's partitions (`FastPath::strong`).
    strong_keys: HashSet<Vec<u8>>,
}

/// A partition's mutex, on cache lines of its own.
#[derive(Default)]
#[repr(align(128))]
struct PartLock(Mutex<Part>);

/// A partition held. (Debug builds check the lock order as partitions are taken: ascending,
/// never after the compact sets, `misc` or a fast-path slot.)
struct PartGuard<'a> {
    g: MutexGuard<'a, Part>,
    #[cfg(debug_assertions)]
    i: usize,
}

impl std::ops::Deref for PartGuard<'_> {
    type Target = Part;
    fn deref(&self) -> &Part {
        &self.g
    }
}

impl std::ops::DerefMut for PartGuard<'_> {
    fn deref_mut(&mut self) -> &mut Part {
        &mut self.g
    }
}

/// A mutex taken under partitions, never before one: the compact sets, `misc`, a
/// fast-path slot.
struct Leaf<'a, T>(MutexGuard<'a, T>);

impl<T> std::ops::Deref for Leaf<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for Leaf<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

fn leaf<T>(m: &Mutex<T>) -> Leaf<'_, T> {
    #[cfg(debug_assertions)]
    order::LEAVES.with(|l| l.set(l.get() + 1));
    Leaf(m.lock())
}

#[cfg(debug_assertions)]
impl<T> Drop for Leaf<'_, T> {
    fn drop(&mut self) {
        order::LEAVES.with(|l| l.set(l.get() - 1));
    }
}

#[cfg(debug_assertions)]
impl Drop for PartGuard<'_> {
    fn drop(&mut self) {
        order::PARTS_HELD.with(|h| h.set(h.get() & !(1 << self.i)));
    }
}

/// The lock order's checks (debug builds): what this thread holds of the lock table.
#[cfg(debug_assertions)]
mod order {
    use std::cell::Cell;
    thread_local! {
        /// The partitions held, as bits.
        pub static PARTS_HELD: Cell<u64> = const { Cell::new(0) };
        /// Leaf mutexes held (`leaf`).
        pub static LEAVES: Cell<u32> = const { Cell::new(0) };
    }

    /// Partition `i` is about to be taken.
    pub fn part(i: usize) {
        assert_eq!(LEAVES.with(Cell::get), 0, "lock order: partition {i} taken under a leaf mutex");
        let held = PARTS_HELD.with(Cell::get);
        assert_eq!(held >> i, 0, "lock order: partition {i} taken while holding {held:#x}");
        PARTS_HELD.with(|h| h.set(held | 1 << i));
    }

    /// `excl` is about to be taken: before any partition.
    pub fn excl() {
        assert_eq!(LEAVES.with(Cell::get), 0, "lock order: exclusion entries taken under a leaf mutex");
        assert_eq!(PARTS_HELD.with(Cell::get), 0, "lock order: exclusion entries taken under a partition");
    }
}

/// Waiters that found their lock granted only when their sleep timed out, 40 ms or more after
/// the grant: a grant's wakeup was lost (tests read it).
pub static LATE_GRANTS: AtomicU64 = AtomicU64::new(0);

/// The oldest grant a waiter found when its sleep timed out, in microseconds (tests read it):
/// no wakeup is lost, so only a timeout racing a grant leaves one here, and a fresh one.
pub static MAX_TIMED_OUT_GRANT_US: AtomicU64 = AtomicU64::new(0);

impl Part {
    /// A strong lock of relation `key` is held or asked for.
    fn mark_strong(&mut self, key: &[u8], strong: &[AtomicU32]) {
        if self.strong_keys.insert(key.to_vec()) {
            strong[fast_partition(key)].fetch_add(1, Ordering::SeqCst);
        }
    }

    /// `key` marked strong, but no longer held or awaited in a strong mode: unmark it.
    fn sync_strong(&mut self, key: &[u8], strong: &[AtomicU32]) {
        if !self.strong_keys.contains(key) {
            return;
        }
        let held = self.locks.get(key).is_some_and(|e| {
            e.holds.iter().any(|h| strong_relation_lock(key, h.mode))
                || e.queue.iter().any(|p| self.waiters.get(p).is_some_and(|w| strong_relation_lock(key, w.mode)))
        });
        if !held {
            self.strong_keys.remove(key);
            strong[fast_partition(key)].fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn drop_empty(&mut self, key: &[u8], strong: &[AtomicU32]) {
        self.sync_strong(key, strong);
        if self.locks.get(key).is_some_and(|e| e.holds.is_empty() && e.queue.is_empty()) {
            self.locks.remove(key);
        }
    }

    /// RemoveFromWaitQueue, and the wakeups it may allow.
    fn remove_waiter(&mut self, proc: u64, strong: &[AtomicU32]) {
        let Some(w) = self.waiters.remove(&proc) else { return };
        if let Some(e) = self.locks.get_mut(&w.key) {
            e.queue.retain(|p| *p != proc);
        }
        self.wakeup(&w.key);
        self.drop_empty(&w.key, strong);
    }

    /// ProcLockWakeup: grant waiters in queue order while they conflict neither with what
    /// is held nor with the waiters still ahead of them.
    fn wakeup(&mut self, key: &[u8]) {
        let Part { locks, waiters, .. } = self;
        let Some(e) = locks.get_mut(key) else { return };
        let mut ahead = 0u16;
        let mut i = 0;
        while i < e.queue.len() {
            let p = e.queue[i];
            let Some(w) = waiters.get_mut(&p) else {
                e.queue.remove(i);
                continue;
            };
            if w.mode.conflicts() & ahead == 0 && !e.conflicts(p, w.mode) {
                e.grant(w.owner, p, w.mode, key);
                w.status = Status::Granted;
                w.granted_at = Some(Instant::now());
                w.cv.notify_one();
                e.queue.remove(i);
            } else {
                ahead |= w.mode.bit();
                i += 1;
            }
        }
    }

    /// Release one count of `owner`'s hold of `key` in `mode`, and the wakeups that allows.
    fn release_one(&mut self, key: &[u8], owner: u64, mode: LockMode, strong: &[AtomicU32]) {
        if let Some(e) = self.locks.get_mut(key)
            && let Some(i) = e.holds.iter().position(|h| h.owner == owner && h.mode == mode)
        {
            e.holds[i].count -= 1;
            if e.holds[i].count == 0 {
                e.holds.remove(i);
            }
            self.wakeup(key);
            self.drop_empty(key, strong);
        }
    }

    /// Release every hold `owner` has of `key`.
    fn release_owner(&mut self, key: &[u8], owner: u64, strong: &[AtomicU32]) {
        if let Some(e) = self.locks.get_mut(key) {
            e.holds.retain(|h| h.owner != owner);
            self.wakeup(key);
            self.drop_empty(key, strong);
        }
    }

    /// Hold (sub)transaction `holder`'s own lock, so others can wait for it to end.
    fn hold_xact(&mut self, holder: u64, owner: u64, proc: u64, xid: u64) {
        let e = self.locks.entry(xact_key(holder)).or_default();
        if xid != 0 {
            e.xid = xid;
        }
        // (One moved from the fast path is the transaction id's lock now: not fast-path.)
        match e.holds.iter_mut().find(|h| h.owner == owner) {
            Some(h) => h.fastpath = false,
            None => e.holds.push(Hold::new(owner, proc, LockMode::Exclusive, false)),
        }
    }

    /// The transaction id of transaction lock `key` (0: none, or not one).
    fn xid_of(&self, key: &[u8]) -> u64 {
        self.locks.get(key).map_or(0, |e| e.xid)
    }
}

/// Every partition, locked in order (pg_locks, deadlock checks, PREPARE: what reads or
/// changes the whole table at once).
struct All<'a>(Vec<PartGuard<'a>>);

impl All<'_> {
    fn part(&mut self, key: &[u8]) -> &mut Part {
        &mut self.0[part_of(key)]
    }

    fn get(&self, key: &[u8]) -> &Part {
        &self.0[part_of(key)]
    }

    fn xid_of(&self, key: &[u8]) -> u64 {
        self.get(key).xid_of(key)
    }
}

/// Transactions' compact row and unique-key locks (`bulklocks`), by transaction: what the
/// partitions' rows and unique keys would otherwise hold for them.
type Bulk = FastMap<u64, BulkLocks>;

/// The compact row locks on `row`, as holds (none when no transaction has any).
fn bulk_row_holds(bulk: &Bulk, row: &[u8]) -> Vec<RowHold> {
    let mut out = Vec::new();
    for (&owner, b) in bulk {
        for g in &b.segs {
            if let Some(t) = g.rows.get(row) {
                let (mode, modifies) = untag(t);
                out.push(RowHold { holder: g.holder, owner, proc: b.proc, mode, modifies });
            }
        }
    }
    out
}

/// Move a compact unique-key lock on `key` into its partition `part` (someone else asks
/// for it): from there it is queued for, released and reported as any lock.
fn materialize(part: &mut Part, bulk: &mut Bulk, key: &[u8]) {
    let mut found = None;
    'all: for (&owner, b) in bulk.iter_mut() {
        for g in &mut b.segs {
            let row_key = g.ukeys.payload(key).and_then(|p| compact_row_key(key, p));
            if g.ukeys.remove(key) {
                g.moved.push(key.to_vec());
                found = Some((owner, b.proc, g.holder, g.xid, row_key));
                break 'all;
            }
        }
    }
    if let Some((owner, proc, holder, xid, row_key)) = found {
        let e = part.locks.entry(key.to_vec()).or_default();
        e.holds.push(Hold::new(owner, proc, LockMode::Exclusive, false));
        e.key_holder = Some((owner, KeyHolder { holder, xid, token: None, row: None, row_key }));
    }
}

/// The lock waits in progress, by their cancel flag's address: what a cancel or terminate
/// request signals (Postgres's SIGINT sets the waiter's latch), so it ends the wait at
/// once, not at the next poll. Touched only by backends that wait.
static SLEEPING: std::sync::LazyLock<Mutex<HashMap<usize, Arc<Condvar>>>> =
    std::sync::LazyLock::new(Default::default);

/// A cancel or terminate request set `flag`: wake its backend if it waits for a lock or
/// sleeps (`latch_sleep`).
pub fn interrupt(flag: &AtomicBool) {
    let key = flag as *const AtomicBool as usize;
    if let Some(cv) = SLEEPING.lock().get(&key) {
        cv.notify_one();
    }
    let latches = LATCHES.lock().get(&key).cloned().unwrap_or_default();
    for l in latches {
        // (Under the latch's mutex: the sleeper checks the flag and waits under it, so the
        // wakeup can't fall between the two.)
        let _g = l.0.lock();
        l.1.notify_all();
    }
}

/// Backends sleeping in `latch_sleep`, by their cancel flag's address (parallel workers
/// share their leader's flag).
type Latch = Arc<(Mutex<()>, Condvar)>;
static LATCHES: std::sync::LazyLock<Mutex<HashMap<usize, Vec<Latch>>>> = std::sync::LazyLock::new(Default::default);

/// Sleep up to `d`, ending at once when a cancel or terminate request sets `flag`: pg_sleep's
/// WaitLatch on MyLatch, which the request's signal sets. (A poll would let a request that
/// comes in its last interval go unnoticed: the statement ends, and the next message read
/// discards the request, as ProcessInterrupts does while DoingCommandRead.)
pub fn latch_sleep(flag: &AtomicBool, d: Duration) {
    let key = flag as *const AtomicBool as usize;
    let latch: Latch = Arc::new((Mutex::new(()), Condvar::new()));
    LATCHES.lock().entry(key).or_default().push(latch.clone());
    struct Registered(usize, Latch);
    impl Drop for Registered {
        fn drop(&mut self) {
            let mut all = LATCHES.lock();
            if let Some(v) = all.get_mut(&self.0) {
                v.retain(|l| !Arc::ptr_eq(l, &self.1));
                if v.is_empty() {
                    all.remove(&self.0);
                }
            }
        }
    }
    let _registered = Registered(key, latch.clone());
    let mut g = latch.0.lock();
    if !flag.load(Ordering::Relaxed) {
        latch.1.wait_for(&mut g, d);
    }
}

/// Whether `key` is a unique key's lock (`UNIQUE_KEY`).
fn is_unique_key(key: &[u8]) -> bool {
    key.len() >= 8 && key[..7] == [0; 7] && key[7] == UNIQUE_KEY
}

/// What a compact unique key keeps of its row's storage key: the part after the table's
/// heap prefix, which the lock key already names (its table's oid follows the tag); none
/// when the row key isn't under it.
fn compact_row_payload<'a>(key: &[u8], row_key: Option<&'a [u8]>) -> &'a [u8] {
    match (key.get(8..12), row_key) {
        (Some(table), Some(r)) if r.len() > 8 && r[..4] == *table && r[4..8] == [0; 4] => &r[8..],
        _ => &[],
    }
}

/// The row key a compact unique key's payload stands for (`compact_row_payload`).
fn compact_row_key(key: &[u8], payload: &[u8]) -> Option<Vec<u8>> {
    if payload.is_empty() {
        return None;
    }
    let mut r = key.get(8..12)?.to_vec();
    r.extend_from_slice(&[0; 4]);
    r.extend_from_slice(payload);
    Some(r)
}

/// Locks taken into compact sets so far (row locks and unique keys; tests read it).
pub static COMPACT_HOLDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The lock table. Lock order, for what takes more than one of its mutexes: `excl`, then
/// partitions in ascending order, then (under a partition) a backend's fast-path slot, the
/// compact sets (`bulk`) or `misc`; never a partition after any of those.
pub struct LockManager {
    parts: Box<[PartLock]>,
    bulk: Mutex<Bulk>,
    /// How many transactions have compact sets (`bulk`'s length, kept under its mutex): a row
    /// or unique key is decided by its partition alone while there are none. (A transaction
    /// adds a row or key to its set under that key's partition, which a check holds.)
    bulk_owners: AtomicUsize,
    /// Exclusion constraints' entries in progress, by constraint and writing backend: their
    /// lock (`EXCL_ENTRY`), row key and writer. An entry whose lock is no longer held is gone
    /// (dropped lazily). By backend, so a check reads only the other backends' entries: a
    /// statement's own thousands of new rows used to be filtered for each of them.
    excl: Mutex<HashMap<u32, HashMap<u64, ExclList>>>,
    misc: Mutex<Misc>,
    fast: FastPath,
}

#[derive(Default)]
struct Misc {
    /// Autovacuum workers' backends, with what cancels each one's work.
    autovacuum: HashMap<u64, std::sync::Arc<AtomicBool>>,
    /// Prepared transactions' dummy backends (`transfer`).
    prepared: HashSet<u64>,
}

impl Default for LockManager {
    fn default() -> Self {
        LockManager {
            parts: (0..PARTS).map(|_| PartLock::default()).collect(),
            bulk: Default::default(),
            bulk_owners: AtomicUsize::new(0),
            excl: Default::default(),
            misc: Default::default(),
            fast: FastPath::default(),
        }
    }
}

/// One heavyweight lock as pg_locks shows it.
#[derive(Clone, Debug)]
pub struct LockInfo {
    pub tag: Tag,
    pub pid: Option<i32>,
    pub owner: u64,
    pub mode: LockMode,
    pub granted: bool,
    pub fastpath: bool,
    pub wait_start: Option<Instant>,
}

/// The backend of a lock owner: a session's own owner carries its pid (`SessionState::
/// lock_owner`); a transaction's is its session's (`peer`), or itself without one.
pub fn proc_of(owner: u64, peer: Option<u64>) -> u64 {
    if owner >> 63 == 1 { owner } else { peer.unwrap_or(owner) }
}

fn pid_of(proc: u64) -> Option<i32> {
    (proc >> 63 == 1).then_some(proc as u32 as i32)
}

/// ProcSleep's log_lock_waits lines for the lock at `key` (in partition `part`) that `proc`
/// waits for in `mode`: `what` is "still waiting for", "acquired", "detected deadlock while
/// waiting for" or "avoided deadlock for" (whose message ends "by rearranging queue order");
/// `holders`: the DETAIL listing the lock's holders and wait queue.
fn log_wait(part: &Part, key: &[u8], proc: u64, mode: LockMode, start: Instant, what: &str, holders: bool) {
    let pid = |p: u64| pid_of(p).unwrap_or(0);
    let e = part.locks.get(key);
    // DescribeLockTag.
    let tag = match tag_of(key, e.map_or(0, |e| e.xid)) {
        Tag::Relation { db, rel } => format!("relation {rel} of database {db}"),
        Tag::Tuple { db, rel, .. } => format!("tuple of relation {rel} of database {db}"),
        Tag::Transaction { xid, .. } if xid != 0 => format!("transaction {xid}"),
        Tag::Transaction { id, .. } => format!("virtual transaction {}", super::procs::vxid_of_owner(id).unwrap_or_default()),
        Tag::Object { db, class, obj, .. } => format!("object {obj} of class {class} of database {db}"),
        Tag::SpecToken { xid, token } => format!("speculative token {token} of transaction {xid}"),
        Tag::ApplyTransaction { db, sub, xid, .. } => format!("remote transaction {xid} of subscription {sub} of database {db}"),
        Tag::Advisory(_) | Tag::Other(_) => "advisory lock".to_string(),
    };
    let us = start.elapsed().as_micros();
    let after = format!("after {}.{:03} ms", us / 1000, us % 1000);
    let me = pid(proc);
    let text = match what {
        "avoided deadlock for" => format!("process {me} avoided deadlock for {} on {tag} by rearranging queue order {after}", mode.name()),
        _ => format!("process {me} {what} {} on {tag} {after}", mode.name()),
    };
    let detail = holders.then(|| {
        let mut held: Vec<i32> = Vec::new();
        for h in e.map_or(&[][..], |e| &e.holds[..]) {
            if !held.contains(&pid(h.proc)) {
                held.push(pid(h.proc));
            }
        }
        let list = |v: &[i32]| v.iter().map(i32::to_string).collect::<Vec<_>>().join(", ");
        let queue: Vec<i32> = e.map_or(&[][..], |e| &e.queue[..]).iter().map(|p| pid(*p)).collect();
        let (one, many) = ("Process holding the lock", "Processes holding the lock");
        format!("{}: {}. Wait queue: {}.", if held.len() == 1 { one } else { many }, list(&held), list(&queue))
    });
    crate::log::backend(&crate::log::Message { detail: detail.as_deref(), ..crate::log::Message::log(&text) });
}

enum Deadlock {
    None,
    Soft,
    /// The cycle: (backend, lock key, mode it waits for).
    Hard(Vec<(u64, Vec<u8>, LockMode)>),
}

/// The partitions and the fast path (`FastSlot`).
impl LockManager {
    fn lock_part(&self, i: usize) -> PartGuard<'_> {
        #[cfg(debug_assertions)]
        order::part(i);
        PartGuard {
            g: self.parts[i].0.lock(),
            #[cfg(debug_assertions)]
            i,
        }
    }

    /// The partition of `key`, locked.
    fn lock_key(&self, key: &[u8]) -> PartGuard<'_> {
        self.lock_part(part_of(key))
    }

    fn lock_all(&self) -> All<'_> {
        All((0..PARTS).map(|i| self.lock_part(i)).collect())
    }

    fn bulk(&self) -> Leaf<'_, Bulk> {
        leaf(&self.bulk)
    }

    fn misc(&self) -> Leaf<'_, Misc> {
        leaf(&self.misc)
    }

    fn excl(&self) -> MutexGuard<'_, HashMap<u32, HashMap<u64, ExclList>>> {
        #[cfg(debug_assertions)]
        order::excl();
        self.excl.lock()
    }

    /// Run `f` on each item in its key's partition, each partition locked once.
    fn by_part<T>(&self, items: impl IntoIterator<Item = T>, key: impl Fn(&T) -> &[u8], mut f: impl FnMut(&mut Part, T)) {
        let mut v: SmallVec<[(usize, T); 8]> = items.into_iter().map(|t| (part_of(key(&t)), t)).collect();
        v.sort_by_key(|(p, _)| *p);
        let mut it = v.into_iter().peekable();
        while let Some((p, t)) = it.next() {
            let mut g = self.lock_part(p);
            f(&mut *g, t);
            while let Some((_, t)) = it.next_if(|(q, _)| *q == p) {
                f(&mut *g, t);
            }
        }
    }

    /// Backend `proc`'s fast-path slot (a session's backend only: a transaction without one
    /// has a backend of its own).
    pub fn fast_slot(&self, proc: u64) -> Option<Arc<FastSlot>> {
        if proc >> 63 == 0 {
            return None;
        }
        if let Some(s) = self.fast.slots.read().get(&proc) {
            return Some(s.clone());
        }
        let mut slots = self.fast.slots.write();
        Some(
            slots
                .entry(proc)
                .or_insert_with(|| {
                    Arc::new(FastSlot { proc, holds: Mutex::new(FastHolds::default()), xact: AtomicU64::new(0) })
                })
                .clone(),
        )
    }

    /// Backend `proc` is gone: its slot too.
    pub fn end_proc(&self, proc: u64) {
        let Some(slot) = self.fast.slots.write().remove(&proc) else { return };
        if !leaf(&slot.holds).holds.is_empty() {
            // (Not expected: what it still held goes to the main table.)
            let mut all = self.lock_all();
            Self::move_slot_all(&mut all, &slot);
        }
    }

    /// Lock `key` in `mode` for `owner` on the fast path, when it can be (a weak lock of a
    /// relation of the database, no strong one held or asked for in its partition).
    pub fn acquire_fast(&self, slot: &FastSlot, key: &[u8], owner: u64, mode: LockMode) -> bool {
        if mode > LockMode::RowExclusive || !is_local_relation(key) {
            return false;
        }
        let mut fast = leaf(&slot.holds);
        let holds = &mut fast.holds;
        if self.fast.strong[fast_partition(key)].load(Ordering::SeqCst) != 0 {
            return false;
        }
        match holds.iter_mut().find(|h| h.owner == owner && h.mode == mode && h.key == key) {
            Some(h) => h.count += 1,
            None => holds.push(FastHold { key: key.to_vec(), owner, mode, count: 1, seq: next_seq() }),
        }
        true
    }

    /// Release one of `owner`'s fast-path holds of `key` in `mode`; false when it has none
    /// there (it is in the main table).
    pub fn release_fast(&self, slot: &FastSlot, key: &[u8], owner: u64, mode: LockMode) -> bool {
        let mut fast = leaf(&slot.holds);
        let holds = &mut fast.holds;
        let Some(i) = holds.iter().position(|h| h.owner == owner && h.mode == mode && h.key == key) else {
            return false;
        };
        holds[i].count -= 1;
        if holds[i].count == 0 {
            holds.swap_remove(i);
        }
        true
    }

    /// Release every fast-path hold of `owner`; true when any of this owner's holds moved
    /// to the main table meanwhile (they are released there).
    pub fn release_fast_owner(&self, slot: &FastSlot, owner: u64) -> bool {
        let mut fast = leaf(&slot.holds);
        let holds = &mut fast.holds;
        holds.retain(|h| h.owner != owner);
        if slot.xact.load(Ordering::Relaxed) == owner {
            slot.xact.store(0, Ordering::Relaxed);
        }
        fast.moved.remove(&owner)
    }

    /// Hold transaction `owner`'s own lock (`hold_xact_lock` in Exclusive mode, before its
    /// first snapshot is registered) on backend `slot`'s fast path, as PostgreSQL's
    /// VirtualXactLockTableInsert does: nothing of the main table is locked until another
    /// backend asks for it (`acquire` moves it there first).
    pub fn hold_xact_fast(&self, slot: &FastSlot, owner: u64) {
        let mut fast = leaf(&slot.holds);
        let holds = &mut fast.holds;
        holds.push(FastHold { key: xact_key(owner), owner, mode: LockMode::Exclusive, count: 1, seq: next_seq() });
        slot.xact.store(owner, Ordering::Release);
    }

    /// Move transaction `owner`'s own lock (`key`, of partition `part`) from the fast path
    /// of the backend holding it, if one does, into the main table.
    fn move_fast_xact(&self, part: &mut Part, key: &[u8], owner: u64) {
        let slots = self.fast.slots.read();
        for slot in slots.values().filter(|sl| sl.xact.load(Ordering::Acquire) == owner) {
            Self::move_slot_key(part, slot, key);
        }
    }

    /// Move the fast-path holds of relation `key` (of partition `part`) into the main table,
    /// in the order they were taken among the holds there.
    fn move_fast_key(&self, part: &mut Part, key: &[u8]) {
        let slots: Vec<Arc<FastSlot>> = self.fast.slots.read().values().cloned().collect();
        for slot in slots {
            Self::move_slot_key(part, &slot, key);
        }
    }

    /// Move every fast-path hold into the main table.
    fn move_fast_all(&self, all: &mut All<'_>) {
        let slots: Vec<Arc<FastSlot>> = self.fast.slots.read().values().cloned().collect();
        for slot in slots {
            Self::move_slot_all(all, &slot);
        }
    }

    fn move_slot_key(part: &mut Part, slot: &FastSlot, key: &[u8]) {
        let mut fast = leaf(&slot.holds);
        if !fast.holds.iter().any(|h| h.key == key) {
            return;
        }
        let (moved, kept): (Vec<FastHold>, Vec<FastHold>) = std::mem::take(&mut fast.holds).into_iter().partition(|h| h.key == key);
        fast.holds = kept;
        for h in moved {
            fast.moved.insert(h.owner);
            Self::put_moved(part, slot.proc, h);
        }
    }

    fn move_slot_all(all: &mut All<'_>, slot: &FastSlot) {
        let mut fast = leaf(&slot.holds);
        for h in std::mem::take(&mut fast.holds) {
            fast.moved.insert(h.owner);
            let p = part_of(&h.key);
            Self::put_moved(&mut all.0[p], slot.proc, h);
        }
    }

    /// A fast-path hold of backend `proc`, moved into its key's partition `part`.
    fn put_moved(part: &mut Part, proc: u64, h: FastHold) {
        let e = part.locks.entry(h.key).or_default();
        if let Some(x) = e.holds.iter_mut().find(|x| x.owner == h.owner && x.proc == proc && x.mode == h.mode) {
            x.count += h.count;
            return;
        }
        let at = e.holds.iter().position(|x| x.seq > h.seq).unwrap_or(e.holds.len());
        let hold = Hold { owner: h.owner, proc, mode: h.mode, count: h.count, fastpath: true, seq: h.seq };
        e.holds.insert(at, hold);
    }
}

impl LockManager {
    /// Acquire heavyweight lock `key` in `mode` for `owner`, of backend `proc`. Ok(false)
    /// only for SKIP LOCKED when it isn't free; NOWAIT errors with "could not obtain lock on
    /// {what}".
    pub fn acquire(
        &self,
        key: &[u8],
        owner: u64,
        proc: u64,
        mode: LockMode,
        w: &Waits<'_>,
        what: &dyn Fn() -> String,
    ) -> Result<bool> {
        let strong = &self.fast.strong[..];
        let mut g = self.lock_key(key);
        // (A compact unique-key lock someone else asks for goes to the main table first.)
        if is_unique_key(key) && self.bulk_owners.load(Ordering::SeqCst) != 0 {
            materialize(&mut g, &mut self.bulk(), key);
        }
        // A strong relation lock: counted for the fast path, whose holds of the relation move
        // here (FastPathTransferRelationLocks).
        if strong_relation_lock(key, mode) {
            g.mark_strong(key, strong);
            self.move_fast_key(&mut g, key);
        }
        // A transaction's own lock may be on its backend's fast path.
        if let Some(id) = xact_key_owner(key) {
            self.move_fast_xact(&mut g, key, id);
        }
        let Part { locks, waiters, .. } = &mut *g;
        let wait_mask = locks.get(key).map_or(0, |e| {
            e.queue.iter().filter_map(|p| waiters.get(p)).fold(0, |m, w| m | w.mode.bit())
        });
        if !locks.contains_key(key) {
            locks.insert(key.to_vec(), Entry::default());
        }
        let e = locks.get_mut(key).unwrap();
        // A strong relation lock moves the fast-path holds into the main table.
        if mode > LockMode::ShareUpdateExclusive {
            for h in &mut e.holds {
                h.fastpath = false;
            }
        }
        // Held in this mode already (by this owner or another of the backend's): granted.
        if e.holds.iter().any(|h| h.proc == proc && h.mode == mode) {
            e.grant(owner, proc, mode, key);
            return Ok(true);
        }
        if mode.conflicts() & wait_mask == 0 && !e.conflicts(proc, mode) {
            e.grant(owner, proc, mode, key);
            return Ok(true);
        }
        // JoinWaitQueue.
        let my_held = e.held_mask(proc);
        let mut insert_at = None;
        if my_held != 0 {
            let mut ahead = 0u16;
            let queue = e.queue.clone();
            for (i, wp) in queue.iter().enumerate() {
                let Some(other) = g.waiters.get(wp) else { continue };
                if other.mode.conflicts() & my_held != 0 {
                    if mode.conflicts() & other.held != 0 {
                        // RememberSimpleDeadLock: each waits for the other.
                        let cycle = vec![
                            (proc, key.to_vec(), mode),
                            (*wp, other.key.clone(), other.mode),
                        ];
                        let msg = self.report(|k| g.xid_of(k), &cycle);
                        g.drop_empty(key, strong);
                        return Err(msg);
                    }
                    let e = g.locks.get_mut(key).unwrap();
                    if mode.conflicts() & ahead == 0 && !e.conflicts(proc, mode) {
                        e.grant(owner, proc, mode, key);
                        return Ok(true);
                    }
                    insert_at = Some(i);
                    break;
                }
                ahead |= other.mode.bit();
            }
        }
        match w.policy {
            WaitPolicy::NoWait => {
                g.drop_empty(key, strong);
                return Err(Error::new(
                    code::LOCK_NOT_AVAILABLE,
                    format!("could not obtain lock on {}", what()),
                ));
            }
            WaitPolicy::SkipLocked => {
                g.drop_empty(key, strong);
                return Ok(false);
            }
            WaitPolicy::Block => {}
        }
        // An autovacuum worker holding a conflicting lock is canceled
        // (GetBlockingAutoVacuumPgproc: PostgreSQL's deadlock checker does it after
        // deadlock_timeout; it is done at once here, so the wait is shorter).
        {
            let misc = self.misc();
            if !misc.autovacuum.is_empty() {
                for h in &g.locks[key].holds {
                    if h.proc != proc
                        && h.mode.conflicts_with(mode)
                        && let Some(c) = misc.autovacuum.get(&h.proc)
                    {
                        c.store(true, Ordering::Relaxed);
                        interrupt(c);
                    }
                }
            }
        }
        let e = g.locks.get_mut(key).unwrap();
        match insert_at {
            Some(i) => e.queue.insert(i, proc),
            None => e.queue.push(proc),
        }
        let start = Instant::now();
        let cv = Arc::new(Condvar::new());
        g.waiters.insert(
            proc,
            Waiter {
                key: key.to_vec(),
                owner,
                mode,
                held: my_held,
                status: Status::Waiting,
                since: start,
                granted_at: None,
                cv: cv.clone(),
            },
        );
        // (Registered for `interrupt` while it waits.)
        struct Sleeping(usize);
        impl Drop for Sleeping {
            fn drop(&mut self) {
                SLEEPING.lock().remove(&self.0);
            }
        }
        let flag = w.cancel as *const AtomicBool as usize;
        SLEEPING.lock().insert(flag, cv.clone());
        let _sleeping = Sleeping(flag);
        // What the statement said so far reaches the client before it waits.
        drop(g);
        crate::exec::flush_live_notices();
        let mut g = self.lock_key(key);
        // (Whether the last sleep ended by its timeout, not a wakeup: `LATE_GRANTS`.)
        let mut timed_out = false;
        let mut checked = false;
        // log_lock_waits (the statement carries a log context only when it is on).
        let log_waits = crate::log::logs_lock_waits();
        let mut logged = false;
        loop {
            if let Some(granted_at) = g.waiters.get(&proc).filter(|x| x.status == Status::Granted).map(|x| x.granted_at) {
                if timed_out && let Some(t) = granted_at {
                    let us = t.elapsed().as_micros() as u64;
                    MAX_TIMED_OUT_GRANT_US.fetch_max(us, Ordering::Relaxed);
                    if us >= 40_000 {
                        LATE_GRANTS.fetch_add(1, Ordering::Relaxed);
                    }
                }
                if logged {
                    log_wait(&g, key, proc, mode, start, "acquired", false);
                }
                g.waiters.remove(&proc);
                // A cancel that came while it slept was Postgres's interrupt of the wait, before
                // the grant: the lock goes back.
                if w.cancel.load(Ordering::Relaxed) {
                    drop(g);
                    self.release(key, owner, mode);
                    return Err(Error::new(code::QUERY_CANCELED, "canceling statement due to user request"));
                }
                return Ok(true);
            }
            let now = Instant::now();
            let fail = if w.cancel.load(Ordering::Relaxed) {
                Some(Error::new(
                    code::QUERY_CANCELED,
                    "canceling statement due to user request",
                ))
            } else if w.lock_timeout.is_some_and(|t| now >= start + t) {
                Some(Error::new(
                    code::LOCK_NOT_AVAILABLE,
                    "canceling statement due to lock timeout",
                ))
            } else if w.deadline.is_some_and(|d| now >= d) {
                Some(Error::new(
                    code::QUERY_CANCELED,
                    "canceling statement due to statement timeout",
                ))
            } else if w.txn_deadline.is_some_and(|d| now >= d) {
                Some(transaction_timeout())
            } else if !checked && now >= start + w.deadlock_timeout {
                checked = true;
                // DeadLockCheck reads and may reorder every partition's wait queues.
                drop(g);
                let mut all = self.lock_all();
                match self.deadlock_check(&mut all, proc) {
                    Deadlock::Hard(cycle) => {
                        if log_waits {
                            log_wait(all.get(key), key, proc, mode, start, "detected deadlock while waiting for", true);
                        }
                        let e = self.report(|k| all.xid_of(k), &cycle);
                        all.part(key).remove_waiter(proc, strong);
                        return Err(e);
                    }
                    Deadlock::Soft => {
                        if log_waits {
                            log_wait(all.get(key), key, proc, mode, start, "avoided deadlock for", true);
                            logged = true;
                            log_wait(all.get(key), key, proc, mode, start, "still waiting for", true);
                        }
                        // (Queues reordered: whoever that lets in was woken.)
                    }
                    Deadlock::None => {
                        // (Not when it was granted meanwhile.)
                        if log_waits && all.get(key).waiters.get(&proc).is_some_and(|w| w.status == Status::Waiting) {
                            logged = true;
                            log_wait(all.get(key), key, proc, mode, start, "still waiting for", true);
                        }
                    }
                }
                drop(all);
                g = self.lock_key(key);
                timed_out = false;
                continue;
            } else {
                None
            };
            if let Some(e) = fail {
                g.remove_waiter(proc, strong);
                drop(g);
                return Err(e);
            }
            let mut wake = now + Duration::from_millis(50);
            if !checked {
                wake = wake.min(start + w.deadlock_timeout);
            }
            if let Some(t) = w.lock_timeout {
                wake = wake.min(start + t);
            }
            if let Some(d) = w.deadline {
                wake = wake.min(d);
            }
            if let Some(d) = w.txn_deadline {
                wake = wake.min(d);
            }
            timed_out = cv.wait_until(&mut g.g, wake.max(now + Duration::from_millis(1))).timed_out();
        }
    }

    /// Backend `proc` is an autovacuum worker whose work `cancel` cancels (None: no more).
    pub fn set_autovacuum(&self, proc: u64, cancel: Option<std::sync::Arc<AtomicBool>>) {
        let mut misc = self.misc();
        match cancel {
            Some(c) => misc.autovacuum.insert(proc, c),
            None => misc.autovacuum.remove(&proc),
        };
    }

    /// Release one count of `owner`'s hold of `mode`.
    pub fn release(&self, key: &[u8], owner: u64, mode: LockMode) {
        self.lock_key(key).release_one(key, owner, mode, &self.fast.strong);
    }

    /// Release everything `owner` holds of these heavyweight locks.
    pub fn release_all(&self, owner: u64, keys: impl IntoIterator<Item = Vec<u8>>) {
        self.release_with_rows(owner, keys, std::iter::empty());
    }

    /// Release `owner`'s holds of the unique keys a compact segment moved to the main table
    /// (one count each: a later lock of the key by the transaction is its own).
    fn release_moved(&self, owner: u64, moved: Vec<Vec<u8>>) {
        let strong = &self.fast.strong[..];
        self.by_part(moved, |k| k.as_slice(), |p, k| p.release_one(&k, owner, LockMode::Exclusive, strong));
    }

    /// A transaction ending: its row locks on `rows`, then its heavyweight locks (its
    /// transaction locks last, so waiters see the rows free when they wake).
    pub fn release_with_rows(
        &self,
        owner: u64,
        keys: impl IntoIterator<Item = Vec<u8>>,
        rows: impl IntoIterator<Item = Vec<u8>>,
    ) {
        let strong = &self.fast.strong[..];
        if self.bulk_owners.load(Ordering::SeqCst) != 0 {
            let removed = {
                let mut bulk = self.bulk();
                let b = bulk.remove(&owner);
                self.bulk_owners.store(bulk.len(), Ordering::SeqCst);
                b
            };
            for g in removed.into_iter().flat_map(|b| b.segs) {
                self.release_moved(owner, g.moved);
            }
        }
        self.by_part(rows, |r| r.as_slice(), |p, r| {
            if let Some(holds) = p.rows.get_mut(&r) {
                holds.retain(|h| h.owner != owner);
                if holds.is_empty() {
                    p.rows.remove(&r);
                }
            }
        });
        let (xacts, others): (Vec<Vec<u8>>, Vec<Vec<u8>>) = keys.into_iter().partition(|k| xact_key_owner(k).is_some());
        self.by_part(others, |k| k.as_slice(), |p, k| p.release_owner(&k, owner, strong));
        self.by_part(xacts, |k| k.as_slice(), |p, k| p.release_owner(&k, owner, strong));
    }

    /// Subtransactions aborting: their row locks on `rows` go, then their transaction locks.
    pub fn release_subxact(&self, owner: u64, holders: &[u64], rows: &[Vec<u8>]) {
        let strong = &self.fast.strong[..];
        if self.bulk_owners.load(Ordering::SeqCst) != 0 {
            let gone = {
                let mut bulk = self.bulk();
                match bulk.get_mut(&owner) {
                    Some(b) => {
                        let (gone, kept): (Vec<_>, Vec<_>) =
                            std::mem::take(&mut b.segs).into_iter().partition(|g| holders.contains(&g.holder));
                        b.segs = kept;
                        gone
                    }
                    None => Vec::new(),
                }
            };
            for g in gone {
                self.release_moved(owner, g.moved);
            }
        }
        self.by_part(rows, |r| r.as_slice(), |p, r| {
            if let Some(holds) = p.rows.get_mut(r) {
                holds.retain(|h| !(h.owner == owner && holders.contains(&h.holder)));
                if holds.is_empty() {
                    p.rows.remove(r);
                }
            }
        });
        self.by_part(holders.iter().map(|id| xact_key(*id)), |k| k.as_slice(), |p, k| p.release_owner(&k, owner, strong));
    }

    /// Hold (sub)transaction `holder`'s own lock from when it gets a transaction id
    /// (`xid`), so others can wait for it to end. (Before any of its row or compact locks
    /// is taken: whoever finds one of those waits for this lock.)
    pub fn hold_xact_lock(&self, holder: u64, owner: u64, proc: u64, xid: u64) {
        self.lock_key(&xact_key(holder)).hold_xact(holder, owner, proc, xid);
    }

    /// Lock row `row` (its data key) in `mode` for (sub)transaction `holder` of top-level
    /// transaction `owner`, as heap_lock_tuple does: Ok(false) only for SKIP LOCKED. `tuple_of`
    /// gives the row's heavyweight tuple lock key; `xid` the holder's transaction id. The
    /// holder's transaction lock (`xact_key(holder)`) must be held already (`hold_xact_lock`).
    /// With `compact`, a new hold may go into the transaction's compact set (`bulklocks`),
    /// which it is told: such a hold is released with the transaction (or its
    /// subtransaction), not by row.
    #[allow(clippy::too_many_arguments)]
    pub fn lock_row(
        &self,
        row: &[u8],
        tuple_of: &dyn Fn() -> Vec<u8>,
        holder: u64,
        owner: u64,
        proc: u64,
        xid: u64,
        mode: RowLockMode,
        w: &Waits<'_>,
        what: &dyn Fn() -> String,
        mut compact: Option<&mut bool>,
        xact_ctx: &dyn Fn() -> Option<String>,
    ) -> Result<bool> {
        // (The tuple lock's key, made only for a wait.)
        let tuple_key = std::cell::OnceCell::new();
        let tuple = || tuple_key.get_or_init(tuple_of).as_slice();
        let mut have_tuple_lock = false;
        let mut first = true;
        let mut skip_tuple_lock = false;
        let release_tuple = |this: &Self, have: bool| {
            if have {
                this.release(tuple(), owner, mode.heavy());
            }
        };
        let pi = part_of(row);
        loop {
            let mut g = self.lock_part(pi);
            // (A bulk statement's next row: no hold in the main table, and only this
            // transaction has compact ones, so nothing can conflict: its compact set takes it
            // without the holds listed.)
            if first && compact.is_some() && !g.rows.contains_key(row) {
                let mut bulk = self.bulk();
                if bulk.keys().all(|&o| o == owner) {
                    // (One probe when the transaction's only compact set is this holder's, as
                    // without savepoints.)
                    if let Some(b) = bulk.get_mut(&owner)
                        && let [sg] = &mut b.segs[..]
                        && sg.holder == holder
                        && sg.xid != 0
                    {
                        let mut prior = None;
                        if sg.rows.upsert(row, |t| {
                            prior = t;
                            t.unwrap_or(row_tag(mode, false))
                        }) {
                            match prior.map(|t| untag(t).0) {
                                None => {
                                    if let Some(flag) = compact.as_deref_mut() {
                                        *flag = true;
                                    }
                                    COMPACT_HOLDS.fetch_add(1, Ordering::Relaxed);
                                    return Ok(true);
                                }
                                Some(m) if m >= mode => return Ok(true),
                                // (A weaker hold of ours: promoted the usual way.)
                                Some(_) => {}
                            }
                        }
                    }
                    let held = bulk.get(&owner).and_then(|b| b.segs.iter().filter_map(|g| g.rows.get(row)).map(|t| untag(t).0).max());
                    if held.is_some_and(|m| m >= mode) {
                        return Ok(true);
                    }
                    if held.is_none() {
                        let b = bulk.entry(owner).or_insert_with(|| BulkLocks { proc, segs: Vec::new() });
                        let done = b.seg(holder, xid).rows.upsert(row, |_| row_tag(mode, false));
                        self.bulk_owners.store(bulk.len(), Ordering::SeqCst);
                        if done {
                            if let Some(flag) = compact.as_deref_mut() {
                                *flag = true;
                            }
                            COMPACT_HOLDS.fetch_add(1, Ordering::Relaxed);
                            return Ok(true);
                        }
                    }
                }
            }
            let bulk = if self.bulk_owners.load(Ordering::SeqCst) == 0 {
                Vec::new()
            } else {
                bulk_row_holds(&self.bulk(), row)
            };
            let holds = || g.rows.get(row).map_or(&[][..], |h| h.as_slice()).iter().chain(&bulk);
            if first {
                first = false;
                let mine: SmallVec<[&RowHold; 2]> = holds().filter(|h| h.owner == owner).collect();
                if mine.iter().any(|h| h.mode >= mode) {
                    // Held at least this strongly by this transaction already.
                    return Ok(true);
                }
                // Promoting a weaker lock: no tuple lock, or we'd deadlock with a waiter
                // holding it and waiting for us.
                skip_tuple_lock = !mine.is_empty();
            }
            let conflicting: SmallVec<[u64; 4]> = holds()
                .filter(|h| h.owner != owner && h.mode.conflicts_with(mode))
                .map(|h| h.holder)
                .collect();
            if conflicting.is_empty() {
                // (A holder's lock stays where it is; a new one goes compact when it may.)
                let in_main = g.rows.get(row).is_some_and(|h| h.iter().any(|h| h.holder == holder));
                if !in_main && let Some(flag) = compact.as_deref_mut() {
                    let mut bulk = self.bulk();
                    let b = bulk.entry(owner).or_insert_with(|| BulkLocks { proc, segs: Vec::new() });
                    let sg = b.seg(holder, xid);
                    let held = |t: Option<u8>| match t.map(untag) {
                        Some((h, modifies)) => row_tag(h.max(mode), modifies),
                        None => row_tag(mode, false),
                    };
                    let done = sg.rows.upsert(row, held);
                    self.bulk_owners.store(bulk.len(), Ordering::SeqCst);
                    if done {
                        *flag = true;
                        COMPACT_HOLDS.fetch_add(1, Ordering::Relaxed);
                        drop(bulk);
                        drop(g);
                        release_tuple(self, have_tuple_lock);
                        return Ok(true);
                    }
                }
                match g.rows.get_mut(row) {
                    Some(holds) => match holds.iter_mut().find(|h| h.holder == holder) {
                        Some(h) => h.mode = h.mode.max(mode),
                        None => holds.push(RowHold { holder, owner, proc, mode, modifies: false }),
                    },
                    None => {
                        let hold = RowHold { holder, owner, proc, mode, modifies: false };
                        g.rows.insert(row.to_vec(), smallvec::smallvec![hold]);
                    }
                }
                drop(g);
                release_tuple(self, have_tuple_lock);
                return Ok(true);
            }
            drop(g);
            match w.policy {
                WaitPolicy::NoWait if !have_tuple_lock && !skip_tuple_lock => {}
                WaitPolicy::NoWait => {
                    release_tuple(self, have_tuple_lock);
                    return Err(Error::new(
                        code::LOCK_NOT_AVAILABLE,
                        format!("could not obtain lock on row in relation \"{}\"", what()),
                    ));
                }
                WaitPolicy::SkipLocked if have_tuple_lock || skip_tuple_lock => {
                    release_tuple(self, have_tuple_lock);
                    return Ok(false);
                }
                _ => {}
            }
            if !have_tuple_lock && !skip_tuple_lock {
                let got = match self.acquire(tuple(), owner, proc, mode.heavy(), w, &|| {
                    format!("row in relation \"{}\"", what())
                }) {
                    Ok(g) => g,
                    Err(e) if e.code == code::LOCK_NOT_AVAILABLE && w.policy == WaitPolicy::NoWait => {
                        return Err(Error::new(
                            code::LOCK_NOT_AVAILABLE,
                            format!("could not obtain lock on row in relation \"{}\"", what()),
                        ));
                    }
                    Err(e) => return Err(e),
                };
                if !got {
                    return Ok(false);
                }
                have_tuple_lock = true;
                continue;
            }
            // Wait for the conflicting (sub)transactions to end.
            for h in conflicting {
                let k = xact_key(h);
                let wait = w.with_policy(match w.policy {
                    WaitPolicy::Block => WaitPolicy::Block,
                    _ => WaitPolicy::NoWait,
                });
                match self.acquire(&k, owner, proc, LockMode::Share, &wait, &String::new) {
                    Ok(_) => self.release(&k, owner, LockMode::Share),
                    Err(e) if e.code == code::LOCK_NOT_AVAILABLE && w.policy != WaitPolicy::Block => {
                        release_tuple(self, have_tuple_lock);
                        if w.policy == WaitPolicy::SkipLocked {
                            return Ok(false);
                        }
                        return Err(Error::new(
                            code::LOCK_NOT_AVAILABLE,
                            format!("could not obtain lock on row in relation \"{}\"", what()),
                        ));
                    }
                    Err(e) => {
                        release_tuple(self, have_tuple_lock);
                        // XactLockTableWait's error context callback ("while updating
                        // tuple ..."): only this wait has it, not the tuple lock's.
                        return Err(e.inner_context(xact_ctx()));
                    }
                }
            }
        }
    }

    /// GetLockConflicts: the transactions (owners) of other backends than `proc` holding
    /// heavyweight lock `key` in a mode conflicting with `mode`.
    pub fn conflicting_owners(&self, key: &[u8], mode: LockMode, proc: u64) -> Vec<u64> {
        let mut g = self.lock_key(key);
        self.move_fast_key(&mut g, key);
        let mut out = Vec::new();
        if let Some(e) = g.locks.get(key) {
            for h in &e.holds {
                if h.proc != proc && h.owner >> 63 == 0 && mode.conflicts_with(h.mode) && !out.contains(&h.owner) {
                    out.push(h.owner);
                }
            }
        }
        out
    }

    /// Record what transaction `owner` (holding unique key lock `key`) put in the index.
    pub fn set_key_holder(&self, key: &[u8], owner: u64, kh: KeyHolder) {
        let mut g = self.lock_key(key);
        if let Some(e) = g.locks.get_mut(key)
            && e.holds.iter().any(|h| h.owner == owner)
        {
            e.key_holder = Some((owner, kh));
        }
    }

    /// Whether lock `key` is held by anyone (an exclusion entry's lock: its writer runs).
    fn held(&self, key: &[u8]) -> bool {
        self.lock_key(key).locks.get(key).is_some_and(|e| !e.holds.is_empty())
    }

    /// Exclusion constraint `con`'s entry for row `row` is in progress: lock `key`, just
    /// taken by `owner`, stands for it until its writer `kh` ends.
    pub fn add_excl_entry(&self, con: u32, key: &[u8], row: &[u8], owner: u64, kh: KeyHolder) {
        let mut excl = self.excl();
        let proc = self.lock_key(key).locks.get(key).and_then(|e| e.holds.iter().find(|h| h.owner == owner)).map(|h| h.proc);
        let Some(proc) = proc else { return };
        let list = excl.entry(con).or_default().entry(proc).or_default();
        list.entries.push((key.to_vec(), row.to_vec(), kh));
        // (Its ended entries dropped as the list doubles: amortized, not per entry.)
        if list.entries.len() >= list.prune_at {
            list.entries.retain(|(k, _, _)| self.held(k));
            list.prune_at = (list.entries.len() * 2).max(64);
        }
    }

    /// A speculative insertion's entry `key` of exclusion constraint `con` is complete: it
    /// stands for its (sub)transaction from now on.
    pub fn settle_excl_entry(&self, con: u32, key: &[u8]) {
        let mut excl = self.excl();
        let procs: Vec<u64> = self.lock_key(key).locks.get(key).map(|e| e.holds.iter().map(|h| h.proc).collect()).unwrap_or_default();
        if let Some(by) = excl.get_mut(&con) {
            for p in procs {
                let Some(list) = by.get_mut(&p) else { continue };
                // (The entry just added is the last one, or near it.)
                for (_, _, kh) in list.entries.iter_mut().rev().filter(|(k, _, _)| k == key) {
                    kh.token = None;
                }
            }
        }
    }

    /// The entries in progress of exclusion constraint `con` that backends other than `proc`
    /// are writing: (row key, writer). (Ended writers' entries go.)
    pub fn excl_entries(&self, con: u32, proc: u64) -> Vec<(Vec<u8>, KeyHolder)> {
        let mut excl = self.excl();
        let Some(by) = excl.get_mut(&con) else { return Vec::new() };
        let mut out = Vec::new();
        by.retain(|p, list| {
            if *p == proc {
                return true;
            }
            list.entries.retain(|(k, row, kh)| {
                let g = self.lock_key(k);
                let Some(e) = g.locks.get(k).filter(|e| !e.holds.is_empty()) else { return false };
                if e.holds.iter().any(|h| h.proc != proc) {
                    out.push((row.clone(), kh.clone()));
                }
                true
            });
            !list.entries.is_empty()
        });
        if by.is_empty() {
            excl.remove(&con);
        }
        out
    }

    /// The in-progress entry another backend than `proc` holds unique key lock `key` for:
    /// what it recorded, or (when it recorded nothing) its top-level transaction.
    pub fn key_holder(&self, key: &[u8], proc: u64) -> Option<KeyHolder> {
        let g = self.lock_key(key);
        let Some(e) = g.locks.get(key) else {
            if self.bulk_owners.load(Ordering::SeqCst) == 0 {
                return None;
            }
            // A compact one: its (sub)transaction's in-progress entry.
            return self.bulk().values().filter(|b| b.proc != proc).find_map(|b| {
                b.segs.iter().find_map(|g| {
                    let row_key = g.ukeys.payload(key)?;
                    Some(KeyHolder {
                        holder: g.holder,
                        xid: g.xid,
                        token: None,
                        row: None,
                        row_key: compact_row_key(key, row_key),
                    })
                })
            });
        };
        let h = e.holds.iter().find(|h| h.proc != proc)?;
        match &e.key_holder {
            Some((owner, kh)) if e.holds.iter().any(|h| h.owner == *owner && h.proc != proc) => {
                Some(kh.clone())
            }
            _ => Some(KeyHolder { holder: h.owner, xid: 0, token: None, row: None, row_key: None }),
        }
    }

    /// Row `row`'s lock by transaction `owner` is now an update's or a delete's (the
    /// tuple's xmax is no longer lock-only): other writers' checks of its unique keys wait
    /// for it to end. (Its hold of `holder`'s, else any of the transaction's.)
    pub fn mark_row_modified(&self, row: &[u8], owner: u64, holder: u64) {
        let mut g = self.lock_key(row);
        let modified = |t: u8| row_tag(untag(t).0, true);
        // The hold of `holder`'s: in the main table, else in its compact set.
        if let Some(h) = g.rows.get_mut(row).and_then(|holds| holds.iter_mut().find(|h| h.owner == owner && h.holder == holder)) {
            h.modifies = true;
            return;
        }
        let mut bulk = (self.bulk_owners.load(Ordering::SeqCst) != 0).then(|| self.bulk());
        let segs = match bulk.as_mut().and_then(|b| b.get_mut(&owner)) {
            Some(b) => &mut b.segs[..],
            None => &mut [],
        };
        if let Some(sg) = segs.iter_mut().find(|sg| sg.holder == holder)
            && sg.rows.update(row, modified)
        {
            return;
        }
        // Else any of the transaction's.
        if let Some(h) = g.rows.get_mut(row).and_then(|holds| holds.iter_mut().find(|h| h.owner == owner)) {
            h.modifies = true;
            return;
        }
        for sg in segs.iter_mut().filter(|sg| sg.holder != holder) {
            if sg.rows.update(row, modified) {
                return;
            }
        }
    }

    /// Every hold of row `row` (of partition `part`): the main table's, then the compact sets'.
    fn all_row_holds(&self, part: &Part, row: &[u8]) -> Vec<RowHold> {
        let mut out: Vec<RowHold> = part.rows.get(row).map(|h| h.to_vec()).unwrap_or_default();
        if self.bulk_owners.load(Ordering::SeqCst) != 0 {
            out.extend(bulk_row_holds(&self.bulk(), row));
        }
        out
    }

    /// The (sub)transactions (lock ids) of other transactions than `owner` holding row
    /// `row` in a mode conflicting with `mode`.
    pub fn row_conflicts(&self, row: &[u8], owner: u64, mode: RowLockMode) -> Vec<u64> {
        let g = self.lock_key(row);
        self.all_row_holds(&g, row)
            .iter()
            .filter(|h| h.owner != owner && h.mode.conflicts_with(mode))
            .map(|h| h.holder)
            .collect()
    }

    /// The (sub)transaction of another backend than `proc` updating or deleting row `row`
    /// (HeapTupleSatisfiesDirty's in-progress xmax), if any.
    pub fn row_modifier(&self, row: &[u8], proc: u64) -> Option<u64> {
        let g = self.lock_key(row);
        if self.bulk_owners.load(Ordering::SeqCst) == 0 {
            return g.rows.get(row)?.iter().find(|h| h.modifies && h.proc != proc).map(|h| h.holder);
        }
        self.all_row_holds(&g, row)
            .iter()
            .find(|h| h.modifies && h.proc != proc)
            .map(|h| h.holder)
    }

    /// The row locks on `row` other transactions than `owner` hold: (mode, pid).
    pub fn row_holders(&self, row: &[u8], owner: u64) -> Vec<(RowLockMode, Option<i32>)> {
        let g = self.lock_key(row);
        self.all_row_holds(&g, row)
            .iter()
            .filter(|h| h.owner != owner)
            .map(|h| (h.mode, pid_of(h.proc)))
            .collect()
    }

    /// The strongest row lock `owner` holds on `row`.
    pub fn row_held(&self, row: &[u8], owner: u64) -> Option<RowLockMode> {
        let g = self.lock_key(row);
        self.all_row_holds(&g, row)
            .iter()
            .filter(|h| h.owner == owner)
            .map(|h| h.mode)
            .max()
    }

    /// pg_blocking_pids: the backends whose holds of the lock backend `pid` waits for
    /// conflict with its request, and those ahead of it in the queue wanting a conflicting
    /// mode.
    pub fn blocking_pids(&self, pid: i32) -> Vec<i32> {
        let all = self.lock_all();
        let prepared = self.misc().prepared.clone();
        let mut out = Vec::new();
        for part in &all.0 {
            for (proc, w) in &part.waiters {
                if pid_of(*proc) != Some(pid) || w.status != Status::Waiting {
                    continue;
                }
                let Some(e) = part.locks.get(&w.key) else { continue };
                for h in &e.holds {
                    if h.proc != *proc
                        && w.mode.conflicts_with(h.mode)
                        && let Some(p) = pid_of(h.proc).or_else(|| prepared.contains(&h.proc).then_some(0))
                        && !out.contains(&p)
                    {
                        out.push(p);
                    }
                }
                for q in &e.queue {
                    if q == proc {
                        break;
                    }
                    if let Some(other) = part.waiters.get(q)
                        && w.mode.conflicts_with(other.mode)
                        && let Some(p) = pid_of(*q)
                        && !out.contains(&p)
                    {
                        out.push(p);
                    }
                }
            }
        }
        out
    }

    /// Every heavyweight lock held or awaited (pg_locks).
    pub fn snapshot(&self) -> Vec<LockInfo> {
        let mut all = self.lock_all();
        self.move_fast_all(&mut all);
        let mut out = Vec::new();
        for part in &all.0 {
            for (k, e) in &part.locks {
                let tag = tag_of(k, e.xid);
                let first = out.len();
                for h in &e.holds {
                    if out[first..].iter().any(|i: &LockInfo| i.owner == h.owner && i.mode == h.mode) {
                        continue;
                    }
                    out.push(LockInfo {
                        tag: tag.clone(),
                        pid: pid_of(h.proc),
                        owner: h.owner,
                        mode: h.mode,
                        granted: true,
                        fastpath: h.fastpath,
                        wait_start: None,
                    });
                }
                // (pg_lock_status: one holder's modes of a lock in lockdefs.h order, whatever the
                // order they were taken in; holders in their first hold's order.)
                let held = &mut out[first..];
                let rank: Vec<u64> = held.iter().map(|i| i.owner).collect();
                held.sort_by_key(|i| (rank.iter().position(|o| *o == i.owner), i.mode));
                for p in &e.queue {
                    if let Some(w) = part.waiters.get(p) {
                        out.push(LockInfo {
                            tag: tag.clone(),
                            pid: pid_of(*p),
                            owner: w.owner,
                            mode: w.mode,
                            granted: false,
                            fastpath: false,
                            wait_start: Some(w.since),
                        });
                    }
                }
            }
        }
        out
    }

    /// The keys `owner` holds, with the modes (pg_locks).
    pub fn held_by(&self, owner: u64) -> Vec<(Vec<u8>, LockMode)> {
        let mut all = self.lock_all();
        self.move_fast_all(&mut all);
        let mut v = Vec::new();
        for part in &all.0 {
            for (k, e) in &part.locks {
                for h in e.holds.iter().filter(|h| h.owner == owner) {
                    v.push((k.clone(), h.mode));
                }
            }
        }
        v
    }

    /// PostPrepare_Locks: what transaction `owner` holds (heavyweight locks, row locks, its
    /// transaction locks) now belongs to its prepared transaction's dummy backend `proc`,
    /// which has no pid: its holds conflict with every session, the one that prepared it
    /// too, and pg_blocking_pids reports it as 0.
    pub fn transfer(&self, owner: u64, proc: u64) {
        let mut all = self.lock_all();
        self.move_fast_all(&mut all);
        self.misc().prepared.insert(proc);
        for part in all.0.iter_mut() {
            for e in part.locks.values_mut() {
                for h in e.holds.iter_mut().filter(|h| h.owner == owner) {
                    h.proc = proc;
                    h.fastpath = false;
                }
            }
            for holds in part.rows.values_mut() {
                for h in holds.iter_mut().filter(|h| h.owner == owner) {
                    h.proc = proc;
                }
            }
        }
        if let Some(b) = self.bulk().get_mut(&owner) {
            b.proc = proc;
        }
        for slot in self.fast.slots.read().values() {
            leaf(&slot.holds).moved.remove(&owner);
        }
    }

    /// A prepared transaction's dummy backend is gone (COMMIT / ROLLBACK PREPARED).
    pub fn forget_prepared(&self, proc: u64) {
        self.misc().prepared.remove(&proc);
    }

    /// AtPrepare_Locks' check: a lock `owner` holds that `peer` (its session's own owner)
    /// holds too.
    pub fn held_by_both(&self, owner: u64, peer: u64) -> bool {
        let mut all = self.lock_all();
        self.move_fast_all(&mut all);
        all.0.iter().any(|part| {
            part.locks.values().any(|e| e.holds.iter().any(|h| h.owner == owner) && e.holds.iter().any(|h| h.owner == peer))
        })
    }

    /// The row locks on `row` (pgrowlocks), in the order they were taken: (mode, whether an
    /// update's or delete's, the holder's transaction id, its backend's pid).
    pub fn row_lockers(&self, row: &[u8]) -> Vec<(RowLockMode, bool, u64, Option<i32>)> {
        let holds = {
            let g = self.lock_key(row);
            self.all_row_holds(&g, row)
        };
        holds
            .iter()
            .map(|h| {
                let k = xact_key(h.holder);
                let xid = self.lock_key(&k).xid_of(&k);
                (h.mode, h.modifies, xid, pid_of(h.proc))
            })
            .collect()
    }

    /// The row locks `owner` holds: (row, strongest mode, whether an update's or delete's).
    pub fn rows_of(&self, owner: u64) -> Vec<(Vec<u8>, RowLockMode, bool)> {
        let mut out = Vec::new();
        for i in 0..PARTS {
            let part = self.lock_part(i);
            for (row, holds) in &part.rows {
                let mut best: Option<(RowLockMode, bool)> = None;
                for h in holds.iter().filter(|h| h.owner == owner) {
                    let b = best.get_or_insert((h.mode, h.modifies));
                    b.0 = b.0.max(h.mode);
                    b.1 |= h.modifies;
                }
                if let Some((m, modifies)) = best {
                    out.push((row.clone(), m, modifies));
                }
            }
        }
        out
    }

    /// The unique key locks `owner` recorded an in-progress index entry for: (key, the
    /// entry's transaction id).
    pub fn key_holders_of(&self, owner: u64) -> Vec<(Vec<u8>, u64)> {
        let mut out = Vec::new();
        for i in 0..PARTS {
            let part = self.lock_part(i);
            out.extend(part.locks.iter().filter_map(|(k, e)| match &e.key_holder {
                Some((o, kh)) if *o == owner => Some((k.clone(), kh.xid)),
                _ => None,
            }));
        }
        out
    }

    /// A recovered prepared transaction's row lock (lock_row's grant, without waiting: at
    /// recovery nothing else holds any).
    pub fn restore_row(&self, row: &[u8], owner: u64, proc: u64, mode: RowLockMode, modifies: bool) {
        let mut g = self.lock_key(row);
        g.rows.entry(row.to_vec()).or_default().push(RowHold {
            holder: owner,
            owner,
            proc,
            mode,
            modifies,
        });
    }

    /// Whether anyone holds a lock (for idle detection and tests).
    pub fn held_count(&self) -> usize {
        let fast: usize = self.fast.slots.read().values().map(|f| leaf(&f.holds).holds.len()).sum();
        let main: usize = (0..PARTS)
            .map(|i| {
                let g = self.lock_part(i);
                g.locks.len() + g.rows.len()
            })
            .sum();
        fast + main + self.bulk().values().map(BulkLocks::len).sum::<usize>()
    }

    /// Take unique key lock `key` (Exclusive) for (sub)transaction `holder` (transaction id
    /// `xid`) of transaction `owner` into its compact set, standing for its in-progress index
    /// entry; `holder`'s transaction lock must be held already. Only while nobody else holds
    /// or wants the key: false, take it the usual way (`acquire`, which first moves another's
    /// compact hold into the main table). `row_key`: the entry's heap tuple (named by the
    /// CONTEXT of a wait for it).
    pub fn hold_compact_unique_key(
        &self,
        key: &[u8],
        owner: u64,
        proc: u64,
        holder: u64,
        xid: u64,
        row_key: Option<&[u8]>,
    ) -> bool {
        let g = self.lock_key(key);
        if g.locks.contains_key(key) {
            return false;
        }
        let mut bulk = self.bulk();
        if bulk.values().any(|b| b.segs.iter().any(|g| g.ukeys.get(key).is_some())) {
            return false;
        }
        let b = bulk.entry(owner).or_insert_with(|| BulkLocks { proc, segs: Vec::new() });
        let done = b.seg(holder, xid).ukeys.insert_with(key, 0, compact_row_payload(key, row_key));
        self.bulk_owners.store(bulk.len(), Ordering::SeqCst);
        if done {
            COMPACT_HOLDS.fetch_add(1, Ordering::Relaxed);
        }
        drop(g);
        done
    }

    /// Move all of `owner`'s compact locks into the main tables (PREPARE TRANSACTION: they
    /// are recorded, moved to its dummy backend and restored as any): the unique keys it
    /// holds there now, and the rows.
    pub fn expand_compact(&self, owner: u64) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        // (Every partition, so nobody finds a row or key free between the set and the table.)
        let mut all = self.lock_all();
        let removed = {
            let mut bulk = self.bulk();
            let b = bulk.remove(&owner);
            self.bulk_owners.store(bulk.len(), Ordering::SeqCst);
            b
        };
        let Some(b) = removed else {
            return (Vec::new(), Vec::new());
        };
        let (mut keys, mut rows) = (Vec::new(), Vec::new());
        for g in b.segs {
            for (row, t) in g.rows.iter() {
                let (mode, modifies) = untag(t);
                let holds = all.part(row).rows.entry(row.to_vec()).or_default();
                match holds.iter_mut().find(|h| h.holder == g.holder) {
                    Some(h) => {
                        h.mode = h.mode.max(mode);
                        h.modifies |= modifies;
                    }
                    None => holds.push(RowHold { holder: g.holder, owner, proc: b.proc, mode, modifies }),
                }
                rows.push(row.to_vec());
            }
            for (k, _, row_key) in g.ukeys.entries() {
                let e = all.part(k).locks.entry(k.to_vec()).or_default();
                e.holds.push(Hold::new(owner, b.proc, LockMode::Exclusive, false));
                let row_key = compact_row_key(k, row_key);
                e.key_holder = Some((owner, KeyHolder { holder: g.holder, xid: g.xid, token: None, row: None, row_key }));
                keys.push(k.to_vec());
            }
            keys.extend(g.moved);
        }
        (keys, rows)
    }

    /// The DETAIL of a deadlock error for this cycle (DeadLockReport); `xid_of` gives a
    /// transaction lock's transaction id.
    fn report(&self, xid_of: impl Fn(&[u8]) -> u64, cycle: &[(u64, Vec<u8>, LockMode)]) -> Error {
        let pid = |p: u64| pid_of(p).unwrap_or(0);
        let mut detail = String::new();
        for (i, (p, key, mode)) in cycle.iter().enumerate() {
            let next = cycle.get(i + 1).unwrap_or(&cycle[0]).0;
            let xid = xid_of(key);
            let db = match tag_of(key, xid) {
                Tag::Relation { db, .. } | Tag::Tuple { db, .. } | Tag::Object { db, .. } => db,
                _ => 0,
            };
            if i > 0 {
                detail.push('\n');
            }
            detail.push_str(&format!(
                "Process {} waits for {} on {}; blocked by process {}.",
                pid(*p),
                mode.name(),
                describe(&tag_of(key, xid), db),
                pid(next)
            ));
        }
        Error::new(code::DEADLOCK_DETECTED, "deadlock detected")
            .detail(detail)
            .hint("See server log for query details.")
    }

    /// DeadLockCheck for waiting backend `proc`: a hard deadlock is reported (and `proc`
    /// left for the caller to remove from its queue); a soft one is resolved by reordering
    /// wait queues, granting what that allows.
    fn deadlock_check(&self, all: &mut All<'_>, proc: u64) -> Deadlock {
        // (Granted while every partition was being taken: nothing to check.)
        let waiting = all.0.iter().any(|p| p.waiters.get(&proc).is_some_and(|w| w.status == Status::Waiting));
        if !waiting {
            return Deadlock::None;
        }
        let mut at = HashMap::new();
        for (i, part) in all.0.iter().enumerate() {
            at.extend(part.waiters.keys().map(|p| (*p, i)));
        }
        let mut d = Dl {
            all,
            at,
            visited: Vec::new(),
            details: Vec::new(),
            wait_orders: Vec::new(),
            cur: Vec::new(),
        };
        if d.check_recurse(proc) {
            d.wait_orders.clear();
            let mut soft = Vec::new();
            d.find_cycle(proc, &mut soft);
            let cycle: Vec<(u64, Vec<u8>, LockMode)> = d.details.iter().flatten().cloned().collect();
            return Deadlock::Hard(cycle);
        }
        let orders = std::mem::take(&mut d.wait_orders);
        if orders.is_empty() {
            return Deadlock::None;
        }
        for (key, procs) in orders {
            let part = all.part(&key);
            if let Some(e) = part.locks.get_mut(&key) {
                e.queue = procs;
            }
            part.wakeup(&key);
        }
        Deadlock::Soft
    }
}

/// A waits-for edge of a soft cycle: `waiter` is queued behind `blocker` on `lock`.
#[derive(Clone, Debug)]
struct Edge {
    waiter: u64,
    blocker: u64,
    lock: Vec<u8>,
}

/// deadlock.c's search state, over every partition.
struct Dl<'s, 'a> {
    all: &'s mut All<'a>,
    /// The partition each waiting backend waits in.
    at: HashMap<u64, usize>,
    visited: Vec<u64>,
    details: Vec<Option<(u64, Vec<u8>, LockMode)>>,
    /// Wait queues reordered (by ExpandConstraints): (lock, its queue).
    wait_orders: Vec<(Vec<u8>, Vec<u64>)>,
    cur: Vec<Edge>,
}

enum Config {
    Hard,
    Edges(Vec<Edge>),
}

impl Dl<'_, '_> {
    fn waiter(&self, proc: u64) -> Option<&Waiter> {
        self.all.0[*self.at.get(&proc)?].waiters.get(&proc)
    }

    fn entry(&self, key: &[u8]) -> Option<&Entry> {
        self.all.get(key).locks.get(key)
    }

    /// DeadLockCheckRecurse: true if no deadlock-free ordering exists.
    fn check_recurse(&mut self, proc: u64) -> bool {
        let edges = match self.test_configuration(proc) {
            Config::Hard => return true,
            Config::Edges(e) if e.is_empty() => return false,
            Config::Edges(e) => e,
        };
        if self.cur.len() >= 64 {
            return true;
        }
        for e in edges {
            self.cur.push(e);
            if !self.check_recurse(proc) {
                return false;
            }
            self.cur.pop();
        }
        true
    }

    fn test_configuration(&mut self, start: u64) -> Config {
        let cur = self.cur.clone();
        if !self.expand_constraints(&cur) {
            return Config::Hard;
        }
        let mut found = Vec::new();
        let mut soft = Vec::new();
        for c in &cur {
            for p in [c.waiter, c.blocker] {
                if self.find_cycle(p, &mut soft) {
                    if soft.is_empty() {
                        return Config::Hard;
                    }
                    found = soft.clone();
                }
            }
        }
        if self.find_cycle(start, &mut soft) {
            if soft.is_empty() {
                return Config::Hard;
            }
            found = soft.clone();
        }
        Config::Edges(found)
    }

    fn find_cycle(&mut self, proc: u64, soft: &mut Vec<Edge>) -> bool {
        self.visited.clear();
        self.details.clear();
        soft.clear();
        self.find_cycle_recurse(proc, 0, soft)
    }

    fn find_cycle_recurse(&mut self, proc: u64, depth: usize, soft: &mut Vec<Edge>) -> bool {
        if let Some(i) = self.visited.iter().position(|p| *p == proc) {
            if i == 0 {
                self.details.clear();
                self.details.resize(depth, None);
                return true;
            }
            return false;
        }
        self.visited.push(proc);
        if self.waiter(proc).is_some_and(|w| w.status == Status::Waiting) {
            return self.find_cycle_member(proc, depth, soft);
        }
        false
    }

    fn find_cycle_member(&mut self, proc: u64, depth: usize, soft: &mut Vec<Edge>) -> bool {
        let (key, mode) = {
            let w = self.waiter(proc).unwrap();
            (w.key.clone(), w.mode)
        };
        let Some(e) = self.entry(&key) else { return false };
        let conflict = mode.conflicts();
        // Hard edges: holders of a conflicting mode.
        let holders: Vec<u64> = e
            .holder_procs()
            .into_iter()
            .filter(|p| *p != proc && e.held_mask(*p) & conflict != 0)
            .collect();
        let real_queue = e.queue.clone();
        for p in holders {
            if self.find_cycle_recurse(p, depth + 1, soft) {
                self.details[depth] = Some((proc, key.clone(), mode));
                return true;
            }
        }
        // Soft edges: waiters ahead of us wanting a conflicting mode.
        let queue = match self.wait_orders.iter().find(|(k, _)| *k == key) {
            Some((_, q)) => q.clone(),
            None => real_queue,
        };
        for p in queue {
            if p == proc {
                break;
            }
            // (A waiter granted but not yet awake waits for nothing.)
            let Some(m) = self.waiter(p).filter(|w| w.status == Status::Waiting).map(|w| w.mode) else { continue };
            if m.bit() & conflict != 0 && self.find_cycle_recurse(p, depth + 1, soft) {
                self.details[depth] = Some((proc, key.clone(), mode));
                soft.push(Edge { waiter: proc, blocker: p, lock: key.clone() });
                return true;
            }
        }
        false
    }

    /// ExpandConstraints: the wait queue orders the constraints call for.
    fn expand_constraints(&mut self, constraints: &[Edge]) -> bool {
        self.wait_orders.clear();
        for i in (0..constraints.len()).rev() {
            let lock = &constraints[i].lock;
            if self.wait_orders.iter().any(|(k, _)| k == lock) {
                continue;
            }
            match self.topo_sort(lock, &constraints[..=i]) {
                Some(order) => self.wait_orders.push((lock.clone(), order)),
                None => return false,
            }
        }
        true
    }

    /// TopoSort: an order of `lock`'s queue putting each constraint's waiter before its
    /// blocker, otherwise keeping the queue's order as far as possible.
    fn topo_sort(&self, lock: &[u8], constraints: &[Edge]) -> Option<Vec<u64>> {
        let queue = self.entry(lock).map(|e| e.queue.clone()).unwrap_or_default();
        let n = queue.len();
        let mut topo: Vec<Option<u64>> = queue.iter().copied().map(Some).collect();
        let mut before = vec![0i32; n];
        let mut after = vec![0usize; n];
        let mut pred = vec![0usize; constraints.len()];
        let mut link = vec![0usize; constraints.len()];
        for (i, c) in constraints.iter().enumerate() {
            if c.lock != lock {
                continue;
            }
            let Some(jj) = queue.iter().rposition(|p| *p == c.waiter) else { continue };
            let Some(kk) = queue.iter().rposition(|p| *p == c.blocker) else { continue };
            before[jj] += 1;
            pred[i] = jj;
            link[i] = after[kk];
            after[kk] = i + 1;
        }
        let mut ordering = vec![0u64; n];
        let mut last = n as isize - 1;
        let mut i = n as isize - 1;
        while i >= 0 {
            while topo[last as usize].is_none() {
                last -= 1;
            }
            let mut j = last;
            while j >= 0 && !(topo[j as usize].is_some() && before[j as usize] == 0) {
                j -= 1;
            }
            if j < 0 {
                return None;
            }
            let j = j as usize;
            ordering[i as usize] = topo[j].take().unwrap();
            i -= 1;
            let mut k = after[j];
            while k > 0 {
                before[pred[k - 1]] -= 1;
                k = link[k - 1];
            }
        }
        Some(ordering)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// Backend `n`'s proc (a session's, carrying pid `n`).
    fn proc(n: u32) -> u64 {
        1 << 63 | n as u64
    }

    /// An object lock key of partition `part`.
    fn key_in(part: usize) -> Vec<u8> {
        (0..).map(|i| object_key(1, 1259, i, 0)).find(|k| part_of(k) == part).unwrap()
    }

    fn waits(cancel: &AtomicBool, deadlock_ms: u64) -> Waits<'_> {
        Waits { deadlock_timeout: Duration::from_millis(deadlock_ms), ..Waits::new(WaitPolicy::Block, cancel) }
    }

    /// Owner `owner` of backend `n` asks for `key` in another thread, waiting up to a minute
    /// before its deadlock check.
    fn ask(m: &Arc<LockManager>, key: &[u8], owner: u64, n: u32, mode: LockMode) -> thread::JoinHandle<Result<bool>> {
        let (m2, key) = (m.clone(), key.to_vec());
        let h = thread::spawn(move || m2.acquire(&key, owner, proc(n), mode, &waits(&AtomicBool::new(false), 60_000), &String::new));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !m_waits(m, n) {
            assert!(Instant::now() < deadline, "backend {n} never waited");
            thread::sleep(Duration::from_millis(2));
        }
        h
    }

    fn m_waits(m: &LockManager, n: u32) -> bool {
        m.snapshot().iter().any(|l| !l.granted && l.pid == Some(n as i32))
    }

    fn take(m: &LockManager, key: &[u8], owner: u64, n: u32, mode: LockMode) {
        assert!(m.acquire(key, owner, proc(n), mode, &waits(&AtomicBool::new(false), 60_000), &String::new).unwrap());
    }

    /// Hard deadlocks whose locks are in two and in three partitions: the backend that checks
    /// fails with the cycle in its DETAIL, and the others get their locks once it lets go.
    #[test]
    fn deadlocks_across_partitions() {
        let x = LockMode::AccessExclusive;
        for n in [2u32, 3] {
            let m = Arc::new(LockManager::default());
            let keys: Vec<Vec<u8>> = (1..=n as usize).map(key_in).collect();
            for (i, k) in keys.iter().enumerate() {
                take(&m, k, i as u64 + 1, i as u32 + 1, x);
            }
            // Backend i waits for backend i + 1's key; the last one closes the cycle.
            let waiting: Vec<_> = (0..n as usize - 1).map(|i| ask(&m, &keys[i + 1], i as u64 + 1, i as u32 + 1, x)).collect();
            let last = n as u64;
            let e = m.acquire(&keys[0], last, proc(n), x, &waits(&AtomicBool::new(false), 20), &String::new).unwrap_err();
            assert_eq!(e.code, code::DEADLOCK_DETECTED);
            assert_eq!(e.detail.as_deref().unwrap_or_default().lines().count(), n as usize, "{e:?}");
            // Each lets go in turn, from the last.
            m.release_all(last, [keys[n as usize - 1].clone()]);
            for (i, h) in waiting.into_iter().enumerate().rev() {
                assert!(h.join().unwrap().unwrap());
                m.release_all(i as u64 + 1, [keys[i].clone(), keys[i + 1].clone()]);
            }
            assert_eq!(m.held_count(), 0);
        }
    }

    /// A soft deadlock (a waiter queued behind one it blocks) found by a backend waiting in
    /// another partition: reordering the queue grants the waiter there, and every lock is
    /// granted in the end.
    #[test]
    fn soft_deadlock_granted_in_another_partition() {
        let m = Arc::new(LockManager::default());
        let (lx, ly) = (key_in(1), key_in(2));
        take(&m, &lx, 1, 1, LockMode::Share);
        take(&m, &ly, 3, 3, LockMode::AccessExclusive);
        let t2 = ask(&m, &lx, 2, 2, LockMode::Exclusive);
        // (Behind 2's request, which conflicts with its own.)
        let t3 = ask(&m, &lx, 3, 3, LockMode::Share);
        // 1 waits for 3, which waits (softly) for 2, which waits for 1: 1's check moves 3 ahead.
        let t1 = {
            let (m, ly) = (m.clone(), ly.clone());
            thread::spawn(move || m.acquire(&ly, 1, proc(1), LockMode::AccessExclusive, &waits(&AtomicBool::new(false), 20), &String::new))
        };
        assert!(t3.join().unwrap().unwrap());
        assert!(m_waits(&m, 1) && m_waits(&m, 2));
        m.release_all(3, [lx.clone(), ly.clone()]);
        assert!(t1.join().unwrap().unwrap());
        m.release_all(1, [lx.clone(), ly]);
        assert!(t2.join().unwrap().unwrap());
        m.release_all(2, [lx]);
        assert_eq!(m.held_count(), 0);
    }

    /// Seeded stress: threads take and release object locks, relation locks (weak ones on the
    /// fast path, strong ones moving its holds) and row locks (some into compact sets) over
    /// every partition with random wait policies, deadlocking now and then; the first run also
    /// with lock timeouts and cancels. Conflicting holders never overlap, pg_locks never shows a
    /// conflicting granted pair nor a backend granted and waiting in one mode, and no grant's
    /// wakeup is lost: without interruptions, no waiter finds its grant only when its sleep
    /// times out, 5 ms or more after it.
    #[test]
    fn partitions_under_load() {
        let seed: u64 = std::env::var("LOCKS_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x5eed);
        let late = LATE_GRANTS.load(Ordering::Relaxed);
        stress(seed, true);
        assert_eq!(LATE_GRANTS.load(Ordering::Relaxed), late, "a grant's wakeup was lost");
        MAX_TIMED_OUT_GRANT_US.store(0, Ordering::Relaxed);
        stress(seed ^ 0x9e37_79b9, false);
        let us = MAX_TIMED_OUT_GRANT_US.load(Ordering::Relaxed);
        assert!(us < 5_000, "a waiter found its grant {us} us old when its sleep timed out");
    }

    fn stress(seed: u64, interrupts: bool) {
        const THREADS: usize = 8;
        const N: usize = 12;
        let m = Arc::new(LockManager::default());
        // Even: object locks; odd: relations of database 1 (the fast path's).
        let keys: Arc<Vec<Vec<u8>>> = Arc::new(
            (0..N as u32).map(|i| if i % 2 == 0 { object_key(1, 1259, i, 0) } else { relation_key(1, 16384 + i) }).collect(),
        );
        let rows: Arc<Vec<Vec<u8>>> = Arc::new((0..N as u32).map(|i| [&[0, 0, 64, 0, 0, 0, 0, 0][..], &i.to_be_bytes()].concat()).collect());
        // What has been granted and not yet released, as the threads saw it: (owner, mode).
        let held: Arc<Vec<Mutex<Vec<(u64, LockMode)>>>> = Arc::new((0..N).map(|_| Mutex::default()).collect());
        let held_rows: Arc<Vec<Mutex<Vec<(u64, RowLockMode)>>>> = Arc::new((0..N).map(|_| Mutex::default()).collect());
        let cancels: Arc<Vec<AtomicBool>> = Arc::new((0..THREADS).map(|_| AtomicBool::new(false)).collect());
        let stop = Arc::new(AtomicBool::new(false));
        // What ended requests other than a grant: deadlocks, lock timeouts, cancels, skips.
        let ends: Arc<[AtomicU64; 4]> = Arc::new(Default::default());
        let rng = |s: &mut u64, n: usize| {
            *s ^= *s << 13;
            *s ^= *s >> 7;
            *s ^= *s << 17;
            (*s % n as u64) as usize
        };
        let watcher = {
            let (m, stop) = (m.clone(), stop.clone());
            thread::spawn(move || {
                let mut s = seed ^ 0xabc;
                while !stop.load(Ordering::Relaxed) {
                    let snap = m.snapshot();
                    // (A backend waits for one lock at a time.)
                    let mut waiting: Vec<Option<i32>> = snap.iter().filter(|l| !l.granted).map(|l| l.pid).collect();
                    let n = waiting.len();
                    waiting.sort();
                    waiting.dedup();
                    assert_eq!(waiting.len(), n, "pg_locks: a backend waiting twice: {snap:?}");
                    for (i, a) in snap.iter().enumerate() {
                        for b in snap[i + 1..].iter().filter(|b| b.tag == a.tag) {
                            if a.granted && b.granted && a.pid != b.pid {
                                assert!(!a.mode.conflicts_with(b.mode), "pg_locks: {a:?} and {b:?}");
                            }
                            // (Granted one mode and waiting for another is an upgrade.)
                            if a.granted != b.granted && a.pid == b.pid {
                                assert!(a.mode != b.mode, "pg_locks: granted and waiting: {a:?} and {b:?}");
                            }
                        }
                    }
                    m.blocking_pids(1 + rng(&mut s, THREADS) as i32);
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };
        let canceller = interrupts.then(|| {
            let (cancels, stop) = (cancels.clone(), stop.clone());
            thread::spawn(move || {
                let mut s = seed ^ 0xdef;
                while !stop.load(Ordering::Relaxed) {
                    let c = &cancels[rng(&mut s, THREADS)];
                    c.store(true, Ordering::Relaxed);
                    interrupt(c);
                    thread::sleep(Duration::from_millis(3));
                }
            })
        });
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                let (m, keys, rows, held, held_rows, cancels, ends) =
                    (m.clone(), keys.clone(), rows.clone(), held.clone(), held_rows.clone(), cancels.clone(), ends.clone());
                let end = move |r: &Result<bool>| {
                    let i = match r {
                        Err(e) if e.code == code::DEADLOCK_DETECTED => 0,
                        Err(e) if e.code == code::LOCK_NOT_AVAILABLE => 1,
                        Err(_) => 2,
                        Ok(_) => 3,
                    };
                    ends[i].fetch_add(1, Ordering::Relaxed);
                };
                thread::spawn(move || {
                    let mut s = seed.wrapping_mul(t as u64 + 1) | 1;
                    let p = proc(t as u32 + 1);
                    let slot = m.fast_slot(p).unwrap();
                    for i in 0..400u64 {
                        let owner = (t as u64) << 32 | i;
                        cancels[t].store(false, Ordering::Relaxed);
                        m.hold_xact_lock(owner, owner, p, owner);
                        let compact = rng(&mut s, 4) == 0;
                        let (mut my_keys, mut my_rows) = (vec![xact_key(owner)], Vec::new());
                        for _ in 0..1 + rng(&mut s, 5) {
                            let policy = match rng(&mut s, 10) {
                                0 => WaitPolicy::NoWait,
                                1 => WaitPolicy::SkipLocked,
                                _ => WaitPolicy::Block,
                            };
                            let lock_timeout = (interrupts && rng(&mut s, 4) == 0).then_some(Duration::from_millis(20));
                            let w = Waits { deadlock_timeout: Duration::from_millis(10), lock_timeout, ..Waits::new(policy, &cancels[t]) };
                            if rng(&mut s, 2) == 0 {
                                let (k, mode) = (rng(&mut s, keys.len()), LockMode::ALL[rng(&mut s, 8)]);
                                let fast = rng(&mut s, 2) == 0 && m.acquire_fast(&slot, &keys[k], owner, mode);
                                let got = if fast { Ok(true) } else { m.acquire(&keys[k], owner, p, mode, &w, &String::new) };
                                if !matches!(got, Ok(true)) {
                                    end(&got);
                                }
                                match got {
                                    Ok(true) => {
                                        let mut h = held[k].lock();
                                        for (o, md) in h.iter() {
                                            assert!(*o == owner || !md.conflicts_with(mode), "{o} holds {md:?} while {owner} got {mode:?}");
                                        }
                                        h.push((owner, mode));
                                        my_keys.push(keys[k].clone());
                                    }
                                    Ok(false) => {}
                                    Err(_) => break,
                                }
                            } else {
                                let r = rng(&mut s, rows.len());
                                let mode = [RowLockMode::KeyShare, RowLockMode::Share, RowLockMode::NoKeyUpdate, RowLockMode::Update][rng(&mut s, 4)];
                                let tuple = || tuple_key(1, 1, &rows[r]);
                                let mut compacted = false;
                                let into = compact.then_some(&mut compacted);
                                let got = m.lock_row(&rows[r], &tuple, owner, owner, p, owner, mode, &w, &|| "t".into(), into, &|| None);
                                if !matches!(got, Ok(true)) {
                                    end(&got);
                                }
                                match got {
                                    Ok(true) => {
                                        let mut h = held_rows[r].lock();
                                        for (o, md) in h.iter() {
                                            assert!(*o == owner || !md.conflicts_with(mode), "{o} holds row {md:?} while {owner} got {mode:?}");
                                        }
                                        h.push((owner, mode));
                                        my_rows.push(rows[r].clone());
                                    }
                                    Ok(false) => {}
                                    Err(_) => break,
                                }
                            }
                        }
                        if rng(&mut s, 2) == 0 {
                            thread::sleep(Duration::from_millis(1));
                        }
                        // (Forgotten here before they are released: what is listed is held.)
                        for h in held.iter() {
                            h.lock().retain(|(o, _)| *o != owner);
                        }
                        for h in held_rows.iter() {
                            h.lock().retain(|(o, _)| *o != owner);
                        }
                        // As a transaction ends: its fast-path holds, then the main table's.
                        m.release_fast_owner(&slot, owner);
                        m.release_with_rows(owner, my_keys, my_rows);
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        watcher.join().unwrap();
        if let Some(c) = canceller {
            c.join().unwrap();
        }
        let n: Vec<u64> = ends.iter().map(|e| e.load(Ordering::Relaxed)).collect();
        eprintln!("stress (interrupts {interrupts}): deadlocks {}, lock timeouts / NOWAIT {}, cancels {}, skipped {}", n[0], n[1], n[2], n[3]);
        assert_eq!(m.held_count(), 0);
    }
}
