//! Autovacuum (autovacuum.c): a launcher that visits each database at most once per
//! autovacuum_naptime, and workers that VACUUM / ANALYZE the tables past their thresholds
//! (relation_needs_vacanalyze), as PostgreSQL's do.
//!
//! Driven by the modification counters, never by polling: a statistics flush that moves a
//! user table's n_mod_since_analyze, n_ins_since_vacuum or n_dead_tup records the table
//! (`pgstat::Shared::av_changed`) and tells the launcher once (`Hook::note`); the launcher
//! sleeps until the next naptime tick after that (ticks run from the engine's start, so a
//! change waits 0 to naptime for its visit, as under PostgreSQL's launcher), and with nothing
//! changed it doesn't wake at all. A visit looks only at the tables recorded since the last
//! one; a table under its thresholds drops out until it changes again. Work runs in a
//! session of its own (an "autovacuum worker" backend) as PostgreSQL's VACUUM / ANALYZE with
//! SKIP_LOCKED, so the statistics are a manual ANALYZE's; a lock request that conflicts with
//! the worker's cancels it (`LockManager::acquire`), as the deadlock checker does.

use crate::engine::Engine;
use crate::session::state::Cluster;
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// What a database's statistics hold to tell the launcher about changes.
#[derive(Clone, Debug)]
pub struct Hook {
    launcher: Weak<Launcher>,
    db: u32,
}

impl Hook {
    pub fn new(launcher: &Arc<Launcher>, db: u32) -> Hook {
        Hook { launcher: Arc::downgrade(launcher), db }
    }

    /// Tables changed (or will have, once counts kept until `at` are flushed): true when a
    /// visit is now scheduled.
    pub fn note(&self, at: Instant) -> bool {
        self.launcher.upgrade().is_some_and(|l| l.note(self.db, at))
    }
}

/// The autovacuum settings (sighup parameters: the server's, not a session's).
#[derive(Clone, Debug)]
pub struct Params {
    /// autovacuum and track_counts.
    pub on: bool,
    pub naptime: Duration,
    pub max_workers: usize,
    pub vac_thresh: f64,
    pub vac_scale: f64,
    pub vac_max: f64,
    pub ins_thresh: f64,
    pub ins_scale: f64,
    pub anl_thresh: f64,
    pub anl_scale: f64,
}

impl Params {
    fn of(e: &Engine) -> Params {
        let mut s = crate::session::settings::Settings::new();
        s.adopt_file_layer(e.server_conf().loaded());
        for (name, value) in e.command_line() {
            s.set_command_line(&name, &value);
        }
        let real = |n: &str| s.get_str(n).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        Params {
            on: s.bool("autovacuum") && s.bool("track_counts"),
            naptime: Duration::from_secs(s.int("autovacuum_naptime").max(1) as u64),
            max_workers: s.int("autovacuum_max_workers").clamp(1, s.int("autovacuum_worker_slots").max(1)) as usize,
            vac_thresh: real("autovacuum_vacuum_threshold"),
            vac_scale: real("autovacuum_vacuum_scale_factor"),
            vac_max: real("autovacuum_vacuum_max_threshold"),
            ins_thresh: real("autovacuum_vacuum_insert_threshold"),
            ins_scale: real("autovacuum_vacuum_insert_scale_factor"),
            anl_thresh: real("autovacuum_analyze_threshold"),
            anl_scale: real("autovacuum_analyze_scale_factor"),
        }
    }
}

struct Running {
    /// Set to make the worker stop after its current table (with its session canceled).
    stop: Arc<AtomicBool>,
    cancel: Option<Arc<AtomicBool>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct State {
    /// Databases with changes: when the launcher visits each.
    due: HashMap<u32, Instant>,
    running: HashMap<u32, Running>,
    /// Databases being dropped or copied (until when): no worker starts there meanwhile.
    held: HashMap<u32, Instant>,
    params: Option<Params>,
    params_generation: u64,
    shutdown: bool,
}

pub struct Launcher {
    st: Mutex<State>,
    wake: Condvar,
    engine: Weak<Engine>,
    /// Naptime ticks count from here.
    anchor: Instant,
    /// How often the launcher thread woke up (tests: none while nothing changes).
    pub wakeups: AtomicU64,
    /// Tables processed (vacuumed or analyzed) by workers.
    pub processed: AtomicU64,
    /// CPU time the workers used, in nanoseconds (Linux: getrusage of each worker thread).
    pub cpu_ns: AtomicU64,
    /// The engine went without a shutdown (`dropped`).
    gone: AtomicBool,
}

impl Launcher {
    pub fn new(engine: Weak<Engine>) -> Launcher {
        Launcher {
            st: Mutex::new(State::default()),
            wake: Condvar::new(),
            engine,
            anchor: Instant::now(),
            wakeups: AtomicU64::new(0),
            processed: AtomicU64::new(0),
            cpu_ns: AtomicU64::new(0),
            gone: AtomicBool::new(false),
        }
    }

    /// The settings, read again after a configuration reload.
    fn params(&self, st: &mut State) -> Option<Params> {
        let e = self.engine.upgrade()?;
        let generation = e.server_conf().generation();
        if st.params.is_none() || st.params_generation != generation {
            st.params = Some(Params::of(&e));
            st.params_generation = generation;
        }
        st.params.clone()
    }

    /// The first naptime tick at or after `at`.
    fn tick(&self, at: Instant, naptime: Duration) -> Instant {
        let since = at.saturating_duration_since(self.anchor).as_nanos();
        let nap = naptime.as_nanos().max(1);
        let n = since.div_ceil(nap);
        self.anchor + Duration::from_nanos((n * nap).min(u64::MAX as u128) as u64)
    }

    fn note(&self, db: u32, at: Instant) -> bool {
        let mut st = self.st.lock();
        if st.shutdown {
            return false;
        }
        let Some(p) = self.params(&mut st).filter(|p| p.on) else { return false };
        let when = self.tick(at, p.naptime);
        let earlier = st.due.get(&db).is_none_or(|d| when < *d);
        if earlier {
            st.due.insert(db, when);
            self.wake.notify_all();
        }
        true
    }

    /// Start the launcher thread (AutoVacLauncherMain).
    pub fn start(self: &Arc<Self>) -> std::io::Result<()> {
        let me = self.clone();
        std::thread::Builder::new().name("roll-autovacuum".into()).spawn(move || me.main())?;
        Ok(())
    }

    fn main(self: Arc<Self>) {
        let mut st = self.st.lock();
        loop {
            if st.shutdown {
                return;
            }
            let now = Instant::now();
            st.held.retain(|_, t| *t > now);
            let p = self.params(&mut st);
            if !p.as_ref().is_some_and(|p| p.on) {
                // (Turned off: what changed is visited once it is on and something changes.)
                st.due.clear();
            }
            let max = p.map_or(1, |p| p.max_workers);
            let mut ready: Vec<(Instant, u32)> = st
                .due
                .iter()
                .filter(|(db, _)| !st.running.contains_key(*db))
                .map(|(db, t)| (*t, *db))
                .collect();
            ready.sort_unstable();
            let mut next: Option<Instant> = st.held.values().min().copied();
            for (t, db) in ready {
                if st.held.contains_key(&db) {
                    continue;
                }
                if t > now {
                    next = Some(next.map_or(t, |n| n.min(t)));
                } else if st.running.len() < max {
                    st.due.remove(&db);
                    self.spawn(&mut st, db);
                }
            }
            if self.gone.load(Ordering::SeqCst) {
                return;
            }
            match next {
                Some(t) => {
                    self.wake.wait_until(&mut st, t);
                }
                None => self.wake.wait(&mut st),
            }
            self.wakeups.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn spawn(self: &Arc<Self>, st: &mut State, db: u32) {
        let stop = Arc::new(AtomicBool::new(false));
        let (me, flag) = (self.clone(), stop.clone());
        let spawned = std::thread::Builder::new().name(format!("roll-autovacuum-{db}")).spawn(move || {
            lower_priority();
            if let Some(e) = me.engine.upgrade() {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| me.visit(&e, db, &flag)));
                if r.is_err() {
                    tracing::error!(database = db, "autovacuum worker panicked");
                }
            }
            // (The thread is the worker's own: its CPU time is the visit's.)
            let (user, sys) = thread_cpu();
            let ns = ((user + sys).max(0) as u64) * 1000;
            let total = me.cpu_ns.fetch_add(ns, Ordering::Relaxed) + ns;
            tracing::debug!(database = db, "autovacuum worker: cpu {} ms, all workers {} ms", ns / 1_000_000, total / 1_000_000);
            let mut st = me.st.lock();
            st.running.remove(&db);
            me.wake.notify_all();
        });
        match spawned {
            Ok(t) => {
                st.running.insert(db, Running { stop, cancel: None, thread: Some(t) });
            }
            Err(err) => tracing::warn!(error = %err, "could not start autovacuum worker"),
        }
    }

    /// Server shutdown: the workers stop (their work canceled) before the databases close.
    pub fn shutdown(&self) {
        let threads: Vec<std::thread::JoinHandle<()>> = {
            let mut st = self.st.lock();
            st.shutdown = true;
            st.due.clear();
            self.wake.notify_all();
            st.running
                .values_mut()
                .filter_map(|r| {
                    r.stop.store(true, Ordering::SeqCst);
                    if let Some(c) = &r.cancel {
                        c.store(true, Ordering::SeqCst);
                        crate::txn::locks::interrupt(c);
                    }
                    r.thread.take()
                })
                .collect()
        };
        for t in threads {
            if t.thread().id() != std::thread::current().id() {
                let _ = t.join();
            }
        }
    }

    /// The engine is dropped (maybe on one of these threads, by its last reference): the
    /// launcher ends without waiting for anything. (A worker holds the engine while it
    /// works, so none is working unless it is the one dropping it.)
    pub fn dropped(&self) {
        self.gone.store(true, Ordering::SeqCst);
        if let Some(_st) = self.st.try_lock_for(Duration::from_millis(100)) {
            self.wake.notify_all();
        }
    }

    /// DROP DATABASE and the like (CountOtherDBBackends): a worker in database `db` stops,
    /// and none starts there for a while.
    pub fn stop_database(&self, db: u32) {
        let mut st = self.st.lock();
        st.held.insert(db, Instant::now() + Duration::from_secs(5));
        if let Some(r) = st.running.get(&db) {
            r.stop.store(true, Ordering::SeqCst);
            if let Some(c) = &r.cancel {
                c.store(true, Ordering::SeqCst);
                crate::txn::locks::interrupt(c);
            }
        }
        self.wake.notify_all();
    }

    /// A worker's visit to database `db` (do_autovacuum): the tables changed since the last
    /// visit that are past their thresholds, each in a transaction of its own. Nothing to
    /// do costs no session.
    fn visit(&self, e: &Arc<Engine>, db: u32, stop: &AtomicBool) {
        let Some(p) = self.params(&mut self.st.lock()) else { return };
        let Some((dbname, database)) = e.autovacuum_database(db) else { return };
        let (changed, entries) = crate::exec::pgstat::take_autovacuum_changes(&database);
        let cat = database.catalog();
        let mut work: Vec<Work> = changed.iter().filter_map(|r| needs(&cat, *r, entries.get(r)?, &p)).collect();
        drop(cat);
        if work.is_empty() {
            return;
        }
        work.sort_by_key(|w| w.rel);
        let (pid, _, _) = e.register_backend(None, None);
        e.set_activity(pid, |a| a.backend_type = Some("autovacuum worker"));
        let mut sess = match e.connect(pid, &dbname, &e.bootstrap_superuser(), "") {
            Ok(s) => s,
            Err(err) => {
                e.unregister_backend(pid);
                tracing::warn!(database = %dbname, error = %err.message, "autovacuum worker could not connect");
                return;
            }
        };
        sess.state.autovacuum.set(true);
        // (A worker's session isn't counted in pg_stat_database.sessions.)
        sess.state.pgstat.borrow_mut().sessions = 0;
        {
            // AutoVacWorkerMain's settings: no timeouts, nothing logged per statement.
            let mut s = sess.state.settings.borrow_mut();
            for (n, v) in [
                ("statement_timeout", "0"),
                ("lock_timeout", "0"),
                ("idle_in_transaction_session_timeout", "0"),
                ("transaction_timeout", "0"),
                ("default_transaction_isolation", "read committed"),
                ("log_statement", "none"),
                ("log_duration", "off"),
                ("log_min_duration_statement", "-1"),
                ("log_min_duration_sample", "-1"),
            ] {
                s.set_internal(n, v);
            }
        }
        let proc = sess.state.lock_owner;
        database.locks.set_autovacuum(proc, Some(sess.cancel.clone()));
        self.set_cancel(db, &sess.cancel);
        e.set_activity(pid, |a| a.user.clear());
        for (i, w) in work.iter().enumerate() {
            if stop.load(Ordering::SeqCst) || e.is_stopping() {
                for w in &work[i..] {
                    crate::exec::pgstat::autovacuum_retry(&database, w.rel);
                }
                break;
            }
            let (shown, sql) = w.commands();
            let count =|db: &crate::txn::Database| {
                db.pgstat.lock().tables.get(&w.rel).map_or(0, |t| t.autovacuum_count + t.autoanalyze_count)
            };
            let before = count(&database);
            sess.cancel.store(false, Ordering::SeqCst);
            let now = crate::types::datetime::Clock::system().now;
            e.set_activity(pid, |a| {
                a.state = "active".into();
                a.query = shown;
                a.query_start = Some(now);
                a.xact_start = Some(now);
                a.state_change = Some(now);
            });
            let before_log = crate::log::active().then(|| Before::take(&database, w.rel));
            let r = sess.query(&sql);
            sess.notices.clear();
            match r {
                Ok(_) if count(&database) > before => {
                    self.processed.fetch_add(1, Ordering::Relaxed);
                    if let Some(b) = &before_log {
                        log_report(&sess.state.settings.borrow(), pid, &database, w, &dbname, b);
                    }
                }
                // (Skipped: its lock was taken. Looked at again on the next visit.)
                Ok(_) => crate::exec::pgstat::autovacuum_retry(&database, w.rel),
                Err(err) => {
                    let what = if w.vacuum { "vacuum" } else { "analyze" };
                    let msg = if err.code == crate::error::code::QUERY_CANCELED { "canceling autovacuum task" } else { err.message.as_str() };
                    tracing::info!(database = %dbname, "{msg} (automatic {what} of table \"{dbname}.{}.{}\")", w.schema, w.name);
                    log_failure(&sess.state.settings.borrow(), pid, &err, w, &dbname);
                    crate::exec::pgstat::autovacuum_retry(&database, w.rel);
                }
            }
        }
        database.locks.set_autovacuum(proc, None);
        drop(sess);
        e.unregister_backend(pid);
    }

    /// The worker's session can be canceled through `cancel` from now on.
    fn set_cancel(&self, db: u32, cancel: &Arc<AtomicBool>) {
        let mut st = self.st.lock();
        if let Some(r) = st.running.get_mut(&db) {
            r.cancel = Some(cancel.clone());
            if r.stop.load(Ordering::SeqCst) {
                cancel.store(true, Ordering::SeqCst);
                crate::txn::locks::interrupt(cancel);
            }
        }
    }
}

/// A table a worker vacuums or analyzes.
#[derive(Clone, Debug, PartialEq)]
pub struct Work {
    pub rel: u32,
    pub schema: String,
    pub name: String,
    pub vacuum: bool,
    pub analyze: bool,
}

impl Work {
    /// The command, as pg_stat_activity shows it ("autovacuum: VACUUM ANALYZE s.t") and as
    /// the worker runs it (vacuum()'s options of table_recheck_autovac: SKIP_LOCKED, the
    /// TOAST table being autovacuum's own entry).
    fn commands(&self) -> (String, String) {
        let what = match (self.vacuum, self.analyze) {
            (true, true) => "VACUUM ANALYZE",
            (true, false) => "VACUUM",
            _ => "ANALYZE",
        };
        let shown = format!("autovacuum: {what} {}.{}", self.schema, self.name);
        let target = format!(
            "{}.{}",
            crate::catalog::quote_ident(&self.schema),
            crate::catalog::quote_ident(&self.name)
        );
        let sql = if self.vacuum {
            let analyze = if self.analyze { ", ANALYZE" } else { "" };
            format!("VACUUM (SKIP_LOCKED, PROCESS_TOAST FALSE{analyze}) {target}")
        } else {
            format!("ANALYZE (SKIP_LOCKED) {target}")
        };
        (shown, sql)
    }
}

/// A storage parameter's value (the last one given).
fn reloption<'a>(options: &'a [String], name: &str) -> Option<&'a str> {
    options
        .iter()
        .rev()
        .find_map(|o| o.split_once('=').filter(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim()))
}

/// relation_needs_vacanalyze: whether table `rel` (by its statistics `e`) needs a VACUUM
/// and / or an ANALYZE now, under the settings and its storage parameters. (Anti-wraparound
/// vacuums aren't modelled: the engine's transaction ids don't wrap.)
pub fn needs(cat: &crate::catalog::Catalog, rel: u32, e: &crate::exec::pgstat::TabEntry, p: &Params) -> Option<Work> {
    let (t, schema, name) = match cat.tables.get(&rel) {
        Some(t) => (t.clone(), t.schema, t.name.clone()),
        None => {
            // A materialized view's storage.
            let v = cat.views.get(&rel)?;
            (v.storage.clone()?, v.schema, v.name.clone())
        }
    };
    // Only plain tables and materialized views; another session's temporary tables can't be.
    if t.temporary || t.partkey.is_some() || t.is_foreign() {
        return None;
    }
    let opt = |n: &str| reloption(&t.options, n);
    let num = |n: &str| opt(n).and_then(|v| v.parse::<f64>().ok());
    if opt("autovacuum_enabled").and_then(|v| crate::types::text::parse_bool(v).ok()) == Some(false) {
        return None;
    }
    let stats = cat.relstats.get(&rel);
    // (Never vacuumed: reltuples counts as zero.)
    let reltuples = stats.map_or(0.0, |s| s.tuples.max(0.0));
    let (relpages, relallfrozen) = stats.map_or((0, 0), |s| (s.pages, s.allfrozen));
    let mut unfrozen = 1.0;
    if relpages > 0 && relallfrozen > 0 {
        unfrozen = 1.0 - relallfrozen.min(relpages) as f64 / relpages as f64;
    }
    let vac_base = num("autovacuum_vacuum_threshold").unwrap_or(p.vac_thresh);
    let vac_scale = num("autovacuum_vacuum_scale_factor").unwrap_or(p.vac_scale);
    let vac_max = num("autovacuum_vacuum_max_threshold").unwrap_or(p.vac_max);
    let ins_base = num("autovacuum_vacuum_insert_threshold").unwrap_or(p.ins_thresh);
    let ins_scale = num("autovacuum_vacuum_insert_scale_factor").unwrap_or(p.ins_scale);
    let anl_base = num("autovacuum_analyze_threshold").unwrap_or(p.anl_thresh);
    let anl_scale = num("autovacuum_analyze_scale_factor").unwrap_or(p.anl_scale);
    // (float4 arithmetic, as PostgreSQL's.)
    let f = |x: f64| x as f32;
    let mut vacthresh = f(vac_base) + f(vac_scale) * f(reltuples);
    if vac_max >= 0.0 && vacthresh > f(vac_max) {
        vacthresh = f(vac_max);
    }
    let insthresh = f(ins_base) + f(ins_scale) * f(reltuples) * f(unfrozen);
    let anlthresh = f(anl_base) + f(anl_scale) * f(reltuples);
    let vacuum = (e.dead_tuples as f32) > vacthresh || (ins_base >= 0.0 && (e.ins_since_vacuum as f32) > insthresh);
    let analyze = (e.mod_since_analyze as f32) > anlthresh;
    if !(vacuum || analyze) {
        return None;
    }
    let schema = cat.schema_name(schema).to_string();
    // (RUST_LOG=pg_engine::autovacuum=debug: what each table's work came from.)
    tracing::debug!(
        "autovacuum: {schema}.{name}: vacuum {vacuum} (dead {} > {vacthresh}, inserted {} > {insthresh}), \
         analyze {analyze} (changed {} > {anlthresh}), reltuples {reltuples}",
        e.dead_tuples,
        e.ins_since_vacuum,
        e.mod_since_analyze,
    );
    Some(Work { rel, schema, name, vacuum, analyze })
}

/// Background work yields to sessions: the worker thread runs at a lower priority.
fn lower_priority() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn gettid() -> i32;
            fn setpriority(which: i32, who: u32, prio: i32) -> i32;
        }
        // SAFETY: plain system calls on the calling thread (PRIO_PROCESS with a thread id
        // sets that thread's nice value on Linux).
        unsafe {
            let tid = gettid();
            let _ = setpriority(0, tid as u32, 10);
        }
    }
}

/// The worker's server log lines: an autovacuum worker's (no client), under its session's
/// settings.
fn worker_log(st: &crate::session::settings::Settings, pid: i32, m: &crate::log::Message) {
    static LINES: std::sync::OnceLock<Arc<crate::log::Lines>> = std::sync::OnceLock::new();
    let f = crate::log::Format::from_settings(st);
    crate::log::write(&f, &crate::log::Who::aux(pid, "autovacuum worker", &LINES), m);
}

/// do_autovacuum's PG_CATCH: the error (a lock conflict's cancel: "canceling autovacuum
/// task") with "automatic vacuum / analyze of table" as its CONTEXT.
fn log_failure(st: &crate::session::settings::Settings, pid: i32, err: &crate::error::Error, w: &Work, dbname: &str) {
    if !crate::log::active() {
        return;
    }
    let what = if w.vacuum { "vacuum" } else { "analyze" };
    let mut e = if err.code == crate::error::code::QUERY_CANCELED {
        crate::error::Error::new(crate::error::code::QUERY_CANCELED, "canceling autovacuum task")
    } else {
        err.clone()
    };
    let ctx = format!("automatic {what} of table \"{dbname}.{}.{}\"", w.schema, w.name);
    e.context = Some(match &e.context {
        Some(c) => format!("{c}\n{ctx}"),
        None => ctx,
    }
    .into_boxed_str());
    worker_log(st, pid, &crate::log::Message::error(&e, None));
}

/// What a table looked like before the worker's command, for log_autovacuum_min_duration's
/// report.
pub(crate) struct Before {
    started: Instant,
    cpu: (i64, i64),
    dead: i64,
}

impl Before {
    fn take(database: &crate::txn::Database, rel: u32) -> Before {
        let dead = database.pgstat.lock().tables.get(&rel).map_or(0, |t| t.dead_tuples);
        Before { started: Instant::now(), cpu: thread_cpu(), dead }
    }
}

/// heap_vacuum_rel's and do_analyze_rel's LOG reports past log_autovacuum_min_duration (the
/// table's storage parameter, else the setting). The engine's own numbers where it has them
/// (pages, tuples, the visibility map, the removal horizon); buffer, WAL and I/O figures of
/// PostgreSQL's buffer manager it doesn't have are 0.
fn log_report(st: &crate::session::settings::Settings, pid: i32, database: &crate::txn::Database, w: &Work, dbname: &str, b: &Before) {
    if !crate::log::active() {
        return;
    }
    let cat = database.catalog();
    let min = cat
        .tables
        .get(&w.rel)
        .and_then(|t| reloption(&t.options, "log_autovacuum_min_duration").and_then(|v| v.parse::<i64>().ok()))
        .unwrap_or_else(|| st.int("log_autovacuum_min_duration"));
    let elapsed = b.started.elapsed();
    if min < 0 || (min > 0 && (elapsed.as_millis() as i64) < min) {
        return;
    }
    let (u1, s1) = thread_cpu();
    let cs = |us: i64| format!("{}.{:02}", us / 1_000_000, us % 1_000_000 / 10_000);
    let usage = format!(
        "system usage: CPU: user: {} s, system: {} s, elapsed: {} s",
        cs(u1 - b.cpu.0),
        cs(s1 - b.cpu.1),
        cs(elapsed.as_micros() as i64)
    );
    let io = "avg read rate: 0.000 MB/s, avg write rate: 0.000 MB/s\nbuffer usage: 0 hits, 0 reads, 0 dirtied\n\
              WAL usage: 0 records, 0 full page images, 0 bytes, 0 buffers full\n";
    let table = format!("\"{dbname}.{}.{}\"", w.schema, w.name);
    if w.vacuum {
        let rs = cat.rel_stats(w.rel, 'r');
        let live = database.pgstat.lock().tables.get(&w.rel).map_or(0, |t| t.live_tuples);
        let xids = database.xids();
        let (cutoff, now) = (xids.horizon(), xids.current_xmin());
        let pct = |n: i32| if rs.pages == 0 { 100.0 } else { 100.0 * n as f64 / rs.pages as f64 };
        let text = format!(
            "automatic vacuum of table {table}: index scans: 0\n\
             pages: 0 removed, {p} remain, {p} scanned ({:.2}% of total), 0 eagerly scanned\n\
             tuples: {} removed, {live} remain, 0 are dead but not yet removable\n\
             removable cutoff: {}, which was {} XIDs old when operation ended\n\
             frozen: 0 pages from table ({:.2}% of total) had 0 tuples frozen\n\
             visibility map: {} pages set all-visible, {} pages set all-frozen (0 were all-visible)\n\
             index scan not needed: 0 pages from table ({:.2}% of total) had 0 dead item identifiers removed\n\
             {io}{usage}",
            pct(rs.pages),
            b.dead,
            cutoff as u32,
            now.saturating_sub(cutoff),
            0.0,
            rs.allvisible,
            rs.allfrozen,
            0.0,
            p = rs.pages,
        );
        worker_log(st, pid, &crate::log::Message::log(&text));
    }
    if w.analyze {
        let text = format!("automatic analyze of table {table}\n{io}{usage}");
        worker_log(st, pid, &crate::log::Message::log(&text));
    }
}

/// This thread's user and system CPU time (microseconds; getrusage RUSAGE_THREAD).
pub(crate) fn thread_cpu() -> (i64, i64) {
    #[cfg(target_os = "linux")]
    {
        #[repr(C)]
        struct Rusage {
            utime: [i64; 2],
            stime: [i64; 2],
            rest: [i64; 14],
        }
        unsafe extern "C" {
            fn getrusage(who: i32, usage: *mut Rusage) -> i32;
        }
        let mut r = Rusage { utime: [0; 2], stime: [0; 2], rest: [0; 14] };
        // SAFETY: getrusage fills the struct, which has struct rusage's layout on Linux.
        if unsafe { getrusage(1, &mut r) } == 0 {
            return (r.utime[0] * 1_000_000 + r.utime[1], r.stime[0] * 1_000_000 + r.stime[1]);
        }
    }
    (0, 0)
}
