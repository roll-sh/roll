//! The proc array's slots (PGPROC numbers) and virtual transaction ids.
//!
//! PostgreSQL gives every backend a PGPROC from a free list of its kind (client backends;
//! autovacuum; background workers, parallel ones included; walsenders) and returns it to the
//! tail of that list at exit, so a new backend takes the slot freed longest ago. A virtual
//! transaction id is the slot's number and a local transaction id the slot counts on across
//! the backends that use it (1 for the first backend's startup transaction, InitPostgres's).
//! pg_locks' virtualxid / virtualtransaction, log_line_prefix's %v, lock waits'
//! "virtual transaction N/M" and pg_stat_get_backend_idset show them.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock, Weak};

/// Which free list a backend's slot comes from (InitProcess's choice).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcKind {
    Backend = 0,
    Autovacuum = 1,
    BgWorker = 2,
    WalSender = 3,
}

/// A virtual transaction id: the slot's number and the local transaction id (0: none).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vxid {
    pub proc: i32,
    pub lxid: u32,
}

impl std::fmt::Display for Vxid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.proc, self.lxid)
    }
}

/// One PGPROC: its number, the local id it hands out next, and what it runs now.
#[derive(Debug)]
struct Slot {
    number: i32,
    /// The backend using it (0: none).
    pid: AtomicI32,
    next_lxid: AtomicU32,
    /// The running transaction's local id (0: none) and lock owner (0: none or not yet).
    lxid: AtomicU32,
    owner: AtomicU64,
    /// The last transaction's local id (what an error raised in it is logged with).
    last_lxid: AtomicU32,
    /// PGPROC subxid count (low 7 bits) and sticky overflow (bit 7), published together.
    subxacts: AtomicU32,
}

/// The slots of one cluster.
#[derive(Debug)]
pub struct Procs {
    free: Mutex<[VecDeque<Arc<Slot>>; 4]>,
    /// Every slot made, for finding a transaction's (`vxid_of`), and the number the next one
    /// made past the free lists takes (the engine doesn't cap sessions at max_connections).
    all: Mutex<(Vec<Arc<Slot>>, i32)>,
    /// MaxBackends: where the auxiliary processes' numbers start.
    max_backends: i32,
}

/// Every cluster's slots in the process, for naming a transaction's virtual id where only
/// its lock owner is known (`vxid_of_owner`: the lock manager's messages).
static REGISTRY: Mutex<Vec<Weak<Procs>>> = Mutex::new(Vec::new());

/// The virtual transaction id of the running transaction whose lock owner is `owner`, in
/// whichever cluster runs it.
pub fn vxid_of_owner(owner: u64) -> Option<Vxid> {
    let r: Vec<_> = REGISTRY.lock().iter().filter_map(Weak::upgrade).collect();
    r.iter().find_map(|p| p.vxid_of(owner))
}

/// The auxiliary processes after MaxBackends (InitAuxiliaryProcess: the startup process,
/// the I/O workers, the checkpointer, ...), however many I/O workers there may be.
const AUX_SLOTS: i32 = 6 + 32;

impl Procs {
    /// InitProcGlobal's lists: `max_connections` client slots, `autovacuum_worker_slots`
    /// plus the launcher's and the slot sync worker's, `max_worker_processes`,
    /// `max_wal_senders`. The launchers hold their first slots for good (the autovacuum
    /// launcher, the logical replication launcher).
    pub fn new(max_connections: usize, autovacuum_slots: usize, workers: usize, wal_senders: usize) -> Arc<Procs> {
        let sizes = [max_connections, autovacuum_slots + 2, workers, wal_senders];
        let mut all = Vec::new();
        let mut free: [VecDeque<Arc<Slot>>; 4] = Default::default();
        let mut n = 0i32;
        for (kind, size) in sizes.iter().enumerate() {
            for i in 0..*size {
                let s = Arc::new(Slot::new(n));
                n += 1;
                all.push(s.clone());
                // (The launchers': taken at the start, never freed.)
                let launcher = i == 0 && (kind == ProcKind::Autovacuum as usize || kind == ProcKind::BgWorker as usize);
                if !launcher {
                    free[kind].push_back(s);
                }
            }
        }
        let p = Arc::new(Procs { free: Mutex::new(free), all: Mutex::new((all, n + AUX_SLOTS)), max_backends: n });
        let mut r = REGISTRY.lock();
        r.retain(|w| w.strong_count() > 0);
        r.push(Arc::downgrade(&p));
        p
    }

    /// The slots of clusters without a server (embedded and test sessions).
    pub fn fallback() -> &'static Arc<Procs> {
        static P: OnceLock<Arc<Procs>> = OnceLock::new();
        P.get_or_init(|| Procs::new(100, 16, 8, 10))
    }

    /// An auxiliary process's number (0: the first after MaxBackends).
    pub fn aux_number(&self, i: i32) -> i32 {
        self.max_backends + i
    }

    /// The autovacuum launcher's and the logical replication launcher's numbers.
    pub fn launcher_numbers(&self, max_connections: usize, autovacuum_slots: usize) -> (i32, i32) {
        (max_connections as i32, (max_connections + autovacuum_slots + 2) as i32)
    }

    /// InitProcess for backend `pid`: the head of `kind`'s free list (a new slot past the
    /// others when it is empty), its startup transaction begun.
    pub fn assign(self: &Arc<Self>, kind: ProcKind, pid: i32) -> Arc<Proc> {
        let slot = self.free.lock()[kind as usize].pop_front().unwrap_or_else(|| {
            let mut all = self.all.lock();
            let s = Arc::new(Slot::new(all.1));
            all.1 += 1;
            all.0.push(s.clone());
            s
        });
        self.take(slot, kind, pid)
    }

    /// `assign` for a background worker that isn't started when every slot is taken
    /// (RegisterDynamicBackgroundWorker fails: a Gather launches fewer workers).
    pub fn try_assign(self: &Arc<Self>, kind: ProcKind, pid: i32) -> Option<Arc<Proc>> {
        let slot = self.free.lock()[kind as usize].pop_front()?;
        Some(self.take(slot, kind, pid))
    }

    fn take(self: &Arc<Self>, slot: Arc<Slot>, kind: ProcKind, pid: i32) -> Arc<Proc> {
        slot.pid.store(pid, Relaxed);
        let p = Proc { slot, kind, procs: Arc::downgrade(self) };
        p.begin();
        Arc::new(p)
    }

    /// What the slots take.
    pub fn memory_bytes(&self) -> usize {
        self.all.lock().0.len() * (std::mem::size_of::<Slot>() + 16)
    }

    /// The backends' slots in use: (pid, the running transaction's lock owner or 0, its
    /// virtual transaction id), in slot order.
    pub fn running(&self) -> Vec<(i32, u64, Vxid)> {
        let all = self.all.lock();
        let mut v: Vec<_> = all
            .0
            .iter()
            .filter_map(|s| {
                let pid = s.pid.load(Relaxed);
                (pid != 0).then(|| (pid, s.owner.load(Relaxed), s.current()))
            })
            .collect();
        v.sort_by_key(|r| r.2.proc);
        v
    }

    /// The virtual transaction id of the transaction whose lock owner is `owner`, if a
    /// backend runs it.
    pub fn vxid_of(&self, owner: u64) -> Option<Vxid> {
        let all = self.all.lock();
        all.0.iter().find(|s| s.owner.load(Relaxed) == owner && owner != 0).map(|s| s.current())
    }
}

impl Slot {
    fn new(number: i32) -> Slot {
        Slot {
            number,
            pid: AtomicI32::new(0),
            next_lxid: AtomicU32::new(1),
            lxid: AtomicU32::new(0),
            owner: AtomicU64::new(0),
            last_lxid: AtomicU32::new(0),
            subxacts: AtomicU32::new(0),
        }
    }

    fn current(&self) -> Vxid {
        Vxid { proc: self.number, lxid: self.lxid.load(Relaxed) }
    }
}

/// A backend's slot, returned to its free list's tail when the backend's last holder drops
/// it (ProcKill).
#[derive(Debug)]
pub struct Proc {
    slot: Arc<Slot>,
    kind: ProcKind,
    procs: Weak<Procs>,
}

impl Proc {
    pub fn number(&self) -> i32 {
        self.slot.number
    }

    pub fn subxacts(&self) -> (usize, bool) {
        let status = self.slot.subxacts.load(Relaxed);
        ((status & 127) as usize, status & 128 != 0)
    }

    pub fn set_subxacts(&self, (count, overflowed): (usize, bool)) {
        self.slot.subxacts.store(count as u32 | (u32::from(overflowed) << 7), Relaxed);
    }

    /// The running transaction's id (local id 0 between transactions).
    pub fn current(&self) -> Vxid {
        self.slot.current()
    }

    /// StartTransaction: a transaction begins (start_xact_command), with the next local id
    /// (GetNextLocalTransactionId, which skips 0). One already begun keeps its id.
    pub fn begin(&self) -> Vxid {
        let s = &self.slot;
        if s.lxid.load(Relaxed) == 0 {
            let mut l = s.next_lxid.fetch_add(1, Relaxed);
            if l == 0 {
                l = s.next_lxid.fetch_add(1, Relaxed);
            }
            s.lxid.store(l, Relaxed);
            s.last_lxid.store(l, Relaxed);
        }
        s.current()
    }

    /// The running transaction's id, else the last one's (an error's: PostgreSQL logs it
    /// before the transaction aborts).
    pub fn current_or_last(&self) -> Vxid {
        let v = self.current();
        if v.lxid != 0 { v } else { Vxid { proc: v.proc, lxid: self.slot.last_lxid.load(Relaxed) } }
    }

    /// The transaction begun runs as lock owner `owner` (what other backends' lock rows and
    /// waits name).
    pub fn set_owner(&self, owner: u64) {
        self.slot.owner.store(owner, Relaxed);
    }

    /// A command running transactions of its own (VACUUM, CREATE INDEX CONCURRENTLY):
    /// the next one begins, as the same lock owner.
    pub fn next_transaction(&self) {
        let owner = self.slot.owner.load(Relaxed);
        self.slot.lxid.store(0, Relaxed);
        self.begin();
        self.slot.owner.store(owner, Relaxed);
    }

    /// The transaction ended (ProcArrayEndTransaction).
    pub fn end(&self) {
        self.set_subxacts((0, false));
        self.slot.lxid.store(0, Relaxed);
        self.slot.owner.store(0, Relaxed);
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.end();
        self.slot.pid.store(0, Relaxed);
        if let Some(p) = self.procs.upgrade() {
            p.free.lock()[self.kind as usize].push_back(self.slot.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_reused_in_order_and_keep_counting() {
        let p = Procs::new(3, 1, 2, 1);
        let a = p.assign(ProcKind::Backend, 1);
        assert_eq!(a.current(), Vxid { proc: 0, lxid: 1 });
        a.end();
        assert_eq!(a.begin().to_string(), "0/2");
        let b = p.assign(ProcKind::Backend, 1);
        drop(a);
        let c = p.assign(ProcKind::Backend, 1);
        assert_eq!((b.number(), c.number()), (1, 2));
        // Slot 0 went to the tail: the next backend takes it, its ids going on.
        let d = p.assign(ProcKind::Backend, 1);
        assert_eq!(d.current().to_string(), "0/3");
        // Past the free list: a new slot after the auxiliary processes'.
        let e = p.assign(ProcKind::Backend, 1);
        assert_eq!(e.number(), 3 + 3 + 2 + 1 + AUX_SLOTS);
        // The launchers keep their slots: the first worker of each kind takes the second.
        assert_eq!(p.assign(ProcKind::Autovacuum, 1).number(), 4);
        assert_eq!(p.assign(ProcKind::BgWorker, 1).number(), 7);
        assert_eq!(p.assign(ProcKind::WalSender, 1).number(), 8);
        b.set_owner(42);
        assert_eq!(p.vxid_of(42), Some(Vxid { proc: 1, lxid: 1 }));
    }
}
