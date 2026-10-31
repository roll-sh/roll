//! Logical replication apply workers (launcher.c, worker.c, tablesync.c): a thread per
//! enabled subscription, which copies the tables not synchronized yet and applies the
//! publisher's pgoutput stream, one local transaction per remote one, as the subscription's
//! owner in its database.
//!
//! Progress is the subscription's replication origin (`pg_<subid>`): each applied
//! transaction commits the origin's position with its changes (`origin::before_commit`),
//! so a restart, even after a crash, resumes START_REPLICATION right after the last local
//! commit, and feedback only ever confirms what was committed: nothing is lost or applied
//! twice. Table synchronization (tablesync.c, `tablesync`): a worker per table copies it
//! under a permanent slot's snapshot and catches up on that slot while the leader keeps
//! applying; srsublsn with state 's' is where it stopped. The stream skips the table's
//! changes of transactions that committed before that point (PG's
//! should_apply_changes_for_rel), and the table is 'r' once the stream passed it.

use super::walrcv::{self, Fail};
use super::apply_rows;

mod parallel;
mod tablesync;
use crate::catalog::{Subscription, quote_ident};
use crate::engine::Engine;
use crate::error::{Error, Severity, code};
use crate::remote::conn::{Conn, Status};
use crate::session::Session;
use crate::session::state::Cluster;
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A worker as pg_stat_subscription shows it.
#[derive(Clone, Debug, Default)]
pub struct WorkerStat {
    pub subid: u32,
    pub relid: Option<u32>,
    pub pid: i32,
    pub worker_type: &'static str,
    pub received_lsn: u64,
    pub last_send: Option<i64>,
    pub last_receipt: Option<i64>,
    pub latest_end_lsn: u64,
    pub latest_end_time: Option<i64>,
    /// A parallel apply worker's leader.
    pub leader_pid: Option<i32>,
    /// Its place in LogicalRepCtx->workers (the first free one when it started), the order
    /// pg_stat_get_subscription lists workers in.
    pub slot: usize,
}

struct Running {
    sub: Subscription,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

#[derive(Default)]
struct State {
    running: HashMap<u32, Running>,
    /// Subscriptions being dropped (since when): not started again meanwhile.
    held: HashMap<u32, Instant>,
    stats: Vec<WorkerStat>,
    shutdown: bool,
    /// The cluster has subscriptions (the launcher looks every LAUNCHER_NAPTIME), or
    /// until when it looks that often after a wakeup (the DDL that woke it commits later).
    any: bool,
    recheck_until: Option<Instant>,
}

/// The cluster's apply workers and their launcher.
#[derive(Default)]
pub struct Workers {
    st: Mutex<State>,
    wake: Condvar,
    /// Table synchronization workers' states in memory, by (subscription, table):
    /// `tablesync`'s SYNCWAIT and CATCHUP.
    relstates: Mutex<HashMap<(u32, u32), (char, u64)>>,
    /// Running table synchronization workers' stop flags, by (subscription, table), each
    /// removed as its worker ends (`stop_sync`).
    sync_stops: Mutex<HashMap<(u32, u32), Arc<AtomicBool>>>,
}

/// How long the launcher waits between looks at the subscriptions.
const LAUNCHER_NAPTIME: Duration = Duration::from_millis(500);
/// How long it sleeps while the cluster has none (a CREATE SUBSCRIPTION wakes it): a server
/// without subscriptions does no periodic work for them.
const LAUNCHER_IDLE: Duration = Duration::from_secs(180);
/// How long after a wakeup it keeps looking every LAUNCHER_NAPTIME.
const WAKE_RECHECK: Duration = Duration::from_secs(10);
/// wal_retrieve_retry_interval: how long a failed worker waits before trying again.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// wal_receiver_status_interval, wal_receiver_timeout.
const STATUS_INTERVAL: Duration = Duration::from_secs(10);
const RECEIVER_TIMEOUT: Duration = Duration::from_secs(60);
/// How much copied data goes into one local COPY.
const COPY_CHUNK: usize = 8 << 20;

/// Whether a running worker serves `s` as it is now (maybe_reread_subscription's fields).
fn same_sub(a: &Subscription, b: &Subscription) -> bool {
    Subscription { skiplsn: 0, ..a.clone() } == Subscription { skiplsn: 0, ..b.clone() }
}

impl Workers {
    /// pg_stat_get_subscription's rows.
    pub fn stats(&self) -> Vec<WorkerStat> {
        self.st.lock().stats.clone()
    }

    fn set_stat(&self, s: &WorkerStat) {
        let mut st = self.st.lock();
        match st.stats.iter_mut().find(|x| x.pid == s.pid && x.relid == s.relid) {
            Some(x) => *x = WorkerStat { slot: x.slot, ..s.clone() },
            None => {
                let slot = (0..).find(|i| !st.stats.iter().any(|x| x.slot == *i)).unwrap_or(0);
                st.stats.push(WorkerStat { slot, ..s.clone() });
            }
        }
    }

    fn clear_stat(&self, pid: i32, relid: Option<u32>) {
        self.st.lock().stats.retain(|x| !(x.pid == pid && x.relid == relid));
    }

    /// The workers running (logical replication worker slots in use).
    fn slots_used(&self) -> usize {
        self.st.lock().stats.len()
    }

    /// A subscription changed: the launcher looks now.
    pub fn wake(&self) {
        self.st.lock().recheck_until = Some(Instant::now() + WAKE_RECHECK);
        self.wake.notify_all();
    }

    /// logicalrep_worker_stop: subscription `subid`'s worker stops, and this waits for it;
    /// `hold`: it isn't started again until `release` (or the subscription is gone).
    pub fn stop(&self, subid: u32, hold: bool) {
        let r = {
            let mut st = self.st.lock();
            if hold {
                st.held.insert(subid, Instant::now());
            }
            st.running.remove(&subid)
        };
        if let Some(r) = r {
            r.stop.store(true, Ordering::SeqCst);
            let _ = r.thread.join();
        }
    }

    pub fn release(&self, subid: u32) {
        self.st.lock().held.remove(&subid);
        self.wake();
    }

    /// Server shutdown: every worker stops (before the databases close).
    pub fn shutdown(&self) {
        let all: Vec<Running> = {
            let mut st = self.st.lock();
            st.shutdown = true;
            st.running.drain().map(|(_, r)| r).collect()
        };
        for r in &all {
            r.stop.store(true, Ordering::SeqCst);
        }
        for r in all {
            let _ = r.thread.join();
        }
        self.wake();
    }

    /// The launcher (ApplyLauncherMain): starts a worker for each enabled subscription and
    /// stops (or restarts) those whose subscription went or changed.
    pub fn start_launcher(self: &Arc<Self>, engine: &Arc<Engine>) -> std::io::Result<()> {
        // (Its first look comes soon: the subscriptions the server starts with.)
        self.wake();
        let weak = Arc::downgrade(engine);
        let me = self.clone();
        std::thread::Builder::new().name("roll-sub-launcher".into()).spawn(move || {
            loop {
                {
                    let mut st = me.st.lock();
                    if st.shutdown {
                        return;
                    }
                    let busy = st.any || st.recheck_until.is_some_and(|t| t > Instant::now());
                    me.wake.wait_for(&mut st, if busy { LAUNCHER_NAPTIME } else { LAUNCHER_IDLE });
                    if st.shutdown {
                        return;
                    }
                }
                let Some(e) = weak.upgrade() else { return };
                if e.is_stopping() {
                    return;
                }
                me.reconcile(&e);
            }
        })?;
        Ok(())
    }

    fn reconcile(self: &Arc<Self>, e: &Arc<Engine>) {
        let subs = e.subscriptions();
        let mut stop = Vec::new();
        let mut start = Vec::new();
        {
            let mut st = self.st.lock();
            if st.shutdown {
                return;
            }
            let now = Instant::now();
            st.any = !subs.is_empty();
            st.held.retain(|id, t| subs.iter().any(|s| s.oid == *id) && now.duration_since(*t) < Duration::from_secs(120));
            let want: Vec<&Subscription> = subs
                .iter()
                .filter(|s| s.enabled && s.slot_name.is_some() && !st.held.contains_key(&s.oid))
                .collect();
            let ids: Vec<u32> = st.running.keys().copied().collect();
            for id in ids {
                let r = &st.running[&id];
                let keep = !r.thread.is_finished() && want.iter().any(|s| s.oid == id && same_sub(s, &r.sub));
                if !keep {
                    stop.extend(st.running.remove(&id));
                }
            }
            for s in want {
                if !st.running.contains_key(&s.oid) && !stop.iter().any(|r| r.sub.oid == s.oid) {
                    start.push(s.clone());
                }
            }
        }
        for r in &stop {
            r.stop.store(true, Ordering::SeqCst);
        }
        for r in stop {
            let _ = r.thread.join();
        }
        for s in start {
            let flag = Arc::new(AtomicBool::new(false));
            let (weak, me, sub, f) = (Arc::downgrade(e), self.clone(), s.clone(), flag.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("roll-apply-{}", s.oid))
                .spawn(move || worker_main(weak, me, sub, f));
            match spawned {
                Ok(thread) => {
                    let mut st = self.st.lock();
                    if st.shutdown {
                        flag.store(true, Ordering::SeqCst);
                        drop(st);
                        let _ = thread.join();
                        return;
                    }
                    st.running.insert(s.oid, Running { sub: s, stop: flag, thread });
                }
                Err(err) => tracing::warn!(error = %err, "could not start logical replication apply worker"),
            }
        }
    }
}

/// A worker's life: attempts, each failure logged; the next starts RETRY_INTERVAL after the
/// last one started (the launcher's wal_retrieve_retry_interval between a worker's starts).
fn worker_main(engine: Weak<Engine>, workers: Arc<Workers>, sub: Subscription, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        let Some(e) = engine.upgrade() else { return };
        let started = Instant::now();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&e, &workers, &sub, &stop)));
        drop(e);
        match r {
            Ok(Ok(())) => return,
            Ok(Err(None)) => {}
            Ok(Err(Some(msg))) => tracing::error!(subscription = %sub.name, "{msg}"),
            Err(_) => tracing::error!(subscription = %sub.name, "logical replication apply worker panicked"),
        }
        let until = started + RETRY_INTERVAL;
        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// What a worker releases when an attempt ends.
struct Backend<'a> {
    e: &'a Arc<Engine>,
    workers: &'a Workers,
    pid: i32,
    relid: Option<u32>,
}

impl Drop for Backend<'_> {
    fn drop(&mut self) {
        if let Some(r) = self.e.repl_state() {
            r.origins.backend_exit(self.pid);
        }
        self.e.unregister_backend(self.pid);
        self.workers.clear_stat(self.pid, self.relid);
    }
}

fn now() -> i64 {
    crate::types::datetime::Clock::system().now
}

/// One attempt: Ok when told to stop, Err (the message to log, None when the worker logged
/// it) on any failure.
fn run(e: &Arc<Engine>, workers: &Arc<Workers>, sub: &Subscription, stop: &AtomicBool) -> Result<(), Option<String>> {
    let mut w = open_worker(e, workers, sub, stop, Role::Apply).map_err(Some)?;
    let r = run_apply(&mut w);
    if let Err(m) = &r {
        w.log(Severity::Error, m);
        w.exited();
    }
    r.map_err(|_| None)
}

/// What a worker is (LogicalRepWorkerType).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// The subscription's leader apply worker.
    Apply,
    /// A parallel apply worker of the leader with this pid (it shares the leader's origin).
    Parallel(i32),
    /// A table synchronization worker of this table (its origin is the table's own).
    Sync(u32),
}

/// TablesyncWorkerMain: copy table `relid` (true: it is synchronized).
fn sync_main(engine: Weak<Engine>, workers: Arc<Workers>, sub: Subscription, relid: u32, stop: Arc<AtomicBool>) -> bool {
    struct Ended<'a>(&'a Workers, u32, u32);
    impl Drop for Ended<'_> {
        fn drop(&mut self) {
            self.0.sync_stops.lock().remove(&(self.1, self.2));
        }
    }
    let _ended = Ended(&workers, sub.oid, relid);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(e) = engine.upgrade() else { return false };
        let mut w = match open_worker(&e, &workers, &sub, &stop, Role::Sync(relid)) {
            Ok(w) => w,
            Err(m) => {
                tracing::error!(subscription = %sub.name, "{m}");
                return false;
            }
        };
        w.load_states();
        let Err(m) = w.sync_table(relid) else { return true };
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        crate::sql::func::pubsub::count_subscription_stat(&w.sess.state.db.catalog(), sub.oid, "sync_error");
        w.log(Severity::Error, &m);
        w.exited();
        if w.sess.state.db.catalog().subscriptions.get(&sub.oid).is_some_and(|s| s.disable_on_err) {
            let _ = w.disable();
        }
        false
    }));
    r.unwrap_or(false)
}

/// A worker's backend, session and state (InitializeLogRepWorker).
fn open_worker<'a>(
    e: &'a Arc<Engine>,
    workers: &'a Arc<Workers>,
    sub: &'a Subscription,
    stop: &'a AtomicBool,
    role: Role,
) -> Result<Worker<'a>, String> {
    let leader = match role {
        Role::Parallel(l) => Some(l),
        _ => None,
    };
    let relid = match role {
        Role::Sync(r) => Some(r),
        _ => None,
    };
    let dbname = e
        .databases()
        .into_iter()
        .find(|d| d.0 == sub.dbid)
        .map(|d| d.1)
        .ok_or_else(|| format!("database {} of subscription \"{}\" does not exist", sub.dbid, sub.name))?;
    let (pid, _, _) = e.register_backend(None, None);
    let backend = Backend { e, workers, pid, relid };
    let msg = |err: Error| err.message;
    let mut sess = e.connect(pid, &dbname, &e.bootstrap_superuser(), &sub.name).map_err(msg)?;
    let owner = sess
        .state
        .db
        .catalog()
        .roles
        .get(&sub.owner)
        .map(|r| r.name.clone())
        .ok_or_else(|| format!("role {} of subscription \"{}\" does not exist", sub.owner, sub.name))?;
    sess.query(&format!(
        "SET session_replication_role = replica; SET search_path = ''; SET datestyle = ISO; \
         SET intervalstyle = postgres; SET extra_float_digits = 3; SET synchronous_commit = {}; \
         SET SESSION AUTHORIZATION {}",
        walrcv::literal(&sub.synccommit),
        quote_ident(&owner)
    ))
    .map_err(msg)?;
    let repl = sess.state.db.repl().ok_or("logical replication is not available")?;
    let origin_name = format!("pg_{}", sub.oid);
    let origin = match repl.origins.by_name(&origin_name) {
        Some(o) => o.id,
        None => repl.origins.create(&origin_name).map_err(msg)?,
    };
    if relid.is_none() {
        repl.origins.session_setup(pid, &origin_name, leader.unwrap_or(0)).map_err(msg)?;
    }
    let env = crate::remote::conn::Env {
        outbound: e.outbound().cloned(),
        local: e.listen_addr(),
        local_port: u16::try_from(sess.state.settings.borrow().int("port")).unwrap_or(5432),
        default_user: e.bootstrap_superuser(),
    };
    let worker_type = match role {
        Role::Apply => "apply",
        Role::Parallel(_) => "parallel apply",
        Role::Sync(_) => "table synchronization",
    };
    let stat = WorkerStat { subid: sub.oid, relid, pid, leader_pid: leader, worker_type, ..Default::default() };
    workers.set_stat(&stat);
    let w = Worker {
        e,
        workers,
        sub,
        stop,
        sess,
        pid,
        origin,
        repl,
        env,
        rels: HashMap::new(),
        local: HashMap::new(),
        stream_block: None,
        spools: HashMap::new(),
        states: HashMap::new(),
        txn: None,
        stat,
        last_received: 0,
        unflushed: Default::default(),
        flushed: 0,
        role,
        pa: None,
        pool: parallel::Pool::default(),
        subxacts: Vec::new(),
        nchanges: 0,
        apply_locks: Vec::new(),
        syncs: Default::default(),
        catchup: None,
        synced_at: None,
        errctx: None,
        lines: std::sync::OnceLock::new(),
        _backend: backend,
    };
    // The progress the last commit stored (the origin file may be behind it after a crash).
    if role == Role::Apply {
        w.load_progress();
    }
    // (A table synchronization worker says it started once it knows the table's name.)
    if relid.is_none() {
        w.log(Severity::Log, &format!("logical replication apply worker for subscription \"{}\" has started", sub.name));
    }
    Ok(w)
}

/// The leader's attempt (after open_worker): copy what needs copying, stream, apply.
fn run_apply(w: &mut Worker) -> Result<(), String> {
    let (sub, stop) = (w.sub, w.stop);
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        w.load_states();
        // (pgstat_report_subscription_error: errors of the copy and of the apply loop.)
        let count = |w: &Worker, what: &str| {
            crate::sql::func::pubsub::count_subscription_stat(&w.sess.state.db.catalog(), w.sub.oid, what)
        };
        let r = match w.stream() {
            Ok(true) => continue,
            Ok(false) => return Ok(()),
            Err(m) => {
                if !m.contains("could not connect to the publisher") && !m.starts_with("password is required") {
                    count(w, "apply_error");
                }
                Err(m)
            }
        };
        // disable_on_error (as committed now: maybe_reread_subscription): the error, then the
        // subscription disabled (the worker ends).
        if let Err(m) = &r
            && w.sess.state.db.catalog().subscriptions.get(&sub.oid).is_some_and(|s| s.disable_on_err)
            && !stop.load(Ordering::SeqCst)
        {
            w.log(Severity::Error, m);
            w.disable()?;
            return Ok(());
        }
        return r;
    }
}

/// A remote relation (a Relation message).
#[derive(Clone, Debug)]
pub(super) struct RemoteRel {
    pub nsp: String,
    pub name: String,
    /// relreplident: 'd' default, 'n' nothing, 'f' full, 'i' index.
    pub ident: u8,
    /// (name, part of the replica identity).
    pub cols: Vec<(String, bool)>,
}

/// A column of a tuple message (its bytes borrowed from the message).
#[derive(Clone, Copy, Debug)]
pub(super) enum Col<'a> {
    Null,
    Unchanged,
    Text(&'a [u8]),
    /// binary = true: the type's send format.
    Binary(&'a [u8]),
}

/// The remote transaction being applied.
struct RemoteTxn {
    final_lsn: u64,
    /// The publisher's transaction id (the apply error CONTEXT names it).
    xid: u32,
    /// Applied before (it committed at or before the origin's progress).
    skip: bool,
    /// ALTER SUBSCRIPTION ... SKIP names it: its changes are left out.
    skipping: bool,
}

struct Worker<'a> {
    e: &'a Arc<Engine>,
    workers: &'a Arc<Workers>,
    sub: &'a Subscription,
    stop: &'a AtomicBool,
    sess: Session,
    pid: i32,
    origin: u16,
    repl: Arc<super::Repl>,
    env: crate::remote::conn::Env,
    rels: HashMap<u32, RemoteRel>,
    /// Each remote relation's local table as bound for applying rows (logicalrep_rel_open).
    local: HashMap<u32, apply_rows::LocalRel>,
    /// The streamed transaction whose block (Stream Start .. Stream Stop) is being received.
    stream_block: Option<u32>,
    /// Streamed transactions in progress (streaming = on / parallel), by top-level xid.
    spools: HashMap<u32, super::apply_stream::Spool>,
    /// The subscription's tables (local oid): (srsubstate, srsublsn).
    states: HashMap<u32, (char, u64)>,
    txn: Option<RemoteTxn>,
    stat: WorkerStat,
    last_received: u64,
    /// lsn_mapping: applied transactions' remote ends with the local commit timestamp each
    /// must be durable at (an asynchronous commit, synchronous_commit = off, returns before
    /// its WAL fsync), oldest first; and the last remote end known durable.
    unflushed: std::collections::VecDeque<(u64, u64)>,
    flushed: u64,
    role: Role,
    /// A parallel apply worker's state shared with its leader (None in a leader).
    pa: Option<Arc<parallel::Shared>>,
    /// A leader's parallel apply workers.
    pool: parallel::Pool,
    /// A parallel apply worker's subtransactions with a savepoint (subxactlist).
    subxacts: Vec<u32>,
    /// Changes a parallel apply worker applied in the current stream block.
    nchanges: u64,
    /// The applytransaction locks the worker holds (session locks).
    apply_locks: Vec<(Vec<u8>, crate::txn::locks::LockMode)>,
    /// A leader's table synchronization workers.
    syncs: tablesync::Syncs,
    /// A table synchronization worker's catch-up: how far (the leader's position), and
    /// where it got to (SYNCDONE's position).
    catchup: Option<u64>,
    synced_at: Option<u64>,
    /// The CONTEXT of the message being applied (apply_error_callback).
    errctx: Option<String>,
    /// The worker's server log line counters.
    lines: std::sync::OnceLock<Arc<crate::log::Lines>>,
    /// (Last: released after everything else.)
    _backend: Backend<'a>,
}

impl Drop for Worker<'_> {
    fn drop(&mut self) {
        // logicalrep_worker_detach: a leader's parallel apply workers stop with it.
        self.pool.shutdown();
        let owner = self.sess.state.lock_owner;
        for (key, mode) in std::mem::take(&mut self.apply_locks) {
            self.sess.state.db.locks.release(&key, owner, mode);
        }
    }
}

/// A worker's log message as one string ("text\nDETAIL:  ...\nHINT:  ...\nCONTEXT:  ...",
/// CONTEXT lines possibly more than one) split into its fields.
fn split_message(m: &str) -> (&str, Option<&str>, Option<&str>, Option<String>) {
    let (mut text, mut detail, mut hint, mut context) = (m, None, None, Vec::new());
    let mut rest = m;
    let mut first = true;
    let markers = ["\nDETAIL:  ", "\nHINT:  ", "\nCONTEXT:  "];
    loop {
        let next = markers.iter().filter_map(|k| rest.find(k).map(|i| (i, *k))).min();
        let (part, after) = match next {
            Some((i, _)) => (&rest[..i], Some(i)),
            None => (rest, None),
        };
        if first {
            text = part;
            first = false;
        }
        let Some(i) = after else { break };
        let (_, k) = next.unwrap();
        let body = &rest[i + k.len()..];
        let end = markers.iter().filter_map(|k| body.find(k)).min().unwrap_or(body.len());
        match k {
            "\nDETAIL:  " => detail = Some(&body[..end]),
            "\nHINT:  " => hint = Some(&body[..end]),
            _ => context.push(&body[..end]),
        }
        rest = &body[end..];
    }
    (text, detail, hint, (!context.is_empty()).then(|| context.join("\n")))
}

/// A protocol message's reader.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let s = self.b.get(self.at..self.at + n).ok_or("invalid logical replication message: too short")?;
        self.at += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap_or_default()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap_or_default()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap_or_default()))
    }
    fn cstr(&mut self) -> Result<String, String> {
        let rest = self.b.get(self.at..).unwrap_or_default();
        let n = rest.iter().position(|c| *c == 0).ok_or("invalid logical replication message: unterminated string")?;
        self.at += n + 1;
        Ok(String::from_utf8_lossy(&rest[..n]).into_owned())
    }
    /// logicalrep_read_tuple.
    fn tuple(&mut self) -> Result<Vec<Col<'a>>, String> {
        let n = self.u16()?;
        let mut out = Vec::with_capacity(n as usize);
        for _ in 0..n {
            out.push(match self.u8()? {
                b'n' => Col::Null,
                b'u' => Col::Unchanged,
                b't' => {
                    let len = self.u32()? as usize;
                    Col::Text(self.take(len)?)
                }
                b'b' => {
                    let len = self.u32()? as usize;
                    Col::Binary(self.take(len)?)
                }
                k => return Err(format!("unrecognized data representation type '{}'", k as char)),
            });
        }
        Ok(out)
    }
}

/// The length of a binary COPY stream's header (signature, flags, extension area), once
/// `b` holds all of it.
fn copy_binary_header(b: &[u8]) -> Option<usize> {
    let ext = u32::from_be_bytes(b.get(15..19)?.try_into().ok()?) as usize;
    (b.len() >= 19 + ext).then_some(19 + ext)
}

/// Where the complete tuples of binary COPY data `b` (after its header) end, scanning from
/// `from` (a tuple boundary); the trailer is left out.
fn copy_binary_tuples(b: &[u8], from: usize) -> usize {
    let mut end = from;
    let mut at = from;
    loop {
        let Some(n) = b.get(at..at + 2) else { return end };
        let n = i16::from_be_bytes([n[0], n[1]]);
        if n < 0 {
            return end;
        }
        at += 2;
        for _ in 0..n {
            let Some(l) = b.get(at..at + 4) else { return end };
            let l = i32::from_be_bytes([l[0], l[1], l[2], l[3]]);
            at += 4 + l.max(0) as usize;
        }
        if at > b.len() {
            return end;
        }
        end = at;
    }
}

fn lsn(l: u64) -> String {
    super::lsn::fmt(l)
}

fn qualified(nsp: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(nsp), quote_ident(name))
}

/// CHECK_FOR_INTERRUPTS of a worker: told to stop.
fn stop_check(stop: &AtomicBool) -> crate::Result<()> {
    if stop.load(Ordering::SeqCst) {
        return Err(Error::new(code::ADMIN_SHUTDOWN, "terminating logical replication worker due to administrator command"));
    }
    Ok(())
}

impl Worker<'_> {
    /// A line of the worker's server log, as its background worker writes it (without a
    /// server log, the engine's own).
    fn log(&self, sev: Severity, m: &str) {
        if !crate::log::active() {
            match sev {
                Severity::Error | Severity::Fatal => tracing::error!(subscription = %self.sub.name, "{m}"),
                Severity::Warning => tracing::warn!("{m}"),
                Severity::Debug => tracing::debug!("{m}"),
                _ => tracing::info!("{m}"),
            }
            return;
        }
        let (text, detail, hint, context) = split_message(m);
        let context = context.or_else(|| self.errctx.clone());
        let f = crate::log::Format::from_settings(&self.sess.state.settings.borrow());
        let kind = self.kind();
        let code = match sev {
            Severity::Error | Severity::Fatal => "XX000",
            Severity::Warning => "01000",
            _ => "00000",
        };
        let msg = crate::log::Message { severity: Some(sev), code, text, detail, hint, context: context.as_deref(), ..Default::default() };
        crate::log::write(&f, &crate::log::Who::aux(self.pid, kind, &self.lines), &msg);
    }

    /// The worker's background worker type (bgw_type).
    fn kind(&self) -> &'static str {
        match self.role {
            Role::Apply => "logical replication apply worker",
            Role::Parallel(_) => "logical replication parallel worker",
            Role::Sync(_) => "logical replication tablesync worker",
        }
    }

    /// The postmaster's line when the worker ended with an error (it is started again).
    fn exited(&self) {
        let text = format!("background worker \"{}\" (PID {}) exited with exit code 1", self.kind(), self.pid);
        crate::log::postmaster(None, Severity::Log, &text);
    }

    /// The worker's session origin becomes origin `name` (made if missing).
    fn use_origin(&mut self, name: &str) -> Result<(), String> {
        let o = &self.repl.origins;
        let _ = o.session_reset(self.pid);
        if o.by_name(name).is_none() {
            o.create(name).map_err(|e| e.message.to_string())?;
        }
        o.session_setup(self.pid, name, 0).map_err(|e| e.message.to_string())
    }

    /// The connection's must_use_password check (password_required, an owner who isn't
    /// superuser, as the subscription is now).
    fn password_check(&self, c: &Conn) -> Result<(), String> {
        let cat = self.sess.state.db.catalog();
        let must = cat.subscriptions.get(&self.sub.oid).unwrap_or(self.sub).password_required && !cat.is_superuser(self.sub.owner);
        walrcv::password_check(c, must).map_err(|e| {
            let mut m = e.message.to_string();
            for (k, v) in [("DETAIL", e.detail.as_deref()), ("HINT", e.hint.as_deref())] {
                if let Some(v) = v {
                    m.push_str(&format!("\n{k}:  {v}"));
                }
            }
            m
        })
    }

    fn query(&mut self, sql: &str) -> Result<Vec<crate::session::QueryResult>, String> {
        match self.sess.query(sql) {
            Ok(r) => Ok(r),
            Err(err) => {
                if self.sess.status() != crate::session::TxnStatus::Idle {
                    let _ = self.sess.query("ROLLBACK");
                }
                Err(match &err.detail {
                    Some(d) => format!("{}\nDETAIL:  {d}", err.message),
                    None => err.message,
                })
            }
        }
    }

    /// The subscription's tables and states as committed (refreshes add and remove some).
    fn load_states(&mut self) {
        let cat = self.sess.state.db.catalog();
        self.states = cat
            .sub_rels
            .get(&self.sub.oid)
            .map(|v| v.iter().filter(|r| cat.tables.contains_key(&r.relid)).map(|r| (r.relid, (r.state, r.lsn))).collect())
            .unwrap_or_default();
    }

    /// UpdateSubscriptionRelState in the open local transaction. It holds the subscription
    /// (LockSharedObject AccessShareLock) until it ends, as ALTER / DROP SUBSCRIPTION's
    /// AccessExclusiveLock waits for: table synchronization workers each change their own
    /// table's row at once, and their commits merge (the rows are items of one catalog list).
    fn set_states(&mut self, changes: &[(u32, char, u64)]) -> Result<(), String> {
        let subid = self.sub.oid;
        let t = self.sess.block_txn().ok_or("no local transaction is open")?;
        let key = crate::txn::locks::object_key(0, crate::catalog::depend::SUBSCRIPTION, subid, 0);
        // (Waited for in steps, so that a worker told to stop by the DDL holding it does.)
        loop {
            let got = t
                .borrow_mut()
                .lock(&key, crate::txn::locks::LockMode::AccessShare, crate::txn::locks::WaitPolicy::SkipLocked, &|| format!("subscription {subid}"))
                .map_err(|e| e.message)?;
            if got {
                break;
            }
            if self.stop.load(Ordering::SeqCst) {
                return Err("terminating logical replication worker due to administrator command".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut c = (*t.borrow().catalog()).clone();
        let rels = c.sub_rels.entry(subid).or_default();
        for (relid, state, lsn) in changes {
            if let Some(r) = rels.iter_mut().find(|r| r.relid == *relid) {
                r.state = *state;
                r.lsn = *lsn;
            }
        }
        t.borrow_mut().set_catalog(c).map_err(|e| e.message)?;
        for (relid, state, lsn) in changes {
            self.states.insert(*relid, (*state, *lsn));
        }
        Ok(())
    }

    /// The subscription's subskiplsn as committed (0: none).
    fn skip_lsn(&self) -> u64 {
        self.sess.state.db.catalog().subscriptions.get(&self.sub.oid).map_or(0, |s| s.skiplsn)
    }

    /// Change the subscription's catalog row in the open local transaction.
    fn alter_sub(&mut self, f: impl FnOnce(&mut Subscription)) -> Result<(), String> {
        let t = self.sess.block_txn().ok_or("no local transaction is open")?;
        let mut c = (*t.borrow().catalog()).clone();
        if let Some(s) = c.subscriptions.get_mut(&self.sub.oid) {
            f(s);
        }
        t.borrow_mut().set_catalog(c).map_err(|e| e.message)
    }

    /// clear_subscription_skip_lsn: subskiplsn back to 0/0 with the skipped transaction's
    /// commit, unless ALTER SUBSCRIPTION ... SKIP changed it meanwhile.
    fn clear_skip_lsn(&mut self, finish: u64) -> Result<(), String> {
        if self.skip_lsn() != finish {
            self.log(Severity::Warning, &format!(
                "skip-LSN of subscription \"{}\" cleared\nDETAIL:  Remote transaction's finish WAL location (LSN) {} did not match skip-LSN {}.",
                self.sub.name,
                lsn(finish),
                lsn(self.skip_lsn())
            ));
            return Ok(());
        }
        self.alter_sub(|s| s.skiplsn = 0)
    }

    /// DisableSubscriptionAndExit: subenabled = false, in a transaction of its own.
    fn disable(&mut self) -> Result<(), String> {
        if self.sess.status() != crate::session::TxnStatus::Idle {
            let _ = self.sess.query("ROLLBACK");
        }
        self.txn = None;
        self.query("BEGIN")?;
        self.alter_sub(|s| s.enabled = false)?;
        self.query("COMMIT")?;
        self.log(Severity::Log, &format!("subscription \"{}\" has been disabled because of an error", self.sub.name));
        Ok(())
    }

    fn setting_int(&self, name: &str) -> i64 {
        self.sess.state.settings.borrow().int(name)
    }

    /// LogicalRepSyncTableStart's copy: table `relid` copied as of a permanent slot's
    /// snapshot (`retry`: a worker failed during an earlier copy, whose slot goes first), in
    /// one local transaction that records the end of the copy (state 'f') and the slot's
    /// consistent point as the synchronization origin's progress; that point.
    fn copy_table(&mut self, relid: u32, nsp: &str, rel: &str, local_cols: &[String], retry: bool) -> Result<u64, String> {
        let stop = self.stop;
        let intr = move || stop_check(stop);
        let fail = |e: Fail| e.msg();
        let slot = tablesync::slot_name(self.sub.oid, relid);
        let mut c = walrcv::connect(&self.sub.conninfo, &slot, &self.env).map_err(|m| {
            format!("table synchronization worker for subscription \"{}\" could not connect to the publisher: {m}", self.sub.name)
        })?;
        self.password_check(&c)?;
        if retry {
            self.drop_sync_slot(&mut c, &slot, true)?;
        }
        self.query("BEGIN")?;
        self.set_states(&[(relid, 'd', 0)])?;
        self.query("COMMIT")?;
        walrcv::identify_system(&mut c, &intr).map_err(fail)?;
        walrcv::exec(&mut c, "BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ", &intr).map_err(fail)?;
        let point = walrcv::create_slot(&mut c, &slot, false, false, false, "use", &intr).map_err(fail)?;
        let cols = walrcv::published_columns(&mut c, nsp, rel, &self.sub.publications, &intr).map_err(fail)?;
        let missing: Vec<String> = cols.iter().filter(|c| !local_cols.contains(c)).map(|c| format!("\"{c}\"")).collect();
        if !missing.is_empty() {
            return Err(format!(
                "logical replication target relation \"{nsp}.{rel}\" is missing replicated column{}: {}",
                if missing.len() == 1 { "" } else { "s" },
                missing.join(", ")
            ));
        }
        let collist = cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
        // copy_table: binary = true copies in binary from a publisher of 16 or later.
        let major = c.parameter("server_version").and_then(|v| v.split(['.', ' ']).next()?.parse::<u32>().ok());
        let binary = self.sub.binary && major.is_some_and(|v| v >= 16);
        let with = if binary { " WITH (FORMAT binary)" } else { "" };
        // A table with row filters (or not a plain table: a partitioned one published through
        // its root) is copied through a query: ONLY the table, its filters OR'ed.
        let (kind, quals) = walrcv::table_filters(&mut c, nsp, rel, &self.sub.publications, &intr).map_err(fail)?;
        let source = if kind == 'r' && quals.is_empty() {
            format!("{} ({collist})", qualified(nsp, rel))
        } else {
            let only = if kind == 'r' { "ONLY " } else { "" };
            let filter = if quals.is_empty() { String::new() } else { format!(" WHERE {}", quals.join(" OR ")) };
            format!("(SELECT {collist} FROM {only}{}{filter})", qualified(nsp, rel))
        };
        let r = c.exec(&format!("COPY {source} TO STDOUT{with}"), &intr).map_err(|e| e.message)?;
        if r.status != Status::CopyOut {
            let m = r.error.map_or_else(|| c.error_text(), |e| e.text().trim_end_matches('\n').to_string());
            return Err(format!("could not start initial contents copy for table \"{nsp}.{rel}\": {m}"));
        }
        let into = format!("COPY {} ({collist}) FROM STDIN{with}", qualified(nsp, rel));
        // (The copy commits under the table's synchronization origin, as a tablesync worker's
        // does: its rows' commit origin isn't the subscription's.)
        self.use_origin(&format!("pg_{}_{relid}", self.sub.oid))?;
        self.query("BEGIN")?;
        // (The copy runs as the table's owner unless run_as_owner is set.)
        let run_as_owner = self.sub.run_as_owner;
        let switched = match self.sess.apply_step(|ctx| apply_rows::sync_check(relid, run_as_owner, ctx)) {
            Ok(s) => s,
            Err(e) => {
                let _ = self.sess.query("ROLLBACK");
                return Err(e.message);
            }
        };
        let r = self.copy_rows(&mut c, &into, binary, &intr);
        if switched {
            let _ = self.sess.apply_step(|ctx| {
                apply_rows::switch_back(ctx);
                Ok(())
            });
        }
        if r.is_err() && self.sess.status() != crate::session::TxnStatus::Idle {
            let _ = self.sess.query("ROLLBACK");
        }
        let rows = r?;
        c.copy_finish(&intr)
            .map_err(|e| e.message)?
            .map_err(|m| format!("error while copying table \"{nsp}.{rel}\": {m}"))?;
        walrcv::exec(&mut c, "COMMIT", &intr).map_err(fail)?;
        // FINISHEDCOPY with the copy, the origin at the slot's consistent point (where the
        // catch-up, or a worker started again after a failure, streams from).
        self.set_states(&[(relid, 'f', 0)])?;
        self.repl.origins.xact_setup(self.pid, point, 0).map_err(|e| e.message)?;
        let r = self.query("COMMIT");
        self.repl.origins.xact_reset(self.pid);
        r?;
        tracing::debug!(relid, rows, "table copied at {}", lsn(point));
        Ok(point)
    }

    /// The COPY OUT's rows into the local table, a chunk of whole rows at a time (lines; in
    /// binary, tuples between the stream's header and a trailer); how many.
    fn copy_rows(&mut self, c: &mut Conn, into: &str, binary: bool, intr: &dyn Fn() -> crate::Result<()>) -> Result<u64, String> {
        let mut buf: Vec<u8> = Vec::new();
        let mut rows = 0u64;
        // Binary: the header, once read; where the complete tuples in `buf` end.
        let mut header: Option<Vec<u8>> = None;
        let mut whole = 0usize;
        loop {
            intr().map_err(|e| e.message)?;
            match c.copy_read(Duration::from_secs(1))? {
                Some(Some(d)) => buf.extend_from_slice(&d),
                Some(None) => break,
                None => continue,
            }
            if buf.len() < COPY_CHUNK {
                continue;
            }
            if !binary {
                if let Some(i) = buf.iter().rposition(|b| *b == b'\n') {
                    let rest = buf.split_off(i + 1);
                    rows += self.copy_in(into, &buf)?;
                    buf = rest;
                }
                continue;
            }
            if header.is_none() {
                let Some(n) = copy_binary_header(&buf) else { continue };
                header = Some(buf.drain(..n).collect());
                whole = 0;
            }
            whole = copy_binary_tuples(&buf, whole);
            if whole > 0 {
                let rest = buf.split_off(whole);
                let mut chunk = header.clone().unwrap_or_default();
                chunk.extend_from_slice(&buf);
                chunk.extend_from_slice(&(-1i16).to_be_bytes());
                rows += self.copy_in(into, &chunk)?;
                buf = rest;
                whole = 0;
            }
        }
        if !buf.is_empty() {
            if let Some(h) = header {
                let mut chunk = h;
                chunk.extend_from_slice(&buf);
                buf = chunk;
            }
            rows += self.copy_in(into, &buf)?;
        }
        Ok(rows)
    }

    fn copy_in(&mut self, into: &str, data: &[u8]) -> Result<u64, String> {
        let r = self.sess.query_copy(into, data).map_err(|e| e.message)?;
        Ok(r.last().and_then(|r| r.tag.rsplit(' ').next()?.parse().ok()).unwrap_or(0))
    }

    /// The origin's progress as its last commit stored it (the origin file may be behind it
    /// after a crash).
    fn load_progress(&self) {
        let db = &self.sess.state.db;
        if let Ok(Some(b)) = db.store.get(&super::origin::progress_key(self.origin), db.store.visible_ts())
            && let Ok(b) = <[u8; 8]>::try_from(&b[..])
        {
            self.repl.origins.advance(self.origin, u64::from_be_bytes(b), 0, false);
        }
    }

    /// The origin's progress: the end of the last transaction applied.
    fn progress(&self) -> u64 {
        self.repl.origins.remote_lsn(self.origin)
    }

    /// LogicalRepApplyLoop: stream from the subscription's slot, from the origin's progress.
    /// Ok(true): it should start again (two_phase can be enabled); Ok(false): told to stop, or
    /// a table synchronization worker's catch-up is done (`synced_at`).
    fn stream(&mut self) -> Result<bool, String> {
        let stop = self.stop;
        let intr = move || stop_check(stop);
        let sync = match self.role {
            Role::Sync(r) => Some(r),
            _ => None,
        };
        let (slot, appname) = match sync {
            Some(r) => (tablesync::slot_name(self.sub.oid, r), tablesync::slot_name(self.sub.oid, r)),
            None => (self.sub.slot_name.clone().ok_or("subscription has no replication slot set")?, self.sub.name.clone()),
        };
        let mut c = walrcv::connect(&self.sub.conninfo, &appname, &self.env).map_err(|m| match sync {
            Some(_) => format!("table synchronization worker for subscription \"{}\" could not connect to the publisher: {m}", self.sub.name),
            None => format!("apply worker for subscription \"{}\" could not connect to the publisher: {m}", self.sub.name),
        })?;
        self.password_check(&c)?;
        let start = self.progress();
        self.last_received = start;
        // (What a restart starts from was durable: the origin's stored progress.)
        self.unflushed.clear();
        self.flushed = start;
        // (A streamed transaction in progress is streamed again from its start.)
        self.stream_block = None;
        self.spools.clear();
        self.pool.shutdown();
        let streaming = match self.sub.stream {
            'p' => Some("parallel"),
            't' => Some("on"),
            _ => None,
        };
        // two_phase = true stays pending until every table is ready; then it is enabled
        // as streaming starts (run_apply_worker). A table synchronization worker's stream
        // has it off (set_stream_options).
        let all_ready = self.states.values().all(|(s, _)| *s == 'r');
        let two_phase = match self.sub.twophase {
            _ if sync.is_some() => false,
            'e' => true,
            'p' if all_ready => {
                self.query("BEGIN")?;
                self.alter_sub(|s| s.twophase = 'e')?;
                self.query("COMMIT")?;
                true
            }
            _ => false,
        };
        walrcv::start_streaming(&mut c, &slot, start, &self.sub.publications, &self.sub.origin, streaming, two_phase, self.sub.binary, &intr)
            .map_err(|e| e.msg())?;
        let mut last_recv = Instant::now();
        let mut last_check = Instant::now();
        let mut last_fb = Instant::now();
        let mut sent = (0u64, 0u64, 0u64);
        let mut pinged = false;
        let mut busy = false;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                if self.txn.take().is_some() {
                    let _ = self.sess.query("ROLLBACK");
                }
                let _ = c.copy_done();
                return Ok(false);
            }
            let mut reply = false;
            // Feedback goes when what was read is applied, not after every transaction of a
            // batch (LogicalRepApplyLoop sends it once the data runs out): right after a
            // message only what is buffered already is read, and running out of it is idle.
            let wait = if busy { Duration::ZERO } else { Duration::from_millis(100) };
            let mut idle = true;
            match c.copy_read(wait)? {
                Some(Some(d)) => {
                    last_recv = Instant::now();
                    pinged = false;
                    idle = false;
                    reply = self.message(&d)?;
                }
                Some(None) => return Err("data stream from publisher has ended".into()),
                None => {}
            }
            busy = !idle;
            // (ConfigReloadPending: a reload's settings.)
            self.sess.poll_config();
            self.pa_check()?;
            let between = self.txn.is_none() && self.spools.is_empty() && self.stream_block.is_none();
            if let Some(target) = self.catchup {
                // process_syncing_tables_for_sync: a table synchronization worker that reached
                // the leader's position (its last commit's end, or what it received when idle)
                // is done there.
                let at = if idle { self.last_received.max(self.progress()) } else { self.progress() };
                if between && at >= target {
                    self.synced_at = Some(at);
                    self.feedback(&mut c, false)?;
                    let _ = c.copy_done();
                    return Ok(false);
                }
            } else if sync.is_none() && between && idle {
                self.sync_for_apply(self.last_received.max(self.progress()))?;
            }
            if sync.is_none() && self.txn.is_none() && last_check.elapsed() >= Duration::from_secs(1) {
                last_check = Instant::now();
                self.load_states();
                if self.sub.twophase == 'p' && !two_phase && self.states.values().all(|(s, _)| *s == 'r') {
                    self.log(Severity::Log, &format!(
                        "logical replication apply worker for subscription \"{}\" will restart so that two_phase can be enabled",
                        self.sub.name
                    ));
                    self.feedback(&mut c, false)?;
                    let _ = c.copy_done();
                    return Ok(true);
                }
            }
            let pos = self.positions();
            if reply || (idle && pos != sent && pos.1 != 0) || last_fb.elapsed() >= STATUS_INTERVAL {
                self.feedback(&mut c, false)?;
                sent = pos;
                last_fb = Instant::now();
            }
            if last_recv.elapsed() >= RECEIVER_TIMEOUT {
                return Err("terminating logical replication worker due to timeout".into());
            }
            if !pinged && last_recv.elapsed() >= RECEIVER_TIMEOUT / 2 {
                self.feedback(&mut c, true)?;
                pinged = true;
            }
        }
    }

    /// (write, flush, apply) as send_feedback reports them: what was received; what is
    /// applied and durable locally (get_flush_position: all received when no applied
    /// transaction still waits for its WAL fsync and no remote transaction is open); and
    /// what is applied.
    fn positions(&mut self) -> (u64, u64, u64) {
        let applied = self.progress();
        let store = &self.sess.state.db.store;
        // (store_flush_position: a commit that moved the origin is at or before the last
        // timestamp handed out once it has returned.)
        if applied > self.unflushed.back().map_or(self.flushed, |e| e.0) {
            self.unflushed.push_back((applied, store.assigned_ts()));
        }
        let durable = store.durable_ts();
        while let Some(&(end, ts)) = self.unflushed.front()
            && ts <= durable
        {
            self.flushed = self.flushed.max(end);
            self.unflushed.pop_front();
        }
        let open = self.txn.is_some() || !self.spools.is_empty() || self.pool.busy();
        if open || !self.unflushed.is_empty() {
            (self.last_received, self.flushed, applied)
        } else {
            let all = self.last_received.max(applied);
            (self.last_received, all, all)
        }
    }

    /// send_feedback: a Standby Status Update.
    fn feedback(&mut self, c: &mut Conn, request_reply: bool) -> Result<(), String> {
        let (write, flush, apply) = self.positions();
        let mut b = Vec::with_capacity(34);
        b.push(b'r');
        b.extend_from_slice(&write.to_be_bytes());
        b.extend_from_slice(&flush.to_be_bytes());
        b.extend_from_slice(&apply.to_be_bytes());
        b.extend_from_slice(&now().to_be_bytes());
        b.push(request_reply as u8);
        c.copy_send(&b)
    }

    /// One CopyData message of the stream: XLogData or a keepalive. True: a reply is wanted.
    fn message(&mut self, d: &[u8]) -> Result<bool, String> {
        let mut r = Reader { b: d, at: 0 };
        let kind = r.u8()?;
        self.stat.last_receipt = Some(now());
        match kind {
            b'w' => {
                let start = r.u64()?;
                let end = r.u64()?;
                let sent = r.u64()? as i64;
                self.last_received = self.last_received.max(start).max(end);
                self.stat.received_lsn = self.last_received;
                self.stat.last_send = Some(sent);
                self.workers.set_stat(&self.stat);
                self.apply(&d[25..])?;
                Ok(false)
            }
            b'k' => {
                let end = r.u64()?;
                let sent = r.u64()? as i64;
                let reply = r.u8()? != 0;
                self.last_received = self.last_received.max(end);
                self.stat.received_lsn = self.last_received;
                self.stat.last_send = Some(sent);
                self.stat.latest_end_lsn = end;
                self.stat.latest_end_time = Some(sent);
                self.workers.set_stat(&self.stat);
                Ok(reply)
            }
            k => Err(format!("unrecognized streaming header: \"{}\"", k as char)),
        }
    }

    /// process_syncing_tables_for_apply: tables synchronized at or before `upto` are ready
    /// (`own`: in a local transaction of their own, else in the open one).
    fn ready_tables(&mut self, upto: u64, own: bool) -> Result<(), String> {
        let ready: Vec<(u32, char, u64)> = self
            .states
            .iter()
            .filter(|(_, (s, l))| *s == 's' && *l <= upto)
            .map(|(r, (_, l))| (*r, 'r', *l))
            .collect();
        if ready.is_empty() {
            return Ok(());
        }
        if own {
            self.query("BEGIN")?;
        }
        self.set_states(&ready)?;
        if own {
            self.query("COMMIT")?;
        }
        // (A ready table's synchronization origin goes.)
        for (relid, _, _) in &ready {
            let _ = self.repl.origins.drop_origin(&format!("pg_{}_{relid}", self.sub.oid), true);
        }
        Ok(())
    }

    /// apply_dispatch: one pgoutput message, its errors (and log lines) with
    /// apply_error_callback's CONTEXT.
    fn apply(&mut self, m: &[u8]) -> Result<(), String> {
        let ctx = self.message_context(m);
        let prev = std::mem::replace(&mut self.errctx, ctx.clone());
        let r = self.apply_message(m);
        self.errctx = prev;
        r.map_err(|e| match ctx {
            Some(c) if !e.contains("\nCONTEXT:  processing remote data") => format!("{e}\nCONTEXT:  {c}"),
            _ => e,
        })
    }

    fn apply_message(&mut self, m: &[u8]) -> Result<(), String> {
        let kind = *m.first().ok_or("invalid logical replication message: empty")?;
        // In a stream block a change carries its (sub)transaction's xid (handle_streamed_transaction).
        if let Some(top) = self.stream_block
            && matches!(kind, b'I' | b'U' | b'D' | b'T' | b'R' | b'Y')
        {
            if !self.streamed_change(top, kind, m)? {
                return Ok(());
            }
            let mut msg = Vec::with_capacity(m.len() - 4);
            msg.push(kind);
            msg.extend_from_slice(&m[5..]);
            return self.dispatch(&msg);
        }
        match kind {
            b'S' => self.stream_start(m),
            b'E' => self.stream_stop(m),
            b'A' => self.stream_abort(m),
            b'c' => self.stream_commit_msg(m),
            b'p' => self.stream_prepare_msg(m),
            _ => self.dispatch(m),
        }
    }

    /// apply_dispatch for messages outside stream blocks (and changes without their xid).
    fn dispatch(&mut self, m: &[u8]) -> Result<(), String> {
        let mut r = Reader { b: m, at: 0 };
        let kind = r.u8()?;
        match kind {
            b'b' => {
                let prepare_lsn = r.u64()?;
                let _end_lsn = r.u64()?;
                let _ts = r.u64()?;
                let xid = r.u32()?;
                let _gid = r.cstr()?;
                self.begin_prepare(prepare_lsn, xid)
            }
            b'P' => {
                let _flags = r.u8()?;
                let prepare_lsn = r.u64()?;
                let end_lsn = r.u64()?;
                let _ts = r.u64()?;
                let _xid = r.u32()?;
                let _gid = r.cstr()?;
                self.prepare(prepare_lsn, end_lsn)
            }
            b'K' => {
                let _flags = r.u8()?;
                let _commit_lsn = r.u64()?;
                let end_lsn = r.u64()?;
                let ts = r.u64()? as i64;
                let xid = r.u32()?;
                let _gid = r.cstr()?;
                self.finish_prepared(xid, end_lsn, ts, true)
            }
            b'r' => {
                let _flags = r.u8()?;
                let _prepare_end_lsn = r.u64()?;
                let end_lsn = r.u64()?;
                let _prepare_ts = r.u64()?;
                let ts = r.u64()? as i64;
                let xid = r.u32()?;
                let _gid = r.cstr()?;
                self.finish_prepared(xid, end_lsn, ts, false)
            }
            b'B' => {
                let final_lsn = r.u64()?;
                let _ts = r.u64()?;
                let xid = r.u32()?;
                if self.txn.is_some() {
                    return Err("BEGIN message in a transaction".into());
                }
                let skip = final_lsn < self.progress();
                // maybe_start_skipping_changes: ALTER SUBSCRIPTION ... SKIP named it.
                let skipping = !skip && final_lsn != 0 && self.skip_lsn() == final_lsn;
                if skipping {
                    self.log(Severity::Log, &format!("logical replication starts skipping transaction at LSN {}", lsn(final_lsn)));
                }
                self.txn = Some(RemoteTxn { final_lsn, xid, skip, skipping });
                if !skip {
                    self.sess.apply_begin();
                }
                Ok(())
            }
            b'C' => {
                let _flags = r.u8()?;
                let commit_lsn = r.u64()?;
                let end_lsn = r.u64()?;
                let ts = r.u64()? as i64;
                let Some(t) = self.txn.take() else {
                    return Err("COMMIT message without a transaction".into());
                };
                if commit_lsn != t.final_lsn {
                    return Err(format!("incorrect commit LSN {} in commit message (expected {})", lsn(commit_lsn), lsn(t.final_lsn)));
                }
                if t.skip {
                    return Ok(());
                }
                if self.role == Role::Apply {
                    self.ready_tables(end_lsn, false)?;
                }
                if t.skipping {
                    self.clear_skip_lsn(t.final_lsn)?;
                }
                self.repl.origins.xact_setup(self.pid, end_lsn, ts).map_err(|e| e.message)?;
                let r = self.sess.apply_commit().map_err(|e| match &e.detail {
                    Some(d) => format!("{}\nDETAIL:  {d}", e.message),
                    None => e.message.to_string(),
                });
                self.repl.origins.xact_reset(self.pid);
                r?;
                if t.skipping {
                    self.log(Severity::Log, &format!("logical replication completed skipping transaction at LSN {}", lsn(t.final_lsn)));
                }
                // (process_syncing_tables after the commit.)
                if self.role == Role::Apply {
                    self.sync_for_apply(end_lsn)?;
                }
                Ok(())
            }
            b'R' => {
                let id = r.u32()?;
                self.local.remove(&id);
                let nsp = r.cstr()?;
                let name = r.cstr()?;
                let ident = r.u8()?;
                let n = r.u16()?;
                let mut cols = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let flags = r.u8()?;
                    let col = r.cstr()?;
                    let _type = r.u32()?;
                    let _typmod = r.u32()?;
                    cols.push((col, flags & 1 != 0));
                }
                self.rels.insert(id, RemoteRel { nsp, name, ident, cols });
                Ok(())
            }
            // Type, origin and logical decoding messages: nothing to apply.
            b'Y' | b'O' | b'M' => Ok(()),
            b'I' => {
                let id = r.u32()?;
                if r.u8()? != b'N' {
                    return Err("expected new tuple but got something else".into());
                }
                let new = r.tuple()?;
                self.change(id, Change::Insert(new))
            }
            b'U' => {
                let id = r.u32()?;
                let mut old = None;
                let mut k = r.u8()?;
                if k == b'K' || k == b'O' {
                    old = Some(r.tuple()?);
                    k = r.u8()?;
                }
                if k != b'N' {
                    return Err("expected action 'N', got something else".into());
                }
                let new = r.tuple()?;
                self.change(id, Change::Update(old, new))
            }
            b'D' => {
                let id = r.u32()?;
                let k = r.u8()?;
                if k != b'K' && k != b'O' {
                    return Err("expected action 'O' or 'K'".into());
                }
                let old = r.tuple()?;
                self.change(id, Change::Delete(old))
            }
            b'T' => {
                let n = r.u32()?;
                let options = r.u8()?;
                let mut targets = Vec::new();
                for _ in 0..n {
                    let rel = self.remote_rel(r.u32()?)?;
                    if let Some(t) = self.target(&rel)? {
                        targets.push(t);
                    }
                }
                if !targets.is_empty() {
                    let restart = if options & 2 != 0 { " RESTART IDENTITY" } else { "" };
                    self.query(&format!("TRUNCATE {}{restart}", targets.join(", ")))?;
                }
                Ok(())
            }
            k => Err(format!("invalid logical replication message type \"??? ({k})\"")),
        }
    }

    /// TwoPhaseTransactionGid: the local name of remote prepared transaction `xid`.
    fn gid(&self, xid: u32) -> String {
        format!("pg_gid_{}_{xid}", self.sub.oid)
    }

    fn prepared_here(&self, gid: &str) -> bool {
        self.sess.state.db.twophase().list().iter().any(|i| i.gid == gid)
    }

    /// apply_handle_begin_prepare: like BEGIN; a transaction prepared here already (sent
    /// again after a restart) is left out.
    fn begin_prepare(&mut self, prepare_lsn: u64, xid: u32) -> Result<(), String> {
        if self.txn.is_some() {
            return Err("BEGIN PREPARE message in a transaction".into());
        }
        let skip = prepare_lsn < self.progress() || self.prepared_here(&self.gid(xid));
        let skipping = !skip && self.skip_lsn() == prepare_lsn;
        if skipping {
            self.log(Severity::Log, &format!("logical replication starts skipping transaction at LSN {}", lsn(prepare_lsn)));
        }
        self.txn = Some(RemoteTxn { final_lsn: prepare_lsn, xid, skip, skipping });
        if !skip {
            self.query("BEGIN")?;
        }
        Ok(())
    }

    /// apply_handle_prepare: the transaction prepared here as pg_gid_<subid>_<xid> (a
    /// skipped one empty, so its COMMIT PREPARED finds it); the origin moves past it.
    fn prepare(&mut self, prepare_lsn: u64, end_lsn: u64) -> Result<(), String> {
        let Some(t) = self.txn.take() else {
            return Err("PREPARE message without a transaction".into());
        };
        if prepare_lsn != t.final_lsn {
            return Err(format!("incorrect prepare LSN {} in prepare message (expected {})", lsn(prepare_lsn), lsn(t.final_lsn)));
        }
        if t.skip {
            return Ok(());
        }
        self.ready_tables(end_lsn, false)?;
        if t.skipping {
            self.clear_skip_lsn(t.final_lsn)?;
        }
        let gid = self.gid(t.xid);
        self.query(&format!("PREPARE TRANSACTION {}", walrcv::literal(&gid)))?;
        self.repl.origins.advance(self.origin, end_lsn, 0, false);
        if t.skipping {
            self.log(Severity::Log, &format!("logical replication completed skipping transaction at LSN {}", lsn(t.final_lsn)));
        }
        Ok(())
    }

    /// apply_handle_commit_prepared / _rollback_prepared: the local prepared transaction
    /// finished, the origin's position stored with it. One not prepared here (finished before
    /// a restart) has nothing left to do.
    fn finish_prepared(&mut self, xid: u32, end_lsn: u64, ts: i64, commit: bool) -> Result<(), String> {
        let gid = self.gid(xid);
        if !self.prepared_here(&gid) {
            if commit && end_lsn > self.progress() {
                return Err(format!("prepared transaction with identifier \"{gid}\" does not exist"));
            }
            return Ok(());
        }
        self.repl.origins.xact_setup(self.pid, end_lsn, ts).map_err(|e| e.message)?;
        let verb = if commit { "COMMIT" } else { "ROLLBACK" };
        let r = self.query(&format!("{verb} PREPARED {}", walrcv::literal(&gid)));
        self.repl.origins.xact_reset(self.pid);
        r.map(|_| ())
    }

    /// apply_handle_stream_commit: the spooled transaction applied as a remote transaction
    /// that came whole (its BEGIN, changes and COMMIT).
    fn stream_commit(&mut self, xid: u32, commit_lsn: u64, end_lsn: u64, ts: u64, spool: super::apply_stream::Spool) -> Result<(), String> {
        let mut b = vec![b'B'];
        b.extend_from_slice(&commit_lsn.to_be_bytes());
        b.extend_from_slice(&ts.to_be_bytes());
        b.extend_from_slice(&xid.to_be_bytes());
        self.apply(&b)?;
        spool.replay(|m| self.apply(m))?;
        let mut c = vec![b'C', 0];
        c.extend_from_slice(&commit_lsn.to_be_bytes());
        c.extend_from_slice(&end_lsn.to_be_bytes());
        c.extend_from_slice(&ts.to_be_bytes());
        self.apply(&c)
    }

    fn remote_rel(&self, id: u32) -> Result<RemoteRel, String> {
        self.rels
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("no relation map entry for remote relation ID {id}"))
    }

    /// logicalrep_rel_open and should_apply_changes_for_rel: the local table a change goes
    /// to (by schema and name, its columns by name), None when it's skipped.
    fn target(&mut self, rel: &RemoteRel) -> Result<Option<String>, String> {
        let Some(t) = &self.txn else {
            return Err("logical replication change outside a transaction".into());
        };
        if t.skip || t.skipping {
            return Ok(None);
        }
        let final_lsn = t.final_lsn;
        let cat = self.sess.state.db.catalog();
        let table = cat
            .schema_by_name(&rel.nsp)
            .and_then(|s| cat.relation_in(s.oid, &rel.name))
            .and_then(|r| match r {
                crate::catalog::Relation::Table(t) => Some(t),
                _ => None,
            })
            .ok_or_else(|| format!("logical replication target relation \"{}.{}\" does not exist", rel.nsp, rel.name))?;
        let missing: Vec<String> = rel
            .cols
            .iter()
            .filter(|(n, _)| !table.columns.iter().any(|c| !c.dropped && c.name == *n))
            .map(|(n, _)| format!("\"{n}\""))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "logical replication target relation \"{}.{}\" is missing replicated column{}: {}",
                rel.nsp,
                rel.name,
                if missing.len() == 1 { "" } else { "s" },
                missing.join(", ")
            ));
        }
        // should_apply_changes_for_rel: a table synchronization worker applies its table's.
        let apply = match (self.role, self.states.get(&table.oid)) {
            (Role::Sync(r), _) => r == table.oid,
            (_, Some(('r', _))) => true,
            (_, Some(('s', l))) => *l <= final_lsn,
            _ => false,
        };
        Ok(apply.then(|| qualified(&rel.nsp, &rel.name)))
    }

    /// An INSERT / UPDATE / DELETE message applied (apply_handle_*) as a step of the open
    /// transaction. A row an UPDATE or DELETE doesn't find is a conflict logged; an error
    /// rolls the transaction back and carries the CONTEXT naming the change.
    fn change(&mut self, id: u32, ch: Change) -> Result<(), String> {
        let Some(rel) = self.rels.get(&id) else {
            return Err(format!("no relation map entry for remote relation ID {id}"));
        };
        let Some(t) = &self.txn else {
            return Err("logical replication change outside a transaction".into());
        };
        if t.skip || t.skipping {
            return Ok(());
        }
        let (final_lsn, xid) = (t.final_lsn, t.xid);
        let origin = self.origin;
        let run_as_owner = self.sub.run_as_owner;
        let parallel = self.pa.is_some().then(|| self.sub.name.clone());
        let role = self.role;
        let (states, local) = (&self.states, &mut self.local);
        let r = self.sess.apply_step(|ctx| {
            if local.get(&id).is_none_or(|l| !l.current(ctx)) {
                local.insert(id, apply_rows::open(rel, ctx)?);
            }
            let lr = &local[&id];
            // should_apply_changes_for_rel (a parallel apply worker doesn't synchronize).
            if let Some(name) = &parallel
                && states.get(&lr.relid).is_some_and(|(s, _)| *s != 'r')
            {
                return Err(Error::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("logical replication parallel apply worker for subscription \"{name}\" will stop"),
                )
                .detail("Cannot handle streamed replication transactions using parallel apply workers until all tables have been synchronized."));
            }
            let apply = match (role, states.get(&lr.relid)) {
                (Role::Sync(r), _) => r == lr.relid,
                (_, Some(('r', _))) => true,
                (_, Some(('s', l))) => *l <= final_lsn,
                _ => false,
            };
            if !apply {
                return Ok(apply_rows::Outcome::Applied);
            }
            ctx.txn.borrow_mut().lock_relation(
                lr.relid,
                crate::txn::locks::LockMode::RowExclusive,
                crate::txn::locks::WaitPolicy::Block,
                &|| rel.name.clone(),
            )?;
            // (Any user code runs as the table's owner unless run_as_owner is set.)
            let switched = !run_as_owner && apply_rows::switch_to_owner(lr.owner(), ctx)?;
            let r = match &ch {
                Change::Insert(new) => apply_rows::insert(lr, rel, new, ctx).map(|_| apply_rows::Outcome::Applied),
                Change::Update(old, new) => apply_rows::update(lr, rel, old.as_deref(), new, origin, ctx),
                Change::Delete(old) => apply_rows::delete(lr, rel, old, origin, ctx),
            };
            if switched {
                apply_rows::switch_back(ctx);
            }
            r
        });
        // (A parallel apply worker's error the leader learns of while it applies a change,
        // as PostgreSQL's leader does in its waits: with the change's CONTEXT.)
        let r = match r {
            Ok(o) if self.pool.busy() => match self.pa_check() {
                Ok(()) => Ok(o),
                Err(m) => Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, m)),
            },
            r => r,
        };
        if let Ok(apply_rows::Outcome::Applied) = r {
            return Ok(());
        }
        let rel = self.rels.get(&id).cloned().unwrap_or_else(|| RemoteRel { nsp: String::new(), name: String::new(), ident: 0, cols: Vec::new() });
        // (A parallel apply worker doesn't know the finish position.)
        let at = if final_lsn == 0 { String::new() } else { format!(", finished at {}", lsn(final_lsn)) };
        let context = format!(
            "processing remote data for replication origin \"pg_{}\" during message type \"{}\" for replication target relation \"{}.{}\" in transaction {xid}{at}",
            self.sub.oid,
            ch.name(),
            rel.nsp,
            rel.name,
        );
        let cat = self.sess.state.db.catalog();
        match r {
            Ok(apply_rows::Outcome::Applied) => Ok(()),
            Ok(apply_rows::Outcome::Logged(kind, detail)) => {
                crate::sql::func::pubsub::count_subscription_stat(&cat, self.sub.oid, kind);
                self.log(Severity::Log, &format!(
                    "conflict detected on relation \"{}.{}\": conflict={kind}\nDETAIL:  {detail}\nCONTEXT:  {context}",
                    rel.nsp,
                    rel.name
                ));
                Ok(())
            }
            Err(err) => {
                if err.message.starts_with("conflict detected")
                    && let Some(kind) = err.message.split("conflict=").nth(1)
                {
                    crate::sql::func::pubsub::count_subscription_stat(&cat, self.sub.oid, kind);
                }
                drop(cat);
                if self.sess.status() != crate::session::TxnStatus::Idle {
                    let _ = self.sess.query("ROLLBACK");
                }
                self.txn = None;
                let mut m = err.message;
                if let Some(d) = &err.detail {
                    m.push_str(&format!("\nDETAIL:  {d}"));
                }
                if let Some(c) = &err.context {
                    m.push_str(&format!("\nCONTEXT:  {c}"));
                }
                m.push_str(&format!("\nCONTEXT:  {context}"));
                Err(m)
            }
        }
    }
}

/// An INSERT, UPDATE (old key or row, if sent; new row) or DELETE (old key or row) message.
enum Change<'a> {
    Insert(Vec<Col<'a>>),
    Update(Option<Vec<Col<'a>>>, Vec<Col<'a>>),
    Delete(Vec<Col<'a>>),
}

impl Change<'_> {
    fn name(&self) -> &'static str {
        match self {
            Change::Insert(_) => "INSERT",
            Change::Update(..) => "UPDATE",
            Change::Delete(_) => "DELETE",
        }
    }
}
