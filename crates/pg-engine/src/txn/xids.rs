//! Transaction ids as Postgres hands them out: cluster-wide, never reused, given to a
//! transaction when it first writes (or asks for one), and remembered as running, committed
//! or aborted for snapshots and pg_xact_status().

use crate::error::{Error, Result, code};
use crate::types::snapshot::Snapshot;
use parking_lot::Mutex;
use std::collections::BTreeSet;

/// Postgres's FirstNormalTransactionId: 0 to 2 are reserved.
pub const FIRST_NORMAL_XID: u64 = 3;
/// Where a new cluster's ids start: Postgres's initdb leaves the next id (and the frozen
/// horizon, datfrozenxid) here, its own transactions frozen and their status gone.
pub const INITDB_XID: u64 = 745;
/// How many aborted transactions to remember (older ones read as committed).
const ABORTED_KEPT: usize = 65536;

/// An exported snapshot (ExportSnapshot's file): what SET TRANSACTION SNAPSHOT installs and
/// checks.
pub struct Exported {
    /// The exporting transaction: its database (`Database::instance`) and lock owner id.
    pub db: u64,
    pub owner: u64,
    /// The database's oid (what "a different database" compares).
    pub db_oid: u32,
    pub ts: u64,
    pub serializable: bool,
    pub read_only: bool,
    /// Keeps the snapshot's versions while it's exported.
    pub pin: crate::storage::SnapshotPin,
}

struct State {
    next: u64,
    /// Ids below this may have been handed out (persisted ahead of `next`).
    reserved: u64,
    running: BTreeSet<u64>,
    /// The running ones that are subtransactions' (a snapshot's subxip, not its xip).
    subs: BTreeSet<u64>,
    aborted: BTreeSet<u64>,
    latest_completed: u64,
    /// The xmins of the snapshots transactions hold (PGPROC.xmin), by registration, with
    /// how many hold each.
    snapshots: std::collections::BTreeMap<u64, usize>,
    /// The same, by the transaction (lock owner) holding them (GetCurrentVirtualXIDs).
    owners: std::collections::HashMap<u64, Vec<u64>>,
    /// Transactions building an index on plain columns concurrently (PROC_IN_SAFE_IC):
    /// other concurrent builds don't wait for their snapshots.
    safe_ic: BTreeSet<u64>,
}

pub struct Xids {
    state: Mutex<State>,
    /// The snapshots pg_export_snapshot() exported, by identifier, until the exporting
    /// transaction ends (cluster-wide, like Postgres's pg_snapshots directory).
    exported: Mutex<std::collections::HashMap<String, Exported>>,
    /// Ids before this (and after the special ones) have no status any more.
    oldest: u64,
    persist: Box<dyn Fn(u64) + Send + Sync>,
    /// This table's own number in the process (`uid`).
    uid: u64,
    /// `horizon` as of the state's last change (`publish`): readers (pruning, the size
    /// models' per-row upkeep) load it instead of taking the state's mutex.
    horizon_now: std::sync::atomic::AtomicU64,
    /// How many times `xmin_now`'s inputs changed (a transaction id ended, or ran again at
    /// recovery), counted under the state's mutex: Postgres's xactCompletionCount. A
    /// snapshot whose count is still this one would get its xmin again
    /// (GetSnapshotDataReuse).
    completions: std::sync::atomic::AtomicU64,
}

/// A snapshot's xmin, registered (`Xids::register_snapshot`) until this is dropped.
pub struct SnapshotXmin {
    xids: std::sync::Arc<Xids>,
    xmin: u64,
    owner: Option<u64>,
    /// `Xids::completions` when the xmin was found.
    seen: u64,
}

impl SnapshotXmin {
    pub fn xmin(&self) -> u64 {
        self.xmin
    }

    /// A snapshot taken now would have another xmin: a transaction id ended since this one
    /// was found.
    pub fn stale(&self) -> bool {
        self.xids.completions.load(std::sync::atomic::Ordering::Acquire) != self.seen
    }
}

impl Drop for SnapshotXmin {
    fn drop(&mut self) {
        let mut s = self.xids.state.lock();
        if let Some(n) = s.snapshots.get_mut(&self.xmin) {
            *n -= 1;
            if *n == 0 {
                s.snapshots.remove(&self.xmin);
            }
        }
        if let Some(o) = self.owner
            && let Some(v) = s.owners.get_mut(&o)
        {
            if let Some(i) = v.iter().position(|x| *x == self.xmin) {
                v.swap_remove(i);
            }
            if v.is_empty() {
                s.owners.remove(&o);
            }
        }
        self.xids.publish(&s);
    }
}

impl std::fmt::Debug for SnapshotXmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SnapshotXmin({})", self.xmin)
    }
}

impl std::fmt::Debug for Xids {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Xids")
            .field("next", &self.state.lock().next)
            .finish()
    }
}

impl Xids {
    /// Ids from `next` on; `persist` durably records how far they may have gone.
    pub fn new(next: u64, persist: impl Fn(u64) + Send + Sync + 'static) -> Xids {
        let next = next.max(FIRST_NORMAL_XID);
        Xids {
            state: Mutex::new(State {
                next,
                reserved: next,
                running: BTreeSet::new(),
                subs: BTreeSet::new(),
                aborted: BTreeSet::new(),
                latest_completed: next - 1,
                snapshots: Default::default(),
                owners: Default::default(),
                safe_ic: BTreeSet::new(),
            }),
            exported: Mutex::new(std::collections::HashMap::new()),
            oldest: FIRST_NORMAL_XID,
            persist: Box::new(persist),
            uid: {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            },
            horizon_now: std::sync::atomic::AtomicU64::new(next),
            completions: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The horizon the state gives now, for `horizon` (under the state's mutex: every change
    /// that can move it publishes it before the mutex is let go).
    fn publish(&self, s: &State) {
        let run = Self::xmin_now(s);
        let h = s.snapshots.first_key_value().map_or(run, |(x, _)| run.min(*x));
        self.horizon_now.store(h, std::sync::atomic::Ordering::Release);
    }

    /// A number no other transaction id table of the process has (caches of ended
    /// transactions' outcomes are keyed by it).
    pub fn uid(&self) -> u64 {
        self.uid
    }

    /// With ids before `oldest` too old to have a status (0: none).
    pub fn with_oldest(mut self, oldest: u64) -> Xids {
        self.oldest = oldest.max(FIRST_NORMAL_XID);
        self
    }

    /// The oldest id whose status is known (TransamVariables->oldestClogXid).
    pub fn oldest(&self) -> u64 {
        self.oldest
    }

    /// A counter kept only in memory (a database outside an engine).
    pub fn local() -> Xids {
        Xids::new(FIRST_NORMAL_XID, |_| {})
    }

    /// A new transaction id, now running.
    pub fn assign(&self) -> u64 {
        let (x, reserve) = {
            let mut s = self.state.lock();
            let x = s.next;
            s.next += 1;
            s.running.insert(x);
            self.publish(&s);
            let reserve = (s.next > s.reserved).then(|| {
                s.reserved = s.next + 1024;
                s.reserved
            });
            (x, reserve)
        };
        if let Some(r) = reserve {
            (self.persist)(r);
        }
        x
    }

    /// A new subtransaction id, now running.
    pub fn assign_sub(&self) -> u64 {
        let x = self.assign();
        self.state.lock().subs.insert(x);
        x
    }

    /// Transaction `xid` committed or aborted.
    pub fn finish(&self, xid: u64, committed: bool) {
        let mut s = self.state.lock();
        s.running.remove(&xid);
        s.subs.remove(&xid);
        if !committed {
            s.aborted.insert(xid);
            if s.aborted.len() > ABORTED_KEPT {
                s.aborted.pop_first();
            }
        }
        s.latest_completed = s.latest_completed.max(xid);
        self.completions.fetch_add(1, std::sync::atomic::Ordering::Release);
        self.publish(&s);
    }

    /// A prepared transaction recovered at startup: its id and its subtransactions' are
    /// running again (ProcArrayAdd of its dummy proc).
    pub fn recover_running(&self, xid: u64, subs: &[u64]) {
        let mut s = self.state.lock();
        for &x in std::iter::once(&xid).chain(subs) {
            s.running.insert(x);
            s.aborted.remove(&x);
            if x >= s.next {
                s.next = x + 1;
            }
        }
        s.subs.extend(subs.iter().copied());
        self.completions.fetch_add(1, std::sync::atomic::Ordering::Release);
        self.publish(&s);
    }

    /// The oldest id running, or the next one to hand out when none runs (every later
    /// transaction's is at least this).
    pub fn oldest_running(&self) -> u64 {
        let s = self.state.lock();
        s.running.first().copied().unwrap_or(s.next)
    }

    /// The next id to hand out.
    pub fn next(&self) -> u64 {
        self.state.lock().next
    }

    /// What a snapshot taken now sees (`own`: the caller's transaction id, not listed as
    /// running but holding back xmin).
    pub fn snapshot(&self, own: Option<u64>) -> Snapshot {
        let s = self.state.lock();
        let xmax = s.latest_completed + 1;
        let xip: Vec<u64> = s
            .running
            .iter()
            .copied()
            .filter(|x| *x < xmax && Some(*x) != own && !s.subs.contains(x))
            .collect();
        let xmin = s
            .running
            .iter()
            .copied()
            .filter(|x| *x < xmax)
            .chain(std::iter::once(xmax))
            .min()
            .unwrap_or(xmax);
        Snapshot { xmin, xmax, xip }
    }

    /// The oldest transaction a snapshot taken now could see as running: its xmin.
    fn xmin_now(s: &State) -> u64 {
        let xmax = s.latest_completed + 1;
        s.running.first().map_or(xmax, |x| (*x).min(xmax))
    }

    /// A transaction took a snapshot: until the guard goes, the snapshot's xmin holds back
    /// what pruning and VACUUM may remove (the backend's PGPROC.xmin).
    pub fn register_snapshot(self: &std::sync::Arc<Self>) -> SnapshotXmin {
        self.register_snapshot_of(None)
    }

    /// `register_snapshot` for transaction (lock owner) `owner`: concurrent index builds
    /// wait for the transactions holding snapshots older than theirs.
    pub fn register_snapshot_of(self: &std::sync::Arc<Self>, owner: Option<u64>) -> SnapshotXmin {
        let mut s = self.state.lock();
        let xmin = Self::xmin_now(&s);
        *s.snapshots.entry(xmin).or_default() += 1;
        if let Some(o) = owner {
            s.owners.entry(o).or_default().push(xmin);
        }
        self.publish(&s);
        let seen = self.completions.load(std::sync::atomic::Ordering::Relaxed);
        SnapshotXmin { xids: self.clone(), xmin, owner, seen }
    }

    /// `register_snapshot_of(owner)` replacing `old` (the next statement's snapshot in place
    /// of the last one's), in one pass under the state's lock: the registration moves to the
    /// xmin a snapshot taken now has, or stays when that is its own.
    pub fn renew_snapshot(self: &std::sync::Arc<Self>, old: Option<SnapshotXmin>, owner: Option<u64>) -> SnapshotXmin {
        let mut o = match old {
            Some(o) if std::sync::Arc::ptr_eq(&o.xids, self) && o.owner == owner => o,
            old => {
                // (The new one registered before the old one goes, as assigning it did.)
                let new = self.register_snapshot_of(owner);
                drop(old);
                return new;
            }
        };
        // (Nothing ended since it was found: the same xmin, without the state's mutex.)
        if !o.stale() {
            return o;
        }
        let mut s = self.state.lock();
        let xmin = Self::xmin_now(&s);
        o.seen = self.completions.load(std::sync::atomic::Ordering::Relaxed);
        if xmin != o.xmin {
            *s.snapshots.entry(xmin).or_default() += 1;
            if let Some(n) = s.snapshots.get_mut(&o.xmin) {
                *n -= 1;
                if *n == 0 {
                    s.snapshots.remove(&o.xmin);
                }
            }
            if let Some(ow) = owner {
                let v = s.owners.entry(ow).or_default();
                match v.iter_mut().find(|x| **x == o.xmin) {
                    Some(x) => *x = xmin,
                    None => v.push(xmin),
                }
            }
            o.xmin = xmin;
        }
        self.publish(&s);
        drop(s);
        o
    }

    /// The xmin a snapshot taken now would have.
    pub fn current_xmin(&self) -> u64 {
        Self::xmin_now(&self.state.lock())
    }

    /// GetCurrentVirtualXIDs for WaitForOlderSnapshots: the transactions other than `me`
    /// holding a snapshot whose xmin is `limit` or older, not building an index on plain
    /// columns concurrently.
    pub fn older_snapshot_owners(&self, limit: u64, me: u64) -> Vec<u64> {
        let s = self.state.lock();
        let mut out: Vec<u64> = s
            .owners
            .iter()
            .filter(|(o, x)| **o != me && !s.safe_ic.contains(o) && x.iter().any(|x| *x <= limit))
            .map(|(o, _)| *o)
            .collect();
        out.sort_unstable();
        out
    }

    /// Whether transaction `owner` still holds a snapshot whose xmin is `limit` or older.
    pub fn holds_snapshot_older(&self, owner: u64, limit: u64) -> bool {
        let s = self.state.lock();
        !s.safe_ic.contains(&owner) && s.owners.get(&owner).is_some_and(|x| x.iter().any(|x| *x <= limit))
    }

    /// Mark transaction `owner` as building an index on plain columns concurrently
    /// (PROC_IN_SAFE_IC), or no longer.
    pub fn set_safe_ic(&self, owner: u64, on: bool) {
        let mut s = self.state.lock();
        if on {
            s.safe_ic.insert(owner);
        } else {
            s.safe_ic.remove(&owner);
        }
    }

    /// The pruning horizon of a database's relations (ComputeXidHorizons'
    /// data_oldest_nonremovable): deletions by transactions before it are dead to everyone.
    /// Every running transaction and every snapshot held counts.
    pub fn horizon(&self) -> u64 {
        #[cfg(debug_assertions)]
        {
            // (Under the mutex the published horizon is the state's.)
            let s = self.state.lock();
            let run = Self::xmin_now(&s);
            let h = self.horizon_now.load(std::sync::atomic::Ordering::Acquire);
            debug_assert_eq!(h, s.snapshots.first_key_value().map_or(run, |(x, _)| run.min(*x)), "published horizon");
        }
        self.horizon_now.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The pruning horizon of a session's temporary relations (temp_oldest_nonremovable):
    /// only its own changes matter, so its running transaction's id (`own`), else
    /// everything completed.
    pub fn temp_horizon(&self, own: Option<u64>) -> u64 {
        own.unwrap_or_else(|| self.state.lock().latest_completed + 1)
    }

    /// Register an exported snapshot under `id`.
    pub fn export(&self, id: String, e: Exported) {
        self.exported.lock().insert(id, e);
    }

    /// The exported snapshot `id`: (database oid, timestamp, serializable, read only).
    pub fn exported(&self, id: &str) -> Option<(u32, u64, bool, bool)> {
        self.exported
            .lock()
            .get(id)
            .map(|e| (e.db_oid, e.ts, e.serializable, e.read_only))
    }

    /// How many snapshots transaction `owner` of database `db` exported.
    pub fn exported_by(&self, db: u64, owner: u64) -> usize {
        self.exported
            .lock()
            .values()
            .filter(|e| e.db == db && e.owner == owner)
            .count()
    }

    /// The exporting transaction ended: its snapshots go.
    pub fn unexport(&self, db: u64, owner: u64) {
        self.exported
            .lock()
            .retain(|_, e| !(e.db == db && e.owner == owner));
    }

    /// pg_xact_status(): in progress, committed or aborted.
    pub fn status(&self, xid: u64) -> Result<&'static str> {
        let s = self.state.lock();
        if xid >= s.next {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("transaction ID {xid} is in the future"),
            ));
        }
        Ok(if s.running.contains(&xid) {
            "in progress"
        } else if s.aborted.contains(&xid) {
            "aborted"
        } else {
            "committed"
        })
    }
}
