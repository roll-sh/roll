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

use super::bulklocks::{BulkLocks, row_tag, untag};
use crate::error::{Error, Result, code};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// A system catalog row's key for row locks (`catalog_row_key`).
pub const CATALOG_ROW: u8 = 11;
/// A unique index key's lock (the engine's in-progress index entry, `unique_key_lock`).
pub const UNIQUE_KEY: u8 = 4;
/// An exclusion constraint's entry in progress (`excl_entry_key`): what a dirty snapshot
/// sees of another writer's index entry, held by the writer until its (sub)transaction ends.
pub const EXCL_ENTRY: u8 = 12;

/// The lock standing for row `row`'s entry in exclusion constraint `con` while its writer
/// runs (`Txn::hold_exclusion_entry`).
pub fn excl_entry_key(con: u32, row: &[u8]) -> Vec<u8> {
    let mut k = prefixed(EXCL_ENTRY);
    k.extend_from_slice(&con.to_be_bytes());
    k.extend_from_slice(row);
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
    let mut k = prefixed(RELATION);
    k.extend_from_slice(&db.to_be_bytes());
    k.extend_from_slice(&rel.to_be_bytes());
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
        Tag::Transaction { xid, .. } => format!("transaction {xid}"),
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
        Tag::Other(_) => "a row".to_string(),
    }
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
                self.holds.push(Hold { owner, proc, mode, count: 1, fastpath })
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

#[derive(Default)]
struct State {
    /// (Keyed with the fast hasher: a row lock is taken for every row a statement writes.)
    locks: crate::exec::fasthash::HashMap<Vec<u8>, Entry>,
    /// Waiting backends' requests (a backend waits for one lock at a time).
    waiters: HashMap<u64, Waiter>,
    /// The process id of each backend.
    pids: HashMap<u64, i32>,
    rows: crate::exec::fasthash::HashMap<Vec<u8>, Vec<RowHold>>,
    /// Prepared transactions' dummy backends (`transfer`).
    prepared: std::collections::HashSet<u64>,
    /// Transactions' compact row and unique-key locks (`bulklocks`), by transaction: what
    /// `rows` and the unique keys of `locks` would otherwise hold for them.
    bulk: crate::exec::fasthash::HashMap<u64, BulkLocks>,
    /// Exclusion constraints' entries in progress, by constraint: their lock (`EXCL_ENTRY`),
    /// row key and writer. An entry whose lock is no longer held is gone (dropped lazily).
    excl: HashMap<u32, Vec<(Vec<u8>, Vec<u8>, KeyHolder)>>,
}

/// Whether `key` is a unique key's lock (`UNIQUE_KEY`).
fn is_unique_key(key: &[u8]) -> bool {
    key.len() >= 8 && key[..7] == [0; 7] && key[7] == UNIQUE_KEY
}

/// Locks taken into compact sets so far (row locks and unique keys; tests read it).
pub static COMPACT_HOLDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl State {
    /// The compact row locks on `row`, as holds (none when no transaction has any).
    fn bulk_row_holds(&self, row: &[u8]) -> Vec<RowHold> {
        let mut out = Vec::new();
        for (&owner, b) in &self.bulk {
            for g in &b.segs {
                if let Some(t) = g.rows.get(row) {
                    let (mode, modifies) = untag(t);
                    out.push(RowHold { holder: g.holder, owner, proc: b.proc, mode, modifies });
                }
            }
        }
        out
    }

    /// Move a compact unique-key lock on `key` into the main table (someone else asks for
    /// it): from there it is queued for, released and reported as any lock.
    fn materialize(&mut self, key: &[u8]) {
        if self.bulk.is_empty() || !is_unique_key(key) {
            return;
        }
        let mut found = None;
        'all: for (&owner, b) in self.bulk.iter_mut() {
            for g in &mut b.segs {
                let row_key = g.ukeys.payload(key).map(<[u8]>::to_vec);
                if g.ukeys.remove(key) {
                    g.moved.push(key.to_vec());
                    found = Some((owner, b.proc, g.holder, g.xid, row_key));
                    break 'all;
                }
            }
        }
        if let Some((owner, proc, holder, xid, row_key)) = found {
            let e = self.locks.entry(key.to_vec()).or_default();
            e.holds.push(Hold { owner, proc, mode: LockMode::Exclusive, count: 1, fastpath: false });
            let row_key = row_key.filter(|k| !k.is_empty());
            e.key_holder = Some((owner, KeyHolder { holder, xid, token: None, row: None, row_key }));
        }
    }

    /// Release `owner`'s holds of the unique keys a compact segment moved to the main table
    /// (one count each: a later lock of the key by the transaction is its own).
    fn release_moved(&mut self, owner: u64, moved: Vec<Vec<u8>>) {
        for k in moved {
            if let Some(e) = self.locks.get_mut(&k)
                && let Some(i) = e.holds.iter().position(|h| h.owner == owner && h.mode == LockMode::Exclusive)
            {
                e.holds[i].count -= 1;
                if e.holds[i].count == 0 {
                    e.holds.remove(i);
                }
                LockManager::wakeup(self, &k);
                LockManager::drop_empty(self, &k);
            }
        }
    }
}

#[derive(Default)]
pub struct LockManager {
    state: Mutex<State>,
    cv: Condvar,
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

enum Deadlock {
    None,
    Soft,
    /// The cycle: (backend, lock key, mode it waits for).
    Hard(Vec<(u64, Vec<u8>, LockMode)>),
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
        let mut s = self.state.lock();
        // (A compact unique-key lock someone else asks for goes to the main table first.)
        s.materialize(key);
        if let Some(pid) = pid_of(proc) {
            s.pids.insert(proc, pid);
        }
        let wait_mask = {
            let e = s.locks.get(key);
            e.map_or(0, |e| {
                e.queue
                    .iter()
                    .filter_map(|p| s.waiters.get(p))
                    .fold(0, |m, w| m | w.mode.bit())
            })
        };
        let e = s.locks.entry(key.to_vec()).or_default();
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
                let Some(other) = s.waiters.get(wp) else { continue };
                if other.mode.conflicts() & my_held != 0 {
                    if mode.conflicts() & other.held != 0 {
                        // RememberSimpleDeadLock: each waits for the other.
                        let cycle = vec![
                            (proc, key.to_vec(), mode),
                            (*wp, other.key.clone(), other.mode),
                        ];
                        let msg = self.report(&s, &cycle);
                        Self::drop_empty(&mut s, key);
                        return Err(msg);
                    }
                    let e = s.locks.get_mut(key).unwrap();
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
                Self::drop_empty(&mut s, key);
                return Err(Error::new(
                    code::LOCK_NOT_AVAILABLE,
                    format!("could not obtain lock on {}", what()),
                ));
            }
            WaitPolicy::SkipLocked => {
                Self::drop_empty(&mut s, key);
                return Ok(false);
            }
            WaitPolicy::Block => {}
        }
        let e = s.locks.get_mut(key).unwrap();
        match insert_at {
            Some(i) => e.queue.insert(i, proc),
            None => e.queue.push(proc),
        }
        let start = Instant::now();
        s.waiters.insert(
            proc,
            Waiter {
                key: key.to_vec(),
                owner,
                mode,
                held: my_held,
                status: Status::Waiting,
                since: start,
            },
        );
        // What the statement said so far reaches the client before it waits.
        drop(s);
        crate::exec::flush_live_notices();
        let mut s = self.state.lock();
        let mut checked = false;
        loop {
            if s.waiters.get(&proc).is_some_and(|w| w.status == Status::Granted) {
                s.waiters.remove(&proc);
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
                match self.deadlock_check(&mut s, proc) {
                    Deadlock::Hard(cycle) => Some(self.report(&s, &cycle)),
                    Deadlock::Soft => {
                        self.cv.notify_all();
                        continue;
                    }
                    Deadlock::None => None,
                }
            } else {
                None
            };
            if let Some(e) = fail {
                self.remove_waiter(&mut s, proc);
                drop(s);
                self.cv.notify_all();
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
            self.cv.wait_until(&mut s, wake.max(now + Duration::from_millis(1)));
        }
    }

    fn drop_empty(s: &mut State, key: &[u8]) {
        if s.locks.get(key).is_some_and(|e| e.holds.is_empty() && e.queue.is_empty()) {
            s.locks.remove(key);
        }
    }

    /// RemoveFromWaitQueue, and the wakeups it may allow.
    fn remove_waiter(&self, s: &mut State, proc: u64) {
        let Some(w) = s.waiters.remove(&proc) else { return };
        if let Some(e) = s.locks.get_mut(&w.key) {
            e.queue.retain(|p| *p != proc);
        }
        Self::wakeup(s, &w.key);
        Self::drop_empty(s, &w.key);
    }

    /// ProcLockWakeup: grant waiters in queue order while they conflict neither with what
    /// is held nor with the waiters still ahead of them.
    fn wakeup(s: &mut State, key: &[u8]) {
        let Some(e) = s.locks.get_mut(key) else { return };
        let mut ahead = 0u16;
        let mut i = 0;
        while i < e.queue.len() {
            let p = e.queue[i];
            let Some(w) = s.waiters.get_mut(&p) else {
                e.queue.remove(i);
                continue;
            };
            if w.mode.conflicts() & ahead == 0 && !e.conflicts(p, w.mode) {
                e.grant(w.owner, p, w.mode, key);
                w.status = Status::Granted;
                e.queue.remove(i);
            } else {
                ahead |= w.mode.bit();
                i += 1;
            }
        }
    }

    /// Release one count of `owner`'s hold of `mode`.
    pub fn release(&self, key: &[u8], owner: u64, mode: LockMode) {
        let mut s = self.state.lock();
        if let Some(e) = s.locks.get_mut(key)
            && let Some(i) = e.holds.iter().position(|h| h.owner == owner && h.mode == mode)
        {
            e.holds[i].count -= 1;
            if e.holds[i].count == 0 {
                e.holds.remove(i);
            }
            Self::wakeup(&mut s, key);
            Self::drop_empty(&mut s, key);
        }
        drop(s);
        self.cv.notify_all();
    }

    /// Release everything `owner` holds of these heavyweight locks.
    pub fn release_all(&self, owner: u64, keys: impl IntoIterator<Item = Vec<u8>>) {
        self.release_with_rows(owner, keys, std::iter::empty());
    }

    /// A transaction ending: its row locks on `rows`, then its heavyweight locks (its
    /// transaction locks among them, so waiters see the rows free when they wake).
    pub fn release_with_rows(
        &self,
        owner: u64,
        keys: impl IntoIterator<Item = Vec<u8>>,
        rows: impl IntoIterator<Item = Vec<u8>>,
    ) {
        let mut s = self.state.lock();
        if let Some(b) = s.bulk.remove(&owner) {
            for g in b.segs {
                s.release_moved(owner, g.moved);
            }
        }
        for r in rows {
            if let Some(holds) = s.rows.get_mut(&r) {
                holds.retain(|h| h.owner != owner);
                if holds.is_empty() {
                    s.rows.remove(&r);
                }
            }
        }
        for k in keys {
            if let Some(e) = s.locks.get_mut(&k) {
                e.holds.retain(|h| h.owner != owner);
                Self::wakeup(&mut s, &k);
                Self::drop_empty(&mut s, &k);
            }
        }
        drop(s);
        self.cv.notify_all();
    }

    /// Subtransactions aborting: their row locks on `rows` go, then their transaction locks.
    pub fn release_subxact(&self, owner: u64, holders: &[u64], rows: &[Vec<u8>]) {
        let mut s = self.state.lock();
        if let Some(b) = s.bulk.get_mut(&owner) {
            let (gone, kept): (Vec<_>, Vec<_>) =
                std::mem::take(&mut b.segs).into_iter().partition(|g| holders.contains(&g.holder));
            b.segs = kept;
            for g in gone {
                s.release_moved(owner, g.moved);
            }
        }
        for r in rows {
            if let Some(holds) = s.rows.get_mut(r) {
                holds.retain(|h| !(h.owner == owner && holders.contains(&h.holder)));
                if holds.is_empty() {
                    s.rows.remove(r);
                }
            }
        }
        for id in holders {
            let k = xact_key(*id);
            if let Some(e) = s.locks.get_mut(&k) {
                e.holds.retain(|h| h.owner != owner);
                Self::wakeup(&mut s, &k);
                Self::drop_empty(&mut s, &k);
            }
        }
        drop(s);
        self.cv.notify_all();
    }

    /// Hold (sub)transaction `holder`'s own lock from when it gets a transaction id
    /// (`xid`), so others can wait for it to end.
    pub fn hold_xact_lock(&self, holder: u64, owner: u64, proc: u64, xid: u64) {
        let mut s = self.state.lock();
        if let Some(pid) = pid_of(proc) {
            s.pids.insert(proc, pid);
        }
        Self::hold_xact(&mut s, holder, owner, proc, xid);
    }

    /// Hold (sub)transaction `holder`'s own lock, so others can wait for it to end.
    fn hold_xact(s: &mut State, holder: u64, owner: u64, proc: u64, xid: u64) {
        let e = s.locks.entry(xact_key(holder)).or_default();
        if xid != 0 {
            e.xid = xid;
        }
        if !e.holds.iter().any(|h| h.owner == owner) {
            e.holds.push(Hold { owner, proc, mode: LockMode::Exclusive, count: 1, fastpath: false });
        }
    }

    /// Lock row `row` (its data key) in `mode` for (sub)transaction `holder` of top-level
    /// transaction `owner`, as heap_lock_tuple does: Ok(false) only for SKIP LOCKED. `tuple`
    /// is the row's heavyweight tuple lock key; `xid` the holder's transaction id. The
    /// holder's transaction lock (`xact_key(holder)`) is held from here on. With `compact`,
    /// a new hold may go into the transaction's compact set (`bulklocks`), which it is told:
    /// such a hold is released with the transaction (or its subtransaction), not by row.
    #[allow(clippy::too_many_arguments)]
    pub fn lock_row(
        &self,
        row: &[u8],
        tuple: &[u8],
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
        let mut have_tuple_lock = false;
        let mut first = true;
        let mut skip_tuple_lock = false;
        let release_tuple = |this: &Self, have: bool| {
            if have {
                this.release(tuple, owner, mode.heavy());
            }
        };
        loop {
            let mut s = self.state.lock();
            if let Some(pid) = pid_of(proc)
                && s.pids.get(&proc) != Some(&pid)
            {
                s.pids.insert(proc, pid);
            }
            let bulk = if s.bulk.is_empty() { Vec::new() } else { s.bulk_row_holds(row) };
            let holds = || s.rows.get(row).map_or(&[][..], |h| h.as_slice()).iter().chain(&bulk);
            if first {
                first = false;
                let mine: Vec<&RowHold> = holds().filter(|h| h.owner == owner).collect();
                if mine.iter().any(|h| h.mode >= mode) {
                    // Held at least this strongly by this transaction already.
                    return Ok(true);
                }
                // Promoting a weaker lock: no tuple lock, or we'd deadlock with a waiter
                // holding it and waiting for us.
                skip_tuple_lock = !mine.is_empty();
            }
            let conflicting: Vec<u64> = holds()
                .filter(|h| h.owner != owner && h.mode.conflicts_with(mode))
                .map(|h| h.holder)
                .collect();
            if conflicting.is_empty() {
                Self::hold_xact(&mut s, holder, owner, proc, xid);
                // (A holder's lock stays where it is; a new one goes compact when it may.)
                let in_main = s.rows.get(row).is_some_and(|h| h.iter().any(|h| h.holder == holder));
                if !in_main && let Some(flag) = compact.as_deref_mut() {
                    let b = s.bulk.entry(owner).or_insert_with(|| BulkLocks { proc, segs: Vec::new() });
                    let g = b.seg(holder, xid);
                    let held = |t: Option<u8>| match t.map(untag) {
                        Some((h, modifies)) => row_tag(h.max(mode), modifies),
                        None => row_tag(mode, false),
                    };
                    if g.rows.upsert(row, held) {
                        *flag = true;
                        COMPACT_HOLDS.fetch_add(1, Ordering::Relaxed);
                        drop(s);
                        release_tuple(self, have_tuple_lock);
                        return Ok(true);
                    }
                }
                let holds = s.rows.entry(row.to_vec()).or_default();
                match holds.iter_mut().find(|h| h.holder == holder) {
                    Some(h) => h.mode = h.mode.max(mode),
                    None => holds.push(RowHold { holder, owner, proc, mode, modifies: false }),
                }
                drop(s);
                release_tuple(self, have_tuple_lock);
                return Ok(true);
            }
            drop(s);
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
                let got = match self.acquire(tuple, owner, proc, mode.heavy(), w, &|| {
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
        let s = self.state.lock();
        let mut out = Vec::new();
        if let Some(e) = s.locks.get(key) {
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
        let mut s = self.state.lock();
        if let Some(e) = s.locks.get_mut(key)
            && e.holds.iter().any(|h| h.owner == owner)
        {
            e.key_holder = Some((owner, kh));
        }
    }

    /// Exclusion constraint `con`'s entry for row `row` is in progress: lock `key`, just
    /// taken by `owner`, stands for it until its writer `kh` ends.
    pub fn add_excl_entry(&self, con: u32, key: &[u8], row: &[u8], owner: u64, kh: KeyHolder) {
        let mut s = self.state.lock();
        if s.locks.get(key).is_some_and(|e| e.holds.iter().any(|h| h.owner == owner)) {
            s.excl.entry(con).or_default().push((key.to_vec(), row.to_vec(), kh));
        }
    }

    /// A speculative insertion's entry `key` of exclusion constraint `con` is complete: it
    /// stands for its (sub)transaction from now on.
    pub fn settle_excl_entry(&self, con: u32, key: &[u8]) {
        let mut s = self.state.lock();
        if let Some(list) = s.excl.get_mut(&con) {
            for (_, _, kh) in list.iter_mut().filter(|(k, _, _)| k == key) {
                kh.token = None;
            }
        }
    }

    /// The entries in progress of exclusion constraint `con` that backends other than `proc`
    /// are writing: (row key, writer). (Ended writers' entries go.)
    pub fn excl_entries(&self, con: u32, proc: u64) -> Vec<(Vec<u8>, KeyHolder)> {
        let mut s = self.state.lock();
        let State { locks, excl, .. } = &mut *s;
        let Some(list) = excl.get_mut(&con) else { return Vec::new() };
        list.retain(|(k, _, _)| locks.get(k).is_some_and(|e| !e.holds.is_empty()));
        let out = list
            .iter()
            .filter(|(k, _, _)| locks.get(k).is_some_and(|e| e.holds.iter().any(|h| h.proc != proc)))
            .map(|(_, row, kh)| (row.clone(), kh.clone()))
            .collect();
        if list.is_empty() {
            excl.remove(&con);
        }
        out
    }

    /// The in-progress entry another backend than `proc` holds unique key lock `key` for:
    /// what it recorded, or (when it recorded nothing) its top-level transaction.
    pub fn key_holder(&self, key: &[u8], proc: u64) -> Option<KeyHolder> {
        let s = self.state.lock();
        let Some(e) = s.locks.get(key) else {
            // A compact one: its (sub)transaction's in-progress entry.
            return s.bulk.values().filter(|b| b.proc != proc).find_map(|b| {
                b.segs.iter().find_map(|g| {
                    let row_key = g.ukeys.payload(key)?;
                    Some(KeyHolder {
                        holder: g.holder,
                        xid: g.xid,
                        token: None,
                        row: None,
                        row_key: (!row_key.is_empty()).then(|| row_key.to_vec()),
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
        let mut s = self.state.lock();
        let State { rows, bulk, .. } = &mut *s;
        let modified = |t: u8| row_tag(untag(t).0, true);
        let mut main = rows.get_mut(row);
        // The hold of `holder`'s: in the main table, else in its compact set.
        if let Some(h) = main
            .as_deref_mut()
            .and_then(|holds| holds.iter_mut().find(|h| h.owner == owner && h.holder == holder))
        {
            h.modifies = true;
            return;
        }
        let segs = match bulk.get_mut(&owner) {
            Some(b) => &mut b.segs[..],
            None => &mut [],
        };
        if let Some(g) = segs.iter_mut().find(|g| g.holder == holder)
            && g.rows.update(row, modified)
        {
            return;
        }
        // Else any of the transaction's.
        if let Some(h) = main.and_then(|holds| holds.iter_mut().find(|h| h.owner == owner)) {
            h.modifies = true;
            return;
        }
        for g in segs.iter_mut().filter(|g| g.holder != holder) {
            if g.rows.update(row, modified) {
                return;
            }
        }
    }

    /// Every hold of row `row`: the main table's, then the compact sets'.
    fn all_row_holds(s: &State, row: &[u8]) -> Vec<RowHold> {
        let mut out: Vec<RowHold> = s.rows.get(row).cloned().unwrap_or_default();
        if !s.bulk.is_empty() {
            out.extend(s.bulk_row_holds(row));
        }
        out
    }

    /// The (sub)transactions (lock ids) of other transactions than `owner` holding row
    /// `row` in a mode conflicting with `mode`.
    pub fn row_conflicts(&self, row: &[u8], owner: u64, mode: RowLockMode) -> Vec<u64> {
        let s = self.state.lock();
        Self::all_row_holds(&s, row)
            .iter()
            .filter(|h| h.owner != owner && h.mode.conflicts_with(mode))
            .map(|h| h.holder)
            .collect()
    }

    /// The (sub)transaction of another backend than `proc` updating or deleting row `row`
    /// (HeapTupleSatisfiesDirty's in-progress xmax), if any.
    pub fn row_modifier(&self, row: &[u8], proc: u64) -> Option<u64> {
        let s = self.state.lock();
        if s.bulk.is_empty() {
            return s.rows.get(row)?.iter().find(|h| h.modifies && h.proc != proc).map(|h| h.holder);
        }
        Self::all_row_holds(&s, row)
            .iter()
            .find(|h| h.modifies && h.proc != proc)
            .map(|h| h.holder)
    }

    /// The row locks on `row` other transactions than `owner` hold: (mode, pid).
    pub fn row_holders(&self, row: &[u8], owner: u64) -> Vec<(RowLockMode, Option<i32>)> {
        let s = self.state.lock();
        Self::all_row_holds(&s, row)
            .iter()
            .filter(|h| h.owner != owner)
            .map(|h| (h.mode, pid_of(h.proc)))
            .collect()
    }

    /// The strongest row lock `owner` holds on `row`.
    pub fn row_held(&self, row: &[u8], owner: u64) -> Option<RowLockMode> {
        let s = self.state.lock();
        Self::all_row_holds(&s, row)
            .iter()
            .filter(|h| h.owner == owner)
            .map(|h| h.mode)
            .max()
    }

    /// pg_blocking_pids: the backends whose holds of the lock backend `pid` waits for
    /// conflict with its request, and those ahead of it in the queue wanting a conflicting
    /// mode.
    pub fn blocking_pids(&self, pid: i32) -> Vec<i32> {
        let s = self.state.lock();
        let mut out = Vec::new();
        for (proc, w) in &s.waiters {
            if s.pids.get(proc) != Some(&pid) || w.status != Status::Waiting {
                continue;
            }
            let Some(e) = s.locks.get(&w.key) else { continue };
            for h in &e.holds {
                if h.proc != *proc
                    && w.mode.conflicts_with(h.mode)
                    && let Some(p) = s
                        .pids
                        .get(&h.proc)
                        .copied()
                        .or_else(|| pid_of(h.proc))
                        .or_else(|| s.prepared.contains(&h.proc).then_some(0))
                    && !out.contains(&p)
                {
                    out.push(p);
                }
            }
            for q in &e.queue {
                if q == proc {
                    break;
                }
                if let Some(other) = s.waiters.get(q)
                    && w.mode.conflicts_with(other.mode)
                    && let Some(p) = s.pids.get(q).copied().or_else(|| pid_of(*q))
                    && !out.contains(&p)
                {
                    out.push(p);
                }
            }
        }
        out
    }

    /// Every heavyweight lock held or awaited (pg_locks).
    pub fn snapshot(&self) -> Vec<LockInfo> {
        let s = self.state.lock();
        let mut out = Vec::new();
        for (k, e) in &s.locks {
            let tag = tag_of(k, e.xid);
            for h in &e.holds {
                if out.iter().any(|i: &LockInfo| {
                    i.tag == tag && i.owner == h.owner && i.mode == h.mode && i.granted
                }) {
                    continue;
                }
                out.push(LockInfo {
                    tag: tag.clone(),
                    pid: s.pids.get(&h.proc).copied().or_else(|| pid_of(h.proc)),
                    owner: h.owner,
                    mode: h.mode,
                    granted: true,
                    fastpath: h.fastpath,
                    wait_start: None,
                });
            }
            for p in &e.queue {
                if let Some(w) = s.waiters.get(p) {
                    out.push(LockInfo {
                        tag: tag.clone(),
                        pid: s.pids.get(p).copied().or_else(|| pid_of(*p)),
                        owner: w.owner,
                        mode: w.mode,
                        granted: false,
                        fastpath: false,
                        wait_start: Some(w.since),
                    });
                }
            }
        }
        out
    }

    /// The keys `owner` holds, with the modes (pg_locks).
    pub fn held_by(&self, owner: u64) -> Vec<(Vec<u8>, LockMode)> {
        let s = self.state.lock();
        let mut v = Vec::new();
        for (k, e) in &s.locks {
            for h in e.holds.iter().filter(|h| h.owner == owner) {
                v.push((k.clone(), h.mode));
            }
        }
        v
    }

    /// PostPrepare_Locks: what transaction `owner` holds (heavyweight locks, row locks, its
    /// transaction locks) now belongs to its prepared transaction's dummy backend `proc`,
    /// which has no pid: its holds conflict with every session, the one that prepared it
    /// too, and pg_blocking_pids reports it as 0.
    pub fn transfer(&self, owner: u64, proc: u64) {
        let mut s = self.state.lock();
        s.prepared.insert(proc);
        for e in s.locks.values_mut() {
            for h in e.holds.iter_mut().filter(|h| h.owner == owner) {
                h.proc = proc;
                h.fastpath = false;
            }
        }
        for holds in s.rows.values_mut() {
            for h in holds.iter_mut().filter(|h| h.owner == owner) {
                h.proc = proc;
            }
        }
        if let Some(b) = s.bulk.get_mut(&owner) {
            b.proc = proc;
        }
    }

    /// A prepared transaction's dummy backend is gone (COMMIT / ROLLBACK PREPARED).
    pub fn forget_prepared(&self, proc: u64) {
        self.state.lock().prepared.remove(&proc);
    }

    /// AtPrepare_Locks' check: a lock `owner` holds that `peer` (its session's own owner)
    /// holds too.
    pub fn held_by_both(&self, owner: u64, peer: u64) -> bool {
        let s = self.state.lock();
        s.locks.values().any(|e| {
            e.holds.iter().any(|h| h.owner == owner) && e.holds.iter().any(|h| h.owner == peer)
        })
    }

    /// The row locks on `row` (pgrowlocks), in the order they were taken: (mode, whether an
    /// update's or delete's, the holder's transaction id, its backend's pid).
    pub fn row_lockers(&self, row: &[u8]) -> Vec<(RowLockMode, bool, u64, Option<i32>)> {
        let s = self.state.lock();
        Self::all_row_holds(&s, row)
            .iter()
            .map(|h| {
                let xid = s.locks.get(&xact_key(h.holder)).map_or(0, |e| e.xid);
                let pid = s.pids.get(&h.proc).copied().or_else(|| pid_of(h.proc));
                (h.mode, h.modifies, xid, pid)
            })
            .collect()
    }

    /// The row locks `owner` holds: (row, strongest mode, whether an update's or delete's).
    pub fn rows_of(&self, owner: u64) -> Vec<(Vec<u8>, RowLockMode, bool)> {
        let s = self.state.lock();
        let mut out = Vec::new();
        for (row, holds) in &s.rows {
            let mine = holds.iter().filter(|h| h.owner == owner);
            let mut best: Option<(RowLockMode, bool)> = None;
            for h in mine {
                let b = best.get_or_insert((h.mode, h.modifies));
                b.0 = b.0.max(h.mode);
                b.1 |= h.modifies;
            }
            if let Some((m, modifies)) = best {
                out.push((row.clone(), m, modifies));
            }
        }
        out
    }

    /// The unique key locks `owner` recorded an in-progress index entry for: (key, the
    /// entry's transaction id).
    pub fn key_holders_of(&self, owner: u64) -> Vec<(Vec<u8>, u64)> {
        let s = self.state.lock();
        s.locks
            .iter()
            .filter_map(|(k, e)| match &e.key_holder {
                Some((o, kh)) if *o == owner => Some((k.clone(), kh.xid)),
                _ => None,
            })
            .collect()
    }

    /// A recovered prepared transaction's row lock (lock_row's grant, without waiting: at
    /// recovery nothing else holds any).
    pub fn restore_row(&self, row: &[u8], owner: u64, proc: u64, mode: RowLockMode, modifies: bool) {
        let mut s = self.state.lock();
        s.rows.entry(row.to_vec()).or_default().push(RowHold {
            holder: owner,
            owner,
            proc,
            mode,
            modifies,
        });
    }

    /// Whether anyone holds a lock (for idle detection and tests).
    pub fn held_count(&self) -> usize {
        let s = self.state.lock();
        s.locks.len() + s.rows.len() + s.bulk.values().map(BulkLocks::len).sum::<usize>()
    }

    /// Take unique key lock `key` (Exclusive) for (sub)transaction `holder` (transaction id
    /// `xid`) of transaction `owner` into its compact set, standing for its in-progress index
    /// entry. Only while nobody else holds or wants the key: false, take it the usual way
    /// (`acquire`, which first moves another's compact hold into the main table). `row_key`:
    /// the entry's heap tuple (named by the CONTEXT of a wait for it).
    pub fn hold_compact_unique_key(
        &self,
        key: &[u8],
        owner: u64,
        proc: u64,
        holder: u64,
        xid: u64,
        row_key: Option<&[u8]>,
    ) -> bool {
        let mut s = self.state.lock();
        if s.locks.contains_key(key) || s.bulk.values().any(|b| b.segs.iter().any(|g| g.ukeys.get(key).is_some())) {
            return false;
        }
        Self::hold_xact(&mut s, holder, owner, proc, xid);
        let b = s.bulk.entry(owner).or_insert_with(|| BulkLocks { proc, segs: Vec::new() });
        let done = b.seg(holder, xid).ukeys.insert_with(key, 0, row_key.unwrap_or_default());
        if done {
            COMPACT_HOLDS.fetch_add(1, Ordering::Relaxed);
        }
        done
    }

    /// Move all of `owner`'s compact locks into the main tables (PREPARE TRANSACTION: they
    /// are recorded, moved to its dummy backend and restored as any): the unique keys it
    /// holds there now, and the rows.
    pub fn expand_compact(&self, owner: u64) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let mut s = self.state.lock();
        let Some(b) = s.bulk.remove(&owner) else {
            return (Vec::new(), Vec::new());
        };
        let (mut keys, mut rows) = (Vec::new(), Vec::new());
        for g in b.segs {
            for (row, t) in g.rows.iter() {
                let (mode, modifies) = untag(t);
                let holds = s.rows.entry(row.to_vec()).or_default();
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
                let e = s.locks.entry(k.to_vec()).or_default();
                e.holds.push(Hold { owner, proc: b.proc, mode: LockMode::Exclusive, count: 1, fastpath: false });
                let row_key = (!row_key.is_empty()).then(|| row_key.to_vec());
                e.key_holder = Some((owner, KeyHolder { holder: g.holder, xid: g.xid, token: None, row: None, row_key }));
                keys.push(k.to_vec());
            }
            keys.extend(g.moved);
        }
        (keys, rows)
    }

    /// The DETAIL of a deadlock error for this cycle (DeadLockReport).
    fn report(&self, s: &State, cycle: &[(u64, Vec<u8>, LockMode)]) -> Error {
        let pid = |p: u64| s.pids.get(&p).copied().or_else(|| pid_of(p)).unwrap_or(0);
        let mut detail = String::new();
        for (i, (p, key, mode)) in cycle.iter().enumerate() {
            let next = cycle.get(i + 1).unwrap_or(&cycle[0]).0;
            let xid = s.locks.get(key).map_or(0, |e| e.xid);
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
    fn deadlock_check(&self, s: &mut State, proc: u64) -> Deadlock {
        let mut d = Dl {
            s,
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
            if let Some(e) = s.locks.get_mut(&key) {
                e.queue = procs;
            }
            Self::wakeup(s, &key);
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

/// deadlock.c's search state.
struct Dl<'s> {
    s: &'s mut State,
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

impl Dl<'_> {
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
        if self
            .s
            .waiters
            .get(&proc)
            .is_some_and(|w| w.status == Status::Waiting)
        {
            return self.find_cycle_member(proc, depth, soft);
        }
        false
    }

    fn find_cycle_member(&mut self, proc: u64, depth: usize, soft: &mut Vec<Edge>) -> bool {
        let (key, mode) = {
            let w = &self.s.waiters[&proc];
            (w.key.clone(), w.mode)
        };
        let Some(e) = self.s.locks.get(&key) else { return false };
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
            let Some(m) = self.s.waiters.get(&p).map(|w| w.mode) else { continue };
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
        let queue = self.s.locks.get(lock).map(|e| e.queue.clone()).unwrap_or_default();
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
