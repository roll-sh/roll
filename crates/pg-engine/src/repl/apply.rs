//! Logical replication apply workers (launcher.c, worker.c, tablesync.c): a thread per
//! enabled subscription, which copies the tables not synchronized yet and applies the
//! publisher's pgoutput stream, one local transaction per remote one, as the subscription's
//! owner in its database.
//!
//! Progress is the subscription's replication origin (`pg_<subid>`): each applied
//! transaction commits the origin's position with its changes (`origin::before_commit`),
//! so a restart, even after a crash, resumes START_REPLICATION right after the last local
//! commit, and feedback only ever confirms what was committed: nothing is lost or applied
//! twice. Table synchronization (tablesync.c, simplified): the apply worker itself copies a
//! table in one local transaction under a temporary slot's snapshot (SNAPSHOT 'use') and
//! records the slot's consistent point as srsublsn with state 's'. The stream skips the
//! table's changes of transactions that committed before that point (PG's
//! should_apply_changes_for_rel), and the table is 'r' once the stream passed it. Tables a
//! refresh adds are copied between streamed transactions.

use super::walrcv::{self, Fail};
use super::apply_rows;
use crate::catalog::{Subscription, quote_ident};
use crate::engine::Engine;
use crate::error::{Error, code};
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
        match st.stats.iter_mut().find(|x| x.subid == s.subid && x.relid == s.relid) {
            Some(x) => *x = s.clone(),
            None => st.stats.push(s.clone()),
        }
    }

    fn clear_stat(&self, subid: u32, relid: Option<u32>) {
        self.st.lock().stats.retain(|x| !(x.subid == subid && x.relid == relid));
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
            Ok(Err(msg)) => tracing::error!(subscription = %sub.name, "{msg}"),
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
    subid: u32,
    pid: i32,
}

impl Drop for Backend<'_> {
    fn drop(&mut self) {
        if let Some(r) = self.e.repl_state() {
            r.origins.backend_exit(self.pid);
        }
        self.e.unregister_backend(self.pid);
        self.workers.clear_stat(self.subid, None);
    }
}

fn now() -> i64 {
    crate::types::datetime::Clock::system().now
}

/// One attempt: Ok when told to stop, Err (the message to log) on any failure.
fn run(e: &Arc<Engine>, workers: &Workers, sub: &Subscription, stop: &AtomicBool) -> Result<(), String> {
    let dbname = e
        .databases()
        .into_iter()
        .find(|d| d.0 == sub.dbid)
        .map(|d| d.1)
        .ok_or_else(|| format!("database {} of subscription \"{}\" does not exist", sub.dbid, sub.name))?;
    let (pid, _, _) = e.register_backend(None, None);
    let _backend = Backend { e, workers, subid: sub.oid, pid };
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
    repl.origins.session_setup(pid, &origin_name, 0).map_err(msg)?;
    // The progress the last commit stored (the origin file may be behind it after a crash).
    let db = sess.state.db.clone();
    if let Ok(Some(b)) = db.store.get(&super::origin::progress_key(origin), db.store.visible_ts())
        && let Ok(b) = <[u8; 8]>::try_from(&b[..])
    {
        repl.origins.advance(origin, u64::from_be_bytes(b), 0, false);
    }
    let env = crate::remote::conn::Env {
        outbound: e.outbound().cloned(),
        local: e.listen_addr(),
        local_port: u16::try_from(sess.state.settings.borrow().int("port")).unwrap_or(5432),
        default_user: e.bootstrap_superuser(),
    };
    tracing::info!("logical replication apply worker for subscription \"{}\" has started", sub.name);
    let stat = WorkerStat { subid: sub.oid, pid, worker_type: "apply", ..Default::default() };
    workers.set_stat(&stat);
    let mut w = Worker {
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
    };
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        w.load_states();
        // (pgstat_report_subscription_error: errors of the copy and of the apply loop.)
        let count = |w: &Worker, what: &str| {
            crate::sql::func::pubsub::count_subscription_stat(&w.sess.state.db.catalog(), w.sub.oid, what)
        };
        let r = match w.sync_tables() {
            Err(m) => {
                count(&w, "sync_error");
                Err(m)
            }
            Ok(()) => match w.stream() {
                Ok(true) => continue,
                Ok(false) => return Ok(()),
                Err(m) => {
                    if !m.contains("could not connect to the publisher") && !m.starts_with("password is required") {
                        count(&w, "apply_error");
                    }
                    Err(m)
                }
            },
        };
        // disable_on_error (as committed now: maybe_reread_subscription): the error, then the
        // subscription disabled (the worker ends).
        if let Err(m) = &r
            && w.sess.state.db.catalog().subscriptions.get(&sub.oid).is_some_and(|s| s.disable_on_err)
            && !stop.load(Ordering::SeqCst)
        {
            tracing::error!(subscription = %sub.name, "{m}");
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
    workers: &'a Workers,
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

    fn needs_sync(&self) -> bool {
        self.states.values().any(|(s, _)| matches!(s, 'i' | 'd' | 'f'))
    }

    /// UpdateSubscriptionRelState in the open local transaction.
    fn set_states(&mut self, changes: &[(u32, char, u64)]) -> Result<(), String> {
        let subid = self.sub.oid;
        let t = self.sess.block_txn().ok_or("no local transaction is open")?;
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
            tracing::warn!(
                "skip-LSN of subscription \"{}\" cleared\nDETAIL:  Remote transaction's finish WAL location (LSN) {} did not match skip-LSN {}.",
                self.sub.name,
                lsn(finish),
                lsn(self.skip_lsn())
            );
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
        tracing::info!("subscription \"{}\" has been disabled because of an error", self.sub.name);
        Ok(())
    }

    /// Copy every table not synchronized yet.
    fn sync_tables(&mut self) -> Result<(), String> {
        let mut todo: Vec<u32> = self.states.iter().filter(|(_, (s, _))| matches!(s, 'i' | 'd' | 'f')).map(|(r, _)| *r).collect();
        todo.sort_unstable();
        for relid in todo {
            if self.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            let r = self.sync_table(relid);
            self.workers.clear_stat(self.sub.oid, Some(relid));
            r?;
        }
        Ok(())
    }

    /// LogicalRepSyncTableStart: copy table `relid` as of a temporary slot's snapshot, in one
    /// local transaction that also records the slot's consistent point (state 's').
    fn sync_table(&mut self, relid: u32) -> Result<(), String> {
        let cat = self.sess.state.db.catalog();
        let Some(t) = cat.tables.get(&relid) else { return Ok(()) };
        let (nsp, rel) = (cat.schema_name(t.schema).to_string(), t.name.clone());
        let local_cols: Vec<String> = t.columns.iter().filter(|c| !c.dropped).map(|c| c.name.clone()).collect();
        drop(cat);
        let stat = WorkerStat {
            subid: self.sub.oid,
            relid: Some(relid),
            pid: self.pid,
            worker_type: "table synchronization",
            ..Default::default()
        };
        self.workers.set_stat(&stat);
        tracing::info!(
            "logical replication table synchronization worker for subscription \"{}\", table \"{rel}\" has started",
            self.sub.name
        );
        self.query("BEGIN")?;
        self.set_states(&[(relid, 'd', 0)])?;
        self.query("COMMIT")?;
        let stop = self.stop;
        let intr = move || stop_check(stop);
        let fail = |e: Fail| e.msg();
        let appname = format!("pg_{}_sync_{relid}", self.sub.oid);
        let mut c = walrcv::connect(&self.sub.conninfo, &appname, &self.env).map_err(|m| {
            format!("table synchronization worker for subscription \"{}\" could not connect to the publisher: {m}", self.sub.name)
        })?;
        self.password_check(&c)?;
        let sysid = walrcv::identify_system(&mut c, &intr).map_err(fail)?;
        walrcv::exec(&mut c, "BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ", &intr).map_err(fail)?;
        let slot = format!("pg_{}_sync_{relid}_{sysid}", self.sub.oid);
        let point = walrcv::create_slot(&mut c, &slot, true, false, false, "use", &intr).map_err(fail)?;
        let cols = walrcv::published_columns(&mut c, &nsp, &rel, &self.sub.publications, &intr).map_err(fail)?;
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
        let (kind, quals) = walrcv::table_filters(&mut c, &nsp, &rel, &self.sub.publications, &intr).map_err(fail)?;
        let source = if kind == 'r' && quals.is_empty() {
            format!("{} ({collist})", qualified(&nsp, &rel))
        } else {
            let only = if kind == 'r' { "ONLY " } else { "" };
            let filter = if quals.is_empty() { String::new() } else { format!(" WHERE {}", quals.join(" OR ")) };
            format!("(SELECT {collist} FROM {only}{}{filter})", qualified(&nsp, &rel))
        };
        let r = c.exec(&format!("COPY {source} TO STDOUT{with}"), &intr).map_err(|e| e.message)?;
        if r.status != Status::CopyOut {
            let m = r.error.map_or_else(|| c.error_text(), |e| e.text().trim_end_matches('\n').to_string());
            return Err(format!("could not start initial contents copy for table \"{nsp}.{rel}\": {m}"));
        }
        let into = format!("COPY {} ({collist}) FROM STDIN{with}", qualified(&nsp, &rel));
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
        self.set_states(&[(relid, 's', point)])?;
        self.query("COMMIT")?;
        self.use_origin(&format!("pg_{}", self.sub.oid))?;
        drop(c);
        tracing::info!(
            "logical replication table synchronization worker for subscription \"{}\", table \"{rel}\" has finished ({rows} rows at {})",
            self.sub.name,
            lsn(point)
        );
        Ok(())
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

    /// The origin's progress: the end of the last transaction applied.
    fn progress(&self) -> u64 {
        self.repl.origins.remote_lsn(self.origin)
    }

    /// LogicalRepApplyLoop: stream from the subscription's slot, from the origin's progress.
    /// Ok(true): tables to copy came up (between transactions); Ok(false): told to stop.
    fn stream(&mut self) -> Result<bool, String> {
        let stop = self.stop;
        let intr = move || stop_check(stop);
        let slot = self.sub.slot_name.clone().ok_or("subscription has no replication slot set")?;
        let mut c = walrcv::connect(&self.sub.conninfo, &self.sub.name, &self.env)
            .map_err(|m| format!("apply worker for subscription \"{}\" could not connect to the publisher: {m}", self.sub.name))?;
        self.password_check(&c)?;
        let start = self.progress();
        self.last_received = start;
        // (A streamed transaction in progress is streamed again from its start.)
        self.stream_block = None;
        self.spools.clear();
        let streaming = match self.sub.stream {
            'p' => Some("parallel"),
            't' => Some("on"),
            _ => None,
        };
        // two_phase = true stays pending until every table is ready; then it is enabled
        // as streaming starts (run_apply_worker).
        let all_ready = self.states.values().all(|(s, _)| *s == 'r');
        let two_phase = match self.sub.twophase {
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
        let mut sent = (0u64, 0u64);
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
            if self.txn.is_none() && last_check.elapsed() >= Duration::from_secs(1) {
                last_check = Instant::now();
                self.load_states();
                self.ready_tables(self.last_received, true)?;
                if self.needs_sync() {
                    self.feedback(&mut c, false)?;
                    let _ = c.copy_done();
                    return Ok(true);
                }
                if self.sub.twophase == 'p' && !two_phase && self.states.values().all(|(s, _)| *s == 'r') {
                    tracing::info!(
                        "logical replication apply worker for subscription \"{}\" will restart so that two_phase can be enabled",
                        self.sub.name
                    );
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

    /// (write, flush): what was received, and what is committed locally (all received when
    /// no remote transaction is open: the rest was applied or had nothing to apply).
    fn positions(&self) -> (u64, u64) {
        let flush = if self.txn.is_some() || !self.spools.is_empty() {
            self.progress()
        } else {
            self.last_received.max(self.progress())
        };
        (self.last_received, flush)
    }

    /// send_feedback: a Standby Status Update.
    fn feedback(&mut self, c: &mut Conn, request_reply: bool) -> Result<(), String> {
        let (write, flush) = self.positions();
        let mut b = Vec::with_capacity(34);
        b.push(b'r');
        b.extend_from_slice(&write.to_be_bytes());
        b.extend_from_slice(&flush.to_be_bytes());
        b.extend_from_slice(&flush.to_be_bytes());
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

    /// apply_dispatch: one pgoutput message.
    fn apply(&mut self, m: &[u8]) -> Result<(), String> {
        let mut r = Reader { b: m, at: 0 };
        let kind = r.u8()?;
        // In a stream block a change carries its (sub)transaction's xid: spooled without it.
        if let Some(top) = self.stream_block
            && matches!(kind, b'I' | b'U' | b'D' | b'T' | b'R' | b'Y')
        {
            let xid = r.u32()?;
            let mut msg = Vec::with_capacity(m.len() - 4);
            msg.push(kind);
            msg.extend_from_slice(&m[5..]);
            let spool = self.spools.get_mut(&top).ok_or("streamed change outside a streamed transaction")?;
            return spool.write(top, xid, &msg);
        }
        match kind {
            b'S' => {
                let xid = r.u32()?;
                let first = r.u8()? == 1;
                if self.stream_block.is_some() {
                    return Err("duplicate STREAM START message".into());
                }
                if first {
                    let dir = self.sess.state.db.store.spill_dir();
                    self.spools.insert(xid, super::apply_stream::Spool::new(&dir)?);
                } else if !self.spools.contains_key(&xid) {
                    return Err(format!("streamed transaction {xid} was not started"));
                }
                self.stream_block = Some(xid);
                Ok(())
            }
            b'E' => {
                if self.stream_block.take().is_none() {
                    return Err("STREAM STOP message without STREAM START".into());
                }
                Ok(())
            }
            b'A' => {
                let xid = r.u32()?;
                let subxid = r.u32()?;
                if xid == subxid {
                    self.spools.remove(&xid);
                } else if let Some(sp) = self.spools.get_mut(&xid) {
                    sp.abort_sub(subxid)?;
                }
                Ok(())
            }
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
            b'p' => {
                let _flags = r.u8()?;
                let prepare_lsn = r.u64()?;
                let end_lsn = r.u64()?;
                let _ts = r.u64()?;
                let xid = r.u32()?;
                let _gid = r.cstr()?;
                let spool = self.spools.remove(&xid).ok_or_else(|| format!("streamed transaction {xid} was not started"))?;
                self.begin_prepare(prepare_lsn, xid)?;
                spool.replay(|m| self.apply(m))?;
                self.prepare(prepare_lsn, end_lsn)
            }
            b'c' => {
                let xid = r.u32()?;
                let _flags = r.u8()?;
                let commit_lsn = r.u64()?;
                let end_lsn = r.u64()?;
                let ts = r.u64()?;
                let spool = self.spools.remove(&xid).ok_or_else(|| format!("streamed transaction {xid} was not started"))?;
                self.stream_commit(xid, commit_lsn, end_lsn, ts, spool)
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
                    tracing::info!("logical replication starts skipping transaction at LSN {}", lsn(final_lsn));
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
                self.ready_tables(end_lsn, false)?;
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
                    tracing::info!("logical replication completed skipping transaction at LSN {}", lsn(t.final_lsn));
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
            tracing::info!("logical replication starts skipping transaction at LSN {}", lsn(prepare_lsn));
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
            tracing::info!("logical replication completed skipping transaction at LSN {}", lsn(t.final_lsn));
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
        let apply = match self.states.get(&table.oid) {
            Some(('r', _)) => true,
            Some(('s', l)) => *l <= final_lsn,
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
        let (states, local) = (&self.states, &mut self.local);
        let r = self.sess.apply_step(|ctx| {
            if local.get(&id).is_none_or(|l| !l.current(ctx)) {
                local.insert(id, apply_rows::open(rel, ctx)?);
            }
            let lr = &local[&id];
            // should_apply_changes_for_rel.
            let apply = match states.get(&lr.relid) {
                Some(('r', _)) => true,
                Some(('s', l)) => *l <= final_lsn,
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
        if let Ok(apply_rows::Outcome::Applied) = r {
            return Ok(());
        }
        let rel = self.rels.get(&id).cloned().unwrap_or_else(|| RemoteRel { nsp: String::new(), name: String::new(), ident: 0, cols: Vec::new() });
        let context = format!(
            "processing remote data for replication origin \"pg_{}\" during message type \"{}\" for replication target relation \"{}.{}\" in transaction {xid}, finished at {}",
            self.sub.oid,
            ch.name(),
            rel.nsp,
            rel.name,
            lsn(final_lsn)
        );
        let cat = self.sess.state.db.catalog();
        match r {
            Ok(apply_rows::Outcome::Applied) => Ok(()),
            Ok(apply_rows::Outcome::Logged(kind, detail)) => {
                crate::sql::func::pubsub::count_subscription_stat(&cat, self.sub.oid, kind);
                tracing::info!(
                    "conflict detected on relation \"{}.{}\": conflict={kind}\nDETAIL:  {detail}\nCONTEXT:  {context}",
                    rel.nsp,
                    rel.name
                );
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
