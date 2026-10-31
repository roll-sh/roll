//! Parallel apply (applyparallelworker.c): with streaming = parallel the leader apply worker
//! hands each streamed in-progress transaction to a parallel apply worker, a thread with a
//! session of its own that applies the changes as they arrive, in a local transaction kept
//! open across the stream's blocks, and commits it when the leader passes the Stream Commit
//! on. The leader waits for that commit before it goes on, so local commits keep the
//! publisher's order.
//!
//! Deadlocks between them go through the lock manager, as in PostgreSQL: the leader holds the
//! transaction's stream lock (an `applytransaction` lock, objid 0) between blocks and the worker
//! takes it in AccessShare mode before it reads the next block; the worker holds its
//! transaction lock (objid 1) until its transaction ends and the leader waits for it at the
//! transaction's end. A row lock the leader waits for while the worker holding it waits for
//! its next block is then a cycle DeadLockCheck reports ("deadlock detected"). A queue that
//! stays full for 10 s (or debug_logical_replication_streaming = immediate) turns the rest of
//! the transaction into a file (partial serialization) the worker applies at its end.

use super::super::apply_stream::Spool;
use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

/// DSM_QUEUE_SIZE: how much the leader's queue to one worker holds.
const QUEUE_SIZE: usize = 16_000_000;
/// pa_send_data: a full queue is retried every second, for at most 9 s
/// (SHM_SEND_RETRY_INTERVAL_MS, SHM_SEND_TIMEOUT_MS).
pub(super) const SEND_RETRY: Duration = Duration::from_millis(1000);
pub(super) const SEND_TIMEOUT: Duration = Duration::from_millis(9000);
/// PARALLEL_APPLY_LOCK_STREAM / _XACT: the objid of the two applytransaction locks.
pub(super) const LOCK_STREAM: u16 = 0;
pub(super) const LOCK_XACT: u16 = 1;

/// ParallelTransState.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum XactState {
    Unknown,
    Started,
    Finished,
}

/// PartialFileSetState.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileSet {
    Empty,
    SerializeInProgress,
    SerializeDone,
    Ready,
}

struct State {
    xid: u32,
    xact: XactState,
    fileset: FileSet,
    /// The leader's partially serialized messages (whole pgoutput messages).
    file: Option<Spool>,
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    /// The error the worker ended with (its ErrorResponse on the error queue).
    error: Option<String>,
    /// The worker's thread ended.
    gone: bool,
    /// The leader asked it to finish (logicalrep_pa_worker_stop).
    finish: bool,
    /// The worker's session's cancel flag, once it has one (to end a wait when it stops).
    cancel: Option<Arc<AtomicBool>>,
}

/// ParallelApplyWorkerShared plus the message queue between the leader and one worker.
pub(super) struct Shared {
    st: Mutex<State>,
    cv: Condvar,
    /// pending_stream_count: stream blocks sent that the worker hasn't finished.
    pub pending: AtomicU32,
}

pub(super) enum Recv {
    Msg(Vec<u8>),
    Empty,
    Finish,
}

impl Shared {
    pub fn new() -> Shared {
        Shared {
            st: Mutex::new(State {
                xid: 0,
                xact: XactState::Unknown,
                fileset: FileSet::Empty,
                file: None,
                queue: VecDeque::new(),
                bytes: 0,
                error: None,
                gone: false,
                finish: false,
                cancel: None,
            }),
            cv: Condvar::new(),
            pending: AtomicU32::new(0),
        }
    }

    pub fn xid(&self) -> u32 {
        self.st.lock().xid
    }

    pub fn xact(&self) -> XactState {
        self.st.lock().xact
    }

    pub fn set_xact(&self, s: XactState) {
        self.st.lock().xact = s;
        self.cv.notify_all();
    }

    pub fn fileset(&self) -> FileSet {
        self.st.lock().fileset
    }

    pub fn set_fileset(&self, f: FileSet) {
        self.st.lock().fileset = f;
        self.cv.notify_all();
    }

    pub fn set_cancel(&self, c: Arc<AtomicBool>) {
        self.st.lock().cancel = Some(c);
    }

    /// shm_mq_send without waiting: false when the queue has no room for `m`.
    pub fn try_send(&self, m: &[u8]) -> Result<bool, String> {
        let mut st = self.st.lock();
        if st.gone {
            return Err("could not send data to shared-memory queue".into());
        }
        if st.bytes > 0 && st.bytes + m.len() > QUEUE_SIZE {
            return Ok(false);
        }
        st.bytes += m.len();
        st.queue.push_back(m.to_vec());
        drop(st);
        self.cv.notify_all();
        Ok(true)
    }

    /// shm_mq_receive, waiting up to `wait` for a message.
    pub fn recv(&self, wait: Duration) -> Recv {
        let mut st = self.st.lock();
        if st.queue.is_empty() && !st.finish {
            self.cv.wait_for(&mut st, wait);
        }
        if st.finish {
            return Recv::Finish;
        }
        match st.queue.pop_front() {
            Some(m) => {
                st.bytes -= m.len();
                Recv::Msg(m)
            }
            None => Recv::Empty,
        }
    }

    /// The worker's thread ends (with its error, if any).
    pub fn exit(&self, error: Option<String>) {
        let mut st = self.st.lock();
        st.gone = true;
        if st.error.is_none() {
            st.error = error;
        }
        drop(st);
        self.cv.notify_all();
    }

    /// The file of the leader's partially serialized messages starts (or is written to).
    pub fn with_file<T>(&self, f: impl FnOnce(&mut Option<Spool>) -> T) -> T {
        f(&mut self.st.lock().file)
    }

    /// Wait (up to 10 ms, as pa_wait_for_xact_state's latch) for the worker's state to move.
    pub fn wait_change(&self) {
        let mut st = self.st.lock();
        if !st.gone {
            self.cv.wait_for(&mut st, Duration::from_millis(10));
        }
    }

    /// The worker's error, or that it is gone: (error, gone).
    pub fn failed(&self) -> Option<(Option<String>, bool)> {
        let st = self.st.lock();
        (st.error.is_some() || st.gone).then(|| (st.error.clone(), st.gone))
    }

    fn assign(&self, xid: u32) {
        let mut st = self.st.lock();
        st.xid = xid;
        st.xact = XactState::Unknown;
    }
}

/// A parallel apply worker as its leader keeps it (ParallelApplyWorkerInfo).
pub(super) struct PaWorker {
    pub shared: Arc<Shared>,
    pub stop: Arc<AtomicBool>,
    pub thread: Option<std::thread::JoinHandle<()>>,
    in_use: bool,
    /// The leader writes the rest of its transaction to a file (serialize_changes).
    pub serialize: bool,
}

impl PaWorker {
    pub fn new(shared: Arc<Shared>, stop: Arc<AtomicBool>, thread: std::thread::JoinHandle<()>) -> PaWorker {
        PaWorker { shared, stop, thread: Some(thread), in_use: false, serialize: false }
    }

    /// The worker's thread ends: `finish` asks it to (it logs "has finished"), otherwise it is
    /// terminated (as logicalrep_worker_detach's SIGTERM).
    pub fn end(mut self, finish: bool) {
        {
            let mut st = self.shared.st.lock();
            if finish {
                st.finish = true;
            } else {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(c) = &st.cancel {
                    c.store(true, Ordering::SeqCst);
                    crate::txn::locks::interrupt(c);
                }
            }
        }
        self.shared.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The leader's parallel apply workers (ParallelApplyWorkerPool, ParallelApplyTxnHash).
#[derive(Default)]
pub(super) struct Pool {
    workers: HashMap<u64, PaWorker>,
    next: u64,
    by_xid: HashMap<u32, u64>,
    /// The worker of the stream block being received (stream_apply_worker).
    pub stream: Option<u64>,
}

impl Pool {
    pub fn get(&self, id: u64) -> &PaWorker {
        &self.workers[&id]
    }

    pub fn get_mut(&mut self, id: u64) -> &mut PaWorker {
        self.workers.get_mut(&id).expect("parallel apply worker")
    }

    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// pa_find_worker.
    pub fn find(&self, xid: u32) -> Option<u64> {
        if xid == 0 {
            return None;
        }
        self.stream.or_else(|| self.by_xid.get(&xid).copied())
    }

    /// A transaction is handed to a worker.
    pub fn busy(&self) -> bool {
        !self.by_xid.is_empty()
    }

    /// A free worker of the pool (pa_launch_parallel_worker's reuse).
    pub fn idle(&self) -> Option<u64> {
        let mut ids: Vec<u64> = self.workers.iter().filter(|(_, w)| !w.in_use).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids.first().copied()
    }

    pub fn add(&mut self, w: PaWorker) -> u64 {
        self.next += 1;
        self.workers.insert(self.next, w);
        self.next
    }

    /// pa_allocate_worker's bookkeeping: worker `id` applies transaction `xid`.
    pub fn assign(&mut self, id: u64, xid: u32) {
        let w = self.get_mut(id);
        w.shared.assign(xid);
        w.in_use = true;
        w.serialize = false;
        self.by_xid.insert(xid, id);
    }

    /// pa_free_worker: its transaction is done; it stays in the pool unless it serialized or
    /// the pool holds more than half of max_parallel_apply_workers_per_subscription.
    pub fn free(&mut self, id: u64, max: usize) {
        let xid = self.get(id).shared.xid();
        self.by_xid.remove(&xid);
        if self.stream == Some(id) {
            self.stream = None;
        }
        let w = self.get_mut(id);
        if w.serialize || self.workers.len() > max / 2 {
            if let Some(w) = self.workers.remove(&id) {
                w.end(true);
            }
            return;
        }
        let w = self.get_mut(id);
        w.in_use = false;
        w.serialize = false;
        w.shared.set_fileset(FileSet::Empty);
    }

    /// The workers whose thread ended while they were free (an error between transactions).
    pub fn reap(&mut self) -> Vec<Option<String>> {
        let gone: Vec<u64> = self.workers.iter().filter(|(_, w)| !w.in_use && w.shared.failed().is_some()).map(|(id, _)| *id).collect();
        gone.into_iter().filter_map(|id| self.workers.remove(&id)).map(|w| w.shared.failed().and_then(|f| f.0)).collect()
    }

    /// The workers handed a transaction.
    pub fn in_use(&self) -> Vec<u64> {
        self.workers.iter().filter(|(_, w)| w.in_use).map(|(id, _)| *id).collect()
    }

    /// Every worker terminated (the leader exits).
    pub fn shutdown(&mut self) {
        self.by_xid.clear();
        self.stream = None;
        for (_, w) in self.workers.drain() {
            w.end(false);
        }
    }
}

/// How the leader handles a streamed transaction's messages (TransApplyAction).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    /// TRANS_LEADER_SERIALIZE (TRANS_LEADER_APPLY outside a stream block): spooled by the
    /// leader, applied at its end.
    Serialize,
    /// TRANS_LEADER_SEND_TO_PARALLEL.
    Send(u64),
    /// TRANS_LEADER_PARTIAL_SERIALIZE.
    Partial(u64),
}

impl Pool {
    /// get_transaction_apply_action for the leader.
    pub fn action(&self, xid: u32) -> Action {
        match self.find(xid) {
            Some(id) if self.get(id).serialize => Action::Partial(id),
            Some(id) => Action::Send(id),
            None => Action::Serialize,
        }
    }
}

use super::{Reader, RemoteTxn, Worker, Workers, lsn, open_worker};
use crate::catalog::Subscription;
use crate::engine::Engine;
use crate::error::Severity;
use crate::txn::locks::{LockMode, WaitPolicy, Waits, apply_transaction_key};

/// pa_savepoint_name.
fn savepoint_name(subid: u32, xid: u32) -> String {
    format!("pg_sp_{subid}_{xid}")
}

/// An error as the worker's message string.
fn error_text(e: crate::error::Error) -> String {
    let mut m = e.message.clone();
    for (k, v) in [("DETAIL", e.detail.as_deref()), ("HINT", e.hint.as_deref()), ("CONTEXT", e.context.as_deref())] {
        if let Some(v) = v {
            m.push_str(&format!("\n{k}:  {v}"));
        }
    }
    m
}

/// ProcessParallelApplyMessage: a worker's error as its leader raises it.
fn exited_due_to_error(err: &str) -> String {
    let ctx = super::split_message(err).3.map(|c| format!("{c}\n")).unwrap_or_default();
    format!("logical replication parallel apply worker exited due to error\nCONTEXT:  {ctx}logical replication parallel apply worker")
}

/// ParallelApplyWorkerMain: a parallel apply worker's thread.
fn pa_main(
    engine: std::sync::Weak<Engine>,
    workers: Arc<Workers>,
    sub: Subscription,
    leader: i32,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(e) = engine.upgrade() else { return Ok(()) };
        let mut w = open_worker(&e, &workers, &sub, &stop, super::Role::Parallel(leader)).inspect_err(|m| tracing::error!(subscription = %sub.name, "{m}"))?;
        w.pa = Some(shared.clone());
        shared.set_cancel(w.sess.cancel.clone());
        let r = w.pa_loop(&shared);
        if let Err(m) = &r {
            if stop.load(Ordering::SeqCst) {
                w.log(Severity::Fatal, "terminating logical replication worker due to administrator command");
                w.exited();
                return Ok(());
            }
            w.log(Severity::Error, m);
            w.exited();
            // (Set before the worker's locks go: a leader waiting for them sees the error.)
            shared.exit_error(m);
        }
        r
    }));
    let err = match r {
        Ok(Ok(())) => None,
        Ok(Err(m)) => Some(m),
        Err(_) => Some("logical replication parallel apply worker panicked".to_string()),
    };
    shared.exit(err);
}

impl Shared {
    fn exit_error(&self, m: &str) {
        let mut st = self.st.lock();
        if st.error.is_none() {
            st.error = Some(m.to_string());
        }
    }
}

impl Worker<'_> {
    /// LockApplyTransactionForSession (waits as any heavyweight lock does, deadlock checks
    /// included).
    fn apply_lock(&mut self, xid: u32, objid: u16, mode: LockMode) -> Result<(), String> {
        let key = apply_transaction_key(self.sub.dbid, self.sub.oid, xid, objid);
        let st = &self.sess.state;
        let owner = st.lock_owner;
        let mut w = Waits::new(WaitPolicy::Block, &self.sess.cancel);
        w.deadlock_timeout = Duration::from_millis(st.settings.borrow().int("deadlock_timeout").max(1) as u64);
        st.db.locks.acquire(&key, owner, owner, mode, &w, &String::new).map_err(error_text)?;
        self.apply_locks.push((key, mode));
        Ok(())
    }

    /// UnlockApplyTransactionForSession.
    fn apply_unlock(&mut self, xid: u32, objid: u16, mode: LockMode) {
        let key = apply_transaction_key(self.sub.dbid, self.sub.oid, xid, objid);
        if let Some(i) = self.apply_locks.iter().position(|(k, m)| *k == key && *m == mode) {
            self.apply_locks.remove(i);
            self.sess.state.db.locks.release(&key, self.sess.state.lock_owner, mode);
        }
    }

    /// pa_can_start.
    fn pa_can_start(&self) -> bool {
        self.role == super::Role::Apply && self.sub.stream == 'p' && self.skip_lsn() == 0 && self.states.values().all(|(s, _)| *s == 'r')
    }

    /// pa_allocate_worker: transaction `xid` goes to a free worker of the pool or a new one,
    /// if it can.
    fn pa_allocate(&mut self, xid: u32) {
        if !self.pa_can_start() {
            return;
        }
        let id = match self.pool.idle() {
            Some(id) => id,
            None => match self.pa_launch() {
                Some(id) => id,
                None => return,
            },
        };
        self.pool.assign(id, xid);
    }

    /// pa_launch_parallel_worker / logicalrep_worker_launch: none past
    /// max_parallel_apply_workers_per_subscription, nor past max_logical_replication_workers.
    fn pa_launch(&mut self) -> Option<u64> {
        let max = self.setting_int("max_parallel_apply_workers_per_subscription").max(0) as usize;
        if self.pool.len() >= max {
            return None;
        }
        if self.workers.slots_used() >= self.setting_int("max_logical_replication_workers").max(0) as usize {
            self.log(
                Severity::Warning,
                "out of logical replication worker slots\nHINT:  You might need to increase \"max_logical_replication_workers\".",
            );
            return None;
        }
        let shared = Arc::new(Shared::new());
        let stop = Arc::new(AtomicBool::new(false));
        let (engine, workers, sub, leader) = (Arc::downgrade(self.e), self.workers.clone(), self.sub.clone(), self.pid);
        let (sh, st) = (shared.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name(format!("roll-pa-{}", self.sub.oid))
            .spawn(move || pa_main(engine, workers, sub, leader, sh, st))
            .ok()?;
        Some(self.pool.add(PaWorker::new(shared, stop, thread)))
    }

    /// pa_send_data: false when the queue stayed full for SEND_TIMEOUT (or
    /// debug_logical_replication_streaming = immediate): the caller serializes instead.
    fn pa_send(&mut self, id: u64, m: &[u8]) -> Result<bool, String> {
        if self.sess.state.settings.borrow().get_str("debug_logical_replication_streaming") == Some("immediate") {
            return Ok(false);
        }
        let shared = self.pool.get(id).shared.clone();
        let mut full_since = None;
        loop {
            let sent = shared.try_send(m);
            if sent.is_err() {
                self.pa_check()?;
            }
            if sent? {
                return Ok(true);
            }
            self.pa_check()?;
            if self.stop.load(Ordering::SeqCst) {
                return Err("terminating logical replication worker due to administrator command".into());
            }
            let since = *full_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= SEND_TIMEOUT + SEND_RETRY {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// pa_switch_to_partial_serialize: the rest of worker `id`'s transaction goes to a file.
    fn pa_switch_to_partial(&mut self, id: u64, stream_locked: bool) -> Result<(), String> {
        let shared = self.pool.get(id).shared.clone();
        let xid = shared.xid();
        self.log(
            Severity::Log,
            &format!("logical replication apply worker will serialize the remaining changes of remote transaction {xid} to a file"),
        );
        self.pool.get_mut(id).serialize = true;
        let spool = Spool::new(&self.sess.state.db.store.spill_dir())?;
        shared.with_file(|f| *f = Some(spool));
        if !stream_locked {
            self.apply_lock(xid, LOCK_STREAM, LockMode::AccessExclusive)?;
        }
        shared.set_fileset(FileSet::SerializeInProgress);
        Ok(())
    }

    /// stream_write_change for a partially serialized transaction (the whole message).
    fn pa_write(&mut self, id: u64, m: &[u8]) -> Result<(), String> {
        let shared = self.pool.get(id).shared.clone();
        shared.with_file(|f| match f {
            Some(s) => s.write(0, 0, m),
            None => Err("no changes file for a serialized transaction".into()),
        })
    }

    /// ProcessParallelApplyMessages: a worker's error is the leader's.
    pub(super) fn pa_check(&mut self) -> Result<(), String> {
        if let Some(e) = self.pool.reap().into_iter().flatten().next() {
            return Err(exited_due_to_error(&e));
        }
        for id in self.pool.in_use() {
            match self.pool.get(id).shared.failed() {
                Some((Some(e), _)) => return Err(exited_due_to_error(&e)),
                Some((None, true)) => return Err("lost connection to the logical replication parallel apply worker".into()),
                _ => {}
            }
        }
        Ok(())
    }

    /// pa_xact_finish: the worker's transaction ends (the leader waits for it, keeping the
    /// publisher's commit order), and the worker goes back to the pool or stops.
    fn pa_xact_finish(&mut self, id: u64) -> Result<(), String> {
        let shared = self.pool.get(id).shared.clone();
        let xid = shared.xid();
        self.apply_unlock(xid, LOCK_STREAM, LockMode::AccessExclusive);
        // pa_wait_for_xact_finish.
        while shared.xact() < XactState::Started {
            self.pa_check()?;
            if self.stop.load(Ordering::SeqCst) {
                return Err("terminating logical replication worker due to administrator command".into());
            }
            shared.wait_change();
        }
        self.apply_lock(xid, LOCK_XACT, LockMode::AccessShare)?;
        self.apply_unlock(xid, LOCK_XACT, LockMode::AccessShare);
        if shared.xact() != XactState::Finished {
            self.pa_check()?;
            return Err("lost connection to the logical replication parallel apply worker".into());
        }
        let max = self.setting_int("max_parallel_apply_workers_per_subscription").max(0) as usize;
        self.pool.free(id, max);
        Ok(())
    }
}

/// logicalrep_message_type.
fn message_name(kind: u8) -> Option<&'static str> {
    Some(match kind {
        b'B' => "BEGIN",
        b'C' => "COMMIT",
        b'O' => "ORIGIN",
        b'I' => "INSERT",
        b'U' => "UPDATE",
        b'D' => "DELETE",
        b'T' => "TRUNCATE",
        b'R' => "RELATION",
        b'Y' => "TYPE",
        b'M' => "MESSAGE",
        b'b' => "BEGIN PREPARE",
        b'P' => "PREPARE",
        b'K' => "COMMIT PREPARED",
        b'r' => "ROLLBACK PREPARED",
        b'S' => "STREAM START",
        b'E' => "STREAM STOP",
        b'c' => "STREAM COMMIT",
        b'A' => "STREAM ABORT",
        b'p' => "STREAM PREPARE",
        _ => return None,
    })
}

impl Worker<'_> {
    /// apply_error_callback's CONTEXT for message `m` (set_apply_error_context_xact's
    /// transaction and finish position).
    pub(super) fn message_context(&self, m: &[u8]) -> Option<String> {
        let kind = *m.first()?;
        let name = message_name(kind)?;
        let mut r = Reader { b: m, at: 1 };
        let (xid, finish) = match kind {
            b'S' => (r.u32().unwrap_or(0), 0),
            b'E' => (self.stream_block.unwrap_or(0), 0),
            b'A' => {
                let _ = r.u32();
                let sub = r.u32().unwrap_or(0);
                (sub, if m.len() >= 25 { r.u64().unwrap_or(0) } else { 0 })
            }
            b'c' => {
                let x = r.u32().unwrap_or(0);
                let _ = r.u8();
                (x, r.u64().unwrap_or(0))
            }
            b'B' | b'b' => {
                let l = r.u64().unwrap_or(0);
                let _ = (r.u64(), if kind == b'b' { r.u64() } else { Ok(0) });
                (r.u32().unwrap_or(0), l)
            }
            b'P' | b'K' | b'p' | b'r' => {
                let _ = r.u8();
                let first = r.u64().unwrap_or(0);
                let end = r.u64().unwrap_or(0);
                let _ = r.u64();
                if kind == b'r' {
                    let _ = r.u64();
                }
                (r.u32().unwrap_or(0), if kind == b'r' { end } else { first })
            }
            _ => match (self.stream_block, &self.txn) {
                (Some(x), _) => (x, 0),
                (None, Some(t)) => (t.xid, t.final_lsn),
                (None, None) => (0, 0),
            },
        };
        let mut s = format!("processing remote data for replication origin \"pg_{}\" during message type \"{name}\"", self.sub.oid);
        if xid != 0 {
            s.push_str(&format!(" in transaction {xid}"));
            if finish != 0 {
                s.push_str(&format!(", finished at {}", lsn(finish)));
            }
        }
        Some(s)
    }
}

impl Worker<'_> {
    /// LogicalParallelApplyLoop: the leader's messages, then (once the queue is empty) a
    /// partially serialized transaction's file.
    fn pa_loop(&mut self, shared: &Shared) -> Result<(), String> {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                self.log(Severity::Fatal, "terminating logical replication worker due to administrator command");
                self.exited();
                return Ok(());
            }
            self.sess.poll_config();
            match shared.recv(Duration::from_millis(1000)) {
                Recv::Finish => {
                    let m = format!("logical replication parallel apply worker for subscription \"{}\" has finished", self.sub.name);
                    self.log(Severity::Log, &m);
                    return Ok(());
                }
                Recv::Msg(m) => self.apply(&m)?,
                Recv::Empty => self.pa_spooled()?,
            }
        }
    }

    /// pa_process_spooled_messages_if_required.
    fn pa_spooled(&mut self) -> Result<(), String> {
        let shared = self.pa.clone().expect("parallel apply worker");
        let mut fs = shared.fileset();
        if fs == FileSet::SerializeInProgress {
            let xid = shared.xid();
            self.apply_lock(xid, LOCK_STREAM, LockMode::AccessShare)?;
            self.apply_unlock(xid, LOCK_STREAM, LockMode::AccessShare);
            fs = shared.fileset();
        }
        match fs {
            FileSet::SerializeDone => shared.set_fileset(FileSet::Ready),
            FileSet::Ready => {
                if let Some(spool) = shared.with_file(|f| f.take()) {
                    spool.replay(|m| self.apply(m))?;
                }
                shared.set_fileset(FileSet::Empty);
            }
            _ => {}
        }
        Ok(())
    }

    /// pa_decr_and_wait_stream_block: a stream block done; past the last one sent, wait for
    /// the leader's next (its stream lock).
    fn pa_decr_and_wait(&mut self) -> Result<(), String> {
        let shared = self.pa.clone().expect("parallel apply worker");
        if shared.pending.load(Ordering::SeqCst) == 0 {
            if shared.fileset() != FileSet::Empty {
                return Ok(());
            }
            return Err("invalid pending streaming chunk 0".into());
        }
        if shared.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
            let xid = shared.xid();
            self.apply_lock(xid, LOCK_STREAM, LockMode::AccessShare)?;
            self.apply_unlock(xid, LOCK_STREAM, LockMode::AccessShare);
        }
        Ok(())
    }

    /// handle_streamed_transaction for a change in a stream block (`m` with its xid). Ok(true):
    /// the caller applies it here too (without the xid).
    pub(super) fn streamed_change(&mut self, top: u32, kind: u8, m: &[u8]) -> Result<bool, String> {
        let xid = Reader { b: m, at: 1 }.u32()?;
        if xid == 0 {
            return Err("invalid transaction ID in streamed replication transaction".into());
        }
        if self.pa.is_some() {
            self.nchanges += 1;
            // pa_start_subtrans.
            if xid != top && !self.subxacts.contains(&xid) {
                let sp = savepoint_name(self.sub.oid, xid);
                self.log(Severity::Debug, &format!("defining savepoint {sp} in logical replication parallel apply worker"));
                self.query(&format!("SAVEPOINT {sp}"))?;
                self.subxacts.push(xid);
            }
            return Ok(true);
        }
        // (The leader keeps relation and type messages of what it sends on too.)
        let schema = matches!(kind, b'R' | b'Y');
        match self.pool.action(top) {
            Action::Serialize => {
                let mut msg = Vec::with_capacity(m.len() - 4);
                msg.push(kind);
                msg.extend_from_slice(&m[5..]);
                let spool = self.spools.get_mut(&top).ok_or("streamed change outside a streamed transaction")?;
                spool.write(top, xid, &msg)?;
                Ok(false)
            }
            Action::Send(id) => {
                if !self.pa_send(id, m)? {
                    self.pa_switch_to_partial(id, false)?;
                    self.pa_write(id, m)?;
                }
                Ok(schema)
            }
            Action::Partial(id) => {
                self.pa_write(id, m)?;
                Ok(schema)
            }
        }
    }

    /// apply_handle_stream_start.
    pub(super) fn stream_start(&mut self, m: &[u8]) -> Result<(), String> {
        let mut r = Reader { b: m, at: 1 };
        let xid = r.u32()?;
        let first = r.u8()? == 1;
        if self.stream_block.is_some() {
            return Err("duplicate STREAM START message".into());
        }
        if xid == 0 {
            return Err("invalid transaction ID in streamed replication transaction".into());
        }
        self.stream_block = Some(xid);
        if let Some(shared) = self.pa.clone() {
            if first {
                self.apply_lock(xid, LOCK_XACT, LockMode::AccessExclusive)?;
                shared.set_xact(XactState::Started);
                self.load_states();
                self.txn = Some(RemoteTxn { final_lsn: 0, xid, skip: false, skipping: false });
                self.sess.apply_begin();
            }
            self.nchanges = 0;
            return Ok(());
        }
        if first {
            self.pa_allocate(xid);
        }
        match self.pool.action(xid) {
            Action::Serialize => {
                if first {
                    let dir = self.sess.state.db.store.spill_dir();
                    self.spools.insert(xid, Spool::new(&dir)?);
                } else if !self.spools.contains_key(&xid) {
                    return Err(format!("streamed transaction {xid} was not started"));
                }
            }
            Action::Send(id) => {
                if self.pa_send(id, m)? {
                    if !first {
                        self.apply_unlock(xid, LOCK_STREAM, LockMode::AccessExclusive);
                    }
                    self.pool.get(id).shared.pending.fetch_add(1, Ordering::SeqCst);
                } else {
                    self.pa_switch_to_partial(id, !first)?;
                    self.pa_write(id, m)?;
                }
                self.pool.stream = Some(id);
            }
            Action::Partial(id) => {
                self.pa_write(id, m)?;
                self.pool.stream = Some(id);
            }
        }
        Ok(())
    }

    /// apply_handle_stream_stop.
    pub(super) fn stream_stop(&mut self, m: &[u8]) -> Result<(), String> {
        let Some(xid) = self.stream_block.take() else {
            return Err("STREAM STOP message without STREAM START".into());
        };
        if self.pa.is_some() {
            self.log(Severity::Debug, &format!("applied {} changes in the streaming chunk", self.nchanges));
            return self.pa_decr_and_wait();
        }
        match self.pool.action(xid) {
            Action::Serialize => {}
            Action::Send(id) => {
                self.apply_lock(xid, LOCK_STREAM, LockMode::AccessExclusive)?;
                if !self.pa_send(id, m)? {
                    self.pa_switch_to_partial(id, true)?;
                    self.pa_write(id, m)?;
                }
            }
            Action::Partial(id) => self.pa_write(id, m)?,
        }
        self.pool.stream = None;
        Ok(())
    }

    /// A transaction's finish message (`m`) for its worker `id` (sent, or written to its
    /// file), then pa_xact_finish.
    fn pa_finish(&mut self, id: u64, m: &[u8], partial: bool) -> Result<(), String> {
        if !partial && !self.pa_send(id, m)? {
            self.pa_switch_to_partial(id, true)?;
            return self.pa_finish(id, m, true);
        }
        if partial {
            self.pa_write(id, m)?;
            self.pool.get(id).shared.set_fileset(FileSet::SerializeDone);
        }
        self.pa_xact_finish(id)
    }

    /// apply_handle_stream_abort (protocol 4 with parallel streaming adds the abort's
    /// position and time).
    pub(super) fn stream_abort(&mut self, m: &[u8]) -> Result<(), String> {
        if self.stream_block.is_some() {
            return Err("STREAM ABORT message without STREAM STOP".into());
        }
        let mut r = Reader { b: m, at: 1 };
        let xid = r.u32()?;
        let subxid = r.u32()?;
        let (abort_lsn, _abort_ts) = if m.len() >= 25 { (r.u64()?, r.u64()?) } else { (0, 0) };
        let top = xid == subxid;
        if self.pa.is_some() {
            self.pa_stream_abort(xid, subxid, abort_lsn)?;
            if !top {
                self.pa_decr_and_wait()?;
            }
            self.log(Severity::Debug, "finished processing the STREAM ABORT command");
            return Ok(());
        }
        match self.pool.action(xid) {
            Action::Serialize => {
                if top {
                    self.spools.remove(&xid);
                } else if let Some(sp) = self.spools.get_mut(&xid) {
                    sp.abort_sub(subxid)?;
                }
                self.log(Severity::Debug, "finished processing the STREAM ABORT command");
            }
            Action::Send(id) | Action::Partial(id) => {
                let partial = matches!(self.pool.action(xid), Action::Partial(_));
                if top {
                    let r = self.pa_finish(id, m, partial);
                    return r;
                }
                if partial {
                    return self.pa_write(id, m);
                }
                self.apply_unlock(xid, LOCK_STREAM, LockMode::AccessExclusive);
                self.pool.get(id).shared.pending.fetch_add(1, Ordering::SeqCst);
                self.apply_lock(xid, LOCK_STREAM, LockMode::AccessExclusive)?;
                if !self.pa_send(id, m)? {
                    self.pa_switch_to_partial(id, true)?;
                    self.pa_write(id, m)?;
                }
            }
        }
        Ok(())
    }

    /// pa_stream_abort: the whole transaction rolled back (the abort's position stored as
    /// the origin's, as RecordTransactionAbort does), or a subtransaction to its savepoint.
    fn pa_stream_abort(&mut self, xid: u32, subxid: u32, abort_lsn: u64) -> Result<(), String> {
        if xid == subxid {
            if self.sess.status() != crate::session::TxnStatus::Idle {
                let _ = self.sess.query("ROLLBACK");
            }
            if abort_lsn != 0 {
                self.repl.origins.advance(self.origin, abort_lsn, 0, false);
            }
            self.txn = None;
            self.subxacts.clear();
            self.pa.as_ref().expect("parallel apply worker").set_xact(XactState::Finished);
            self.apply_unlock(xid, LOCK_XACT, LockMode::AccessExclusive);
            return Ok(());
        }
        if let Some(i) = self.subxacts.iter().position(|x| *x == subxid) {
            let sp = savepoint_name(self.sub.oid, subxid);
            self.log(Severity::Debug, &format!("rolling back to savepoint {sp} in logical replication parallel apply worker"));
            self.query(&format!("ROLLBACK TO SAVEPOINT {sp}"))?;
            self.subxacts.truncate(i);
        }
        Ok(())
    }

    /// apply_handle_stream_commit.
    pub(super) fn stream_commit_msg(&mut self, m: &[u8]) -> Result<(), String> {
        if self.stream_block.is_some() {
            return Err("STREAM COMMIT message without STREAM STOP".into());
        }
        let mut r = Reader { b: m, at: 1 };
        let xid = r.u32()?;
        let _flags = r.u8()?;
        let commit_lsn = r.u64()?;
        let end_lsn = r.u64()?;
        let ts = r.u64()?;
        if self.pa.is_some() {
            // apply_handle_commit_internal, as the worker's transaction.
            self.repl.origins.xact_setup(self.pid, end_lsn, ts as i64).map_err(|e| e.message)?;
            let res = self.sess.apply_commit().map_err(error_text);
            self.repl.origins.xact_reset(self.pid);
            self.txn = None;
            res?;
            self.pa.as_ref().expect("parallel apply worker").set_xact(XactState::Finished);
            self.apply_unlock(xid, LOCK_XACT, LockMode::AccessExclusive);
            self.subxacts.clear();
            self.log(Severity::Debug, "finished processing the STREAM COMMIT command");
            return Ok(());
        }
        match self.pool.action(xid) {
            Action::Serialize => {
                let spool = self.spools.remove(&xid).ok_or_else(|| format!("streamed transaction {xid} was not started"))?;
                self.stream_commit(xid, commit_lsn, end_lsn, ts, spool)?;
            }
            Action::Send(id) | Action::Partial(id) => {
                let partial = matches!(self.pool.action(xid), Action::Partial(_));
                let r = self.pa_finish(id, m, partial);
                r?;
            }
        }
        self.log(Severity::Debug, "finished processing the STREAM COMMIT command");
        Ok(())
    }

    /// apply_handle_stream_prepare.
    pub(super) fn stream_prepare_msg(&mut self, m: &[u8]) -> Result<(), String> {
        if self.stream_block.is_some() {
            return Err("STREAM PREPARE message without STREAM STOP".into());
        }
        let mut r = Reader { b: m, at: 1 };
        let _flags = r.u8()?;
        let prepare_lsn = r.u64()?;
        let end_lsn = r.u64()?;
        let _ts = r.u64()?;
        let xid = r.u32()?;
        let _gid = r.cstr()?;
        if self.pa.is_some() {
            if let Some(t) = &mut self.txn {
                t.final_lsn = prepare_lsn;
            }
            self.prepare(prepare_lsn, end_lsn)?;
            self.pa.as_ref().expect("parallel apply worker").set_xact(XactState::Finished);
            self.apply_unlock(xid, LOCK_XACT, LockMode::AccessExclusive);
            self.subxacts.clear();
            self.log(Severity::Debug, "finished processing the STREAM PREPARE command");
            return Ok(());
        }
        match self.pool.action(xid) {
            Action::Serialize => {
                let spool = self.spools.remove(&xid).ok_or_else(|| format!("streamed transaction {xid} was not started"))?;
                self.begin_prepare(prepare_lsn, xid)?;
                spool.replay(|m| self.apply(m))?;
                self.prepare(prepare_lsn, end_lsn)?;
            }
            Action::Send(id) | Action::Partial(id) => {
                let partial = matches!(self.pool.action(xid), Action::Partial(_));
                let r = self.pa_finish(id, m, partial);
                r?;
            }
        }
        self.log(Severity::Debug, "finished processing the STREAM PREPARE command");
        Ok(())
    }
}
