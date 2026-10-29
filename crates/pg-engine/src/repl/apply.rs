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

/// A worker's life: attempts, each failure logged and retried after RETRY_INTERVAL.
fn worker_main(engine: Weak<Engine>, workers: Arc<Workers>, sub: Subscription, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        let Some(e) = engine.upgrade() else { return };
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&e, &workers, &sub, &stop)));
        drop(e);
        match r {
            Ok(Ok(())) => return,
            Ok(Err(msg)) => tracing::error!(subscription = %sub.name, "{msg}"),
            Err(_) => tracing::error!(subscription = %sub.name, "logical replication apply worker panicked"),
        }
        let until = Instant::now() + RETRY_INTERVAL;
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
        w.sync_tables()?;
        if !w.stream()? {
            return Ok(());
        }
    }
}

/// A remote relation (a Relation message).
#[derive(Clone, Debug)]
struct RemoteRel {
    nsp: String,
    name: String,
    /// relreplident: 'd' default, 'n' nothing, 'f' full, 'i' index.
    ident: u8,
    /// (name, part of the replica identity).
    cols: Vec<(String, bool)>,
}

/// A column of a tuple message.
#[derive(Clone, Debug)]
enum Col {
    Null,
    Unchanged,
    Text(String),
}

/// The remote transaction being applied.
struct RemoteTxn {
    final_lsn: u64,
    /// Applied before (it committed at or before the origin's progress).
    skip: bool,
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

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
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
    fn tuple(&mut self) -> Result<Vec<Col>, String> {
        let n = self.u16()?;
        let mut out = Vec::with_capacity(n as usize);
        for _ in 0..n {
            out.push(match self.u8()? {
                b'n' => Col::Null,
                b'u' => Col::Unchanged,
                b't' => {
                    let len = self.u32()? as usize;
                    Col::Text(String::from_utf8_lossy(self.take(len)?).into_owned())
                }
                k => return Err(format!("unrecognized data representation type '{}'", k as char)),
            });
        }
        Ok(out)
    }
}

fn lsn(l: u64) -> String {
    super::lsn::fmt(l)
}

fn qualified(nsp: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(nsp), quote_ident(name))
}

fn literal(c: &Col) -> String {
    match c {
        Col::Text(s) => walrcv::literal(s),
        _ => "NULL".into(),
    }
}

/// CHECK_FOR_INTERRUPTS of a worker: told to stop.
fn stop_check(stop: &AtomicBool) -> crate::Result<()> {
    if stop.load(Ordering::SeqCst) {
        return Err(Error::new(code::ADMIN_SHUTDOWN, "terminating logical replication worker due to administrator command"));
    }
    Ok(())
}

impl Worker<'_> {

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
        let r = c.exec(&format!("COPY {} ({collist}) TO STDOUT", qualified(&nsp, &rel)), &intr).map_err(|e| e.message)?;
        if r.status != Status::CopyOut {
            let m = r.error.map_or_else(|| c.error_text(), |e| e.text().trim_end_matches('\n').to_string());
            return Err(format!("could not start initial contents copy for table \"{nsp}.{rel}\": {m}"));
        }
        let into = format!("COPY {} ({collist}) FROM STDIN", qualified(&nsp, &rel));
        self.query("BEGIN")?;
        let r = self.copy_rows(&mut c, &into, &intr);
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
        drop(c);
        tracing::info!(
            "logical replication table synchronization worker for subscription \"{}\", table \"{rel}\" has finished ({rows} rows at {})",
            self.sub.name,
            lsn(point)
        );
        Ok(())
    }

    /// The COPY OUT's rows into the local table, a chunk of whole lines at a time; how many.
    fn copy_rows(&mut self, c: &mut Conn, into: &str, intr: &dyn Fn() -> crate::Result<()>) -> Result<u64, String> {
        let mut buf: Vec<u8> = Vec::new();
        let mut rows = 0u64;
        loop {
            intr().map_err(|e| e.message)?;
            match c.copy_read(Duration::from_secs(1))? {
                Some(Some(d)) => buf.extend_from_slice(&d),
                Some(None) => break,
                None => continue,
            }
            if buf.len() >= COPY_CHUNK
                && let Some(i) = buf.iter().rposition(|b| *b == b'\n')
            {
                let rest = buf.split_off(i + 1);
                rows += self.copy_in(into, &buf)?;
                buf = rest;
            }
        }
        if !buf.is_empty() {
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
        self.repl.origins.list().into_iter().find(|o| o.id == self.origin).map_or(0, |o| o.remote_lsn)
    }

    /// LogicalRepApplyLoop: stream from the subscription's slot, from the origin's progress.
    /// Ok(true): tables to copy came up (between transactions); Ok(false): told to stop.
    fn stream(&mut self) -> Result<bool, String> {
        let stop = self.stop;
        let intr = move || stop_check(stop);
        let slot = self.sub.slot_name.clone().ok_or("subscription has no replication slot set")?;
        let mut c = walrcv::connect(&self.sub.conninfo, &self.sub.name, &self.env)
            .map_err(|m| format!("apply worker for subscription \"{}\" could not connect to the publisher: {m}", self.sub.name))?;
        let start = self.progress();
        self.last_received = start;
        walrcv::start_streaming(&mut c, &slot, start, &self.sub.publications, &self.sub.origin, false, &intr).map_err(|e| e.msg())?;
        let mut last_recv = Instant::now();
        let mut last_check = Instant::now();
        let mut last_fb = Instant::now();
        let mut sent = (0u64, 0u64);
        let mut pinged = false;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                if self.txn.take().is_some() {
                    let _ = self.sess.query("ROLLBACK");
                }
                let _ = c.copy_done();
                return Ok(false);
            }
            let mut reply = false;
            match c.copy_read(Duration::from_millis(100))? {
                Some(Some(d)) => {
                    last_recv = Instant::now();
                    pinged = false;
                    reply = self.message(&d)?;
                }
                Some(None) => return Err("data stream from publisher has ended".into()),
                None => {}
            }
            if self.txn.is_none() && last_check.elapsed() >= Duration::from_secs(1) {
                last_check = Instant::now();
                self.load_states();
                self.ready_tables(self.last_received, true)?;
                if self.needs_sync() {
                    self.feedback(&mut c, false)?;
                    let _ = c.copy_done();
                    return Ok(true);
                }
            }
            let pos = self.positions();
            if reply || (pos != sent && pos.1 != 0) || last_fb.elapsed() >= STATUS_INTERVAL {
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
        let flush = if self.txn.is_some() { self.progress() } else { self.last_received.max(self.progress()) };
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
        Ok(())
    }

    /// apply_dispatch: one pgoutput message.
    fn apply(&mut self, m: &[u8]) -> Result<(), String> {
        let mut r = Reader { b: m, at: 0 };
        match r.u8()? {
            b'B' => {
                let final_lsn = r.u64()?;
                if self.txn.is_some() {
                    return Err("BEGIN message in a transaction".into());
                }
                let skip = final_lsn < self.progress();
                self.txn = Some(RemoteTxn { final_lsn, skip });
                if !skip {
                    self.query("BEGIN")?;
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
                self.repl.origins.xact_setup(self.pid, end_lsn, ts).map_err(|e| e.message)?;
                let r = self.query("COMMIT");
                self.repl.origins.xact_reset(self.pid);
                r?;
                Ok(())
            }
            b'R' => {
                let id = r.u32()?;
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
                let rel = self.remote_rel(r.u32()?)?;
                if r.u8()? != b'N' {
                    return Err("expected new tuple but got something else".into());
                }
                let new = r.tuple()?;
                let Some(target) = self.target(&rel)? else { return Ok(()) };
                let (names, vals): (Vec<String>, Vec<String>) = rel
                    .cols
                    .iter()
                    .zip(&new)
                    .filter(|(_, v)| !matches!(v, Col::Unchanged))
                    .map(|((n, _), v)| (quote_ident(n), literal(v)))
                    .unzip();
                self.query(&format!("INSERT INTO {target} ({}) VALUES ({})", names.join(", "), vals.join(", ")))?;
                Ok(())
            }
            b'U' => {
                let rel = self.remote_rel(r.u32()?)?;
                let mut old = None;
                let mut k = r.u8()?;
                if k == b'K' || k == b'O' {
                    old = Some((k, r.tuple()?));
                    k = r.u8()?;
                }
                if k != b'N' {
                    return Err("expected action 'N', got something else".into());
                }
                let new = r.tuple()?;
                let Some(target) = self.target(&rel)? else { return Ok(()) };
                let set: Vec<String> = rel
                    .cols
                    .iter()
                    .zip(&new)
                    .filter(|(_, v)| !matches!(v, Col::Unchanged))
                    .map(|((n, _), v)| format!("{} = {}", quote_ident(n), literal(v)))
                    .collect();
                if set.is_empty() {
                    return Ok(());
                }
                let cond = self.find_row(&rel, &target, old.as_ref(), &new)?;
                let r = self.query(&format!("UPDATE {target} SET {} WHERE {cond}", set.join(", ")))?;
                self.missing(&r, &target, "update")
            }
            b'D' => {
                let rel = self.remote_rel(r.u32()?)?;
                let k = r.u8()?;
                if k != b'K' && k != b'O' {
                    return Err("expected action 'O' or 'K'".into());
                }
                let old = r.tuple()?;
                let Some(target) = self.target(&rel)? else { return Ok(()) };
                let cond = self.find_row(&rel, &target, Some(&(k, old.clone())), &old)?;
                let r = self.query(&format!("DELETE FROM {target} WHERE {cond}"))?;
                self.missing(&r, &target, "delete")
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
        if t.skip {
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

    /// The condition finding the row an UPDATE / DELETE changes: by the replica identity's
    /// columns (the old key, or the new row's key when it didn't change), or by every column
    /// of the old row with REPLICA IDENTITY FULL (one such row, as PG takes the first found).
    fn find_row(&self, rel: &RemoteRel, target: &str, old: Option<&(u8, Vec<Col>)>, new: &[Col]) -> Result<String, String> {
        let (full, vals) = match old {
            Some((b'O', t)) => (true, t.as_slice()),
            Some((_, t)) => (false, t.as_slice()),
            None => (false, new),
        };
        let mut conds = Vec::new();
        for ((name, key), v) in rel.cols.iter().zip(vals) {
            if !full && !key {
                continue;
            }
            let c = quote_ident(name);
            match v {
                Col::Unchanged => {}
                Col::Null => conds.push(format!("{c} IS NULL")),
                v if full => conds.push(format!("{c} IS NOT DISTINCT FROM {}", literal(v))),
                v => conds.push(format!("{c} = {}", literal(v))),
            }
        }
        if conds.is_empty() {
            return Err(format!(
                "publisher did not send replica identity column expected by the logical replication target relation \"{}.{}\"",
                rel.nsp, rel.name
            ));
        }
        let cond = conds.join(" AND ");
        // (One row of those matching, as FindReplTupleInLocalRel takes the first; the
        // condition again outside, so a row id that moved can't take another row.)
        Ok(if full || rel.ident == b'n' {
            format!("ctid = (SELECT ctid FROM {target} WHERE {cond} LIMIT 1) AND {cond}")
        } else {
            cond
        })
    }

    /// A row an UPDATE / DELETE didn't find: logged as PG 18 logs the conflict.
    fn missing(&self, r: &[crate::session::QueryResult], target: &str, op: &str) -> Result<(), String> {
        let n: u64 = r.last().and_then(|r| r.tag.rsplit(' ').next()?.parse().ok()).unwrap_or(0);
        if n == 0 {
            tracing::info!(
                "conflict detected on relation {target}: conflict={op}_missing\nDETAIL:  Could not find the row to be {op}d."
            );
        }
        Ok(())
    }
}
