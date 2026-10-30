//! Autovacuum (`src/autovacuum.rs`): tables past autovacuum's thresholds are vacuumed and
//! analyzed in the background as PostgreSQL's autovacuum does them, storage parameters
//! respected, and nothing runs while nothing changes. (Naptime 1 s here.)

mod common;

use common::{engine_query, render, statements};
use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn engine(dir: &std::path::Path, settings: &[(&str, &str)]) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: settings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    })
    .unwrap()
}

/// The rows of each statement of `sql` (rendered `a | b`).
fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let mut out = String::new();
    for stmt in statements(sql) {
        out.push_str(&render(&stmt, &engine_query(s, &stmt)));
    }
    out.lines()
        .filter(|l| !l.starts_with("> ") && !l.starts_with("-- ") && !l.is_empty() && !l.contains(':'))
        .map(str::to_string)
        .collect()
}

fn raw(s: &mut Session, sql: &str) -> String {
    statements(sql).iter().map(|stmt| render(stmt, &engine_query(s, stmt))).collect()
}

/// Wait (up to `secs`) until `sql`'s rows are `want`; the rows last seen.
fn wait_for(s: &mut Session, sql: &str, want: &[&str], secs: u64) -> Vec<String> {
    let until = Instant::now() + Duration::from_secs(secs);
    loop {
        let got = rows(s, sql);
        if got == want || Instant::now() >= until {
            return got;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

const COUNTS: &str = "SELECT autovacuum_count, autoanalyze_count, vacuum_count, analyze_count, \
     last_autoanalyze IS NOT NULL, n_mod_since_analyze FROM pg_stat_user_tables WHERE relname = 'av_t'";

#[test]
fn loaded_tables_are_vacuumed_and_analyzed() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path(), &[("autovacuum_naptime", "1")]);
    let mut s = e.connect(1, "postgres", "postgres", "t").unwrap();
    // 1,500 inserts: past the insert threshold (1,000) and the analyze one (50).
    rows(&mut s, "CREATE TABLE av_t (a int, b text); INSERT INTO av_t SELECT i, 'x' || i % 7 FROM generate_series(1, 1500) i");
    rows(&mut s, "SELECT pg_stat_force_next_flush()");
    rows(&mut s, "SELECT 1");
    let got = wait_for(&mut s, COUNTS, &["1 | 1 | 0 | 0 | t | 0"], 20);
    assert_eq!(got, ["1 | 1 | 0 | 0 | t | 0"]);
    // The statistics are a manual ANALYZE's.
    let auto = rows(&mut s, "SELECT reltuples, attname, n_distinct FROM pg_class JOIN pg_stats ON tablename = relname WHERE relname = 'av_t' ORDER BY attname");
    assert_eq!(auto.len(), 2, "{auto:?}");
    assert!(auto[0].starts_with("1500 | a"), "{auto:?}");
    rows(&mut s, "ANALYZE av_t");
    assert_eq!(rows(&mut s, "SELECT reltuples, attname, n_distinct FROM pg_class JOIN pg_stats ON tablename = relname WHERE relname = 'av_t' ORDER BY attname"), auto);
    // 40 changes stay under 50 + 0.1 * 1500: nothing more.
    rows(&mut s, "UPDATE av_t SET b = 'y' WHERE a <= 40");
    rows(&mut s, "SELECT pg_stat_force_next_flush()");
    rows(&mut s, "SELECT 1");
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(rows(&mut s, COUNTS), ["1 | 1 | 0 | 1 | t | 40"]);
    drop(s);
    e.shutdown();
}

#[test]
fn storage_parameters_and_settings_are_respected() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path(), &[("autovacuum_naptime", "1")]);
    let mut s = e.connect(1, "postgres", "postgres", "t").unwrap();
    rows(
        &mut s,
        "CREATE TABLE av_off (a int) WITH (autovacuum_enabled = off);
         CREATE TABLE av_low (a int) WITH (autovacuum_analyze_threshold = 5, autovacuum_vacuum_insert_threshold = -1);
         CREATE TABLE av_few (a int);
         INSERT INTO av_off SELECT generate_series(1, 200);
         INSERT INTO av_low SELECT generate_series(1, 20);
         INSERT INTO av_few SELECT generate_series(1, 20);
         SELECT pg_stat_force_next_flush()",
    );
    rows(&mut s, "SELECT 1");
    let q = "SELECT relname, autoanalyze_count, autovacuum_count FROM pg_stat_user_tables WHERE relname LIKE 'av_%' ORDER BY relname";
    let want = ["av_few | 0 | 0", "av_low | 1 | 0", "av_off | 0 | 0"];
    assert_eq!(wait_for(&mut s, q, &want, 20), want);
    std::thread::sleep(Duration::from_millis(2000));
    assert_eq!(rows(&mut s, q), want);
    drop(s);
    e.shutdown();
    // Off: nothing runs.
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path(), &[("autovacuum_naptime", "1"), ("autovacuum", "off")]);
    let mut s = e.connect(1, "postgres", "postgres", "t").unwrap();
    rows(&mut s, "CREATE TABLE av_t (a int); INSERT INTO av_t SELECT generate_series(1, 200); SELECT pg_stat_force_next_flush()");
    rows(&mut s, "SELECT 1");
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(rows(&mut s, "SELECT autoanalyze_count FROM pg_stat_user_tables WHERE relname = 'av_t'"), ["0"]);
    drop(s);
    e.shutdown();
}

#[test]
fn nothing_changed_nothing_runs() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path(), &[("autovacuum_naptime", "1")]);
    let mut s = e.connect(1, "postgres", "postgres", "t").unwrap();
    rows(&mut s, "CREATE TABLE av_t (a int); INSERT INTO av_t SELECT generate_series(1, 100); SELECT pg_stat_force_next_flush()");
    rows(&mut s, "SELECT 1");
    wait_for(&mut s, "SELECT autoanalyze_count FROM pg_stat_user_tables WHERE relname = 'av_t'", &["1"], 20);
    std::thread::sleep(Duration::from_millis(1500));
    // Reads change no counter autovacuum looks at: the launcher sleeps through them.
    let woke = e.autovacuum().wakeups.load(Ordering::Relaxed);
    for _ in 0..30 {
        rows(&mut s, "SELECT count(*) FROM av_t");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(e.autovacuum().wakeups.load(Ordering::Relaxed), woke);
    assert_eq!(e.autovacuum().processed.load(Ordering::Relaxed), 1);
    drop(s);
    e.shutdown();
}

/// A running autovacuum shows in pg_stat_activity and pg_stat_progress_analyze, and a
/// conflicting lock request cancels it (its table is analyzed again on a later visit).
#[test]
fn a_conflicting_lock_cancels_the_worker() {
    let dir = tempfile::tempdir().unwrap();
    // (A cost delay of 100 ms per page: the ANALYZE takes seconds.)
    let e = engine(
        dir.path(),
        &[("autovacuum_naptime", "1"), ("autovacuum_vacuum_cost_delay", "100"), ("autovacuum_vacuum_cost_limit", "1")],
    );
    let mut s = e.connect(1, "postgres", "postgres", "t").unwrap();
    // VACUUM first: the pages are known and only the ANALYZE is due.
    rows(&mut s, "CREATE TABLE av_t (a int, b text) WITH (autovacuum_vacuum_insert_threshold = -1)");
    rows(&mut s, "INSERT INTO av_t SELECT i, repeat('x', 100) FROM generate_series(1, 2000) i");
    rows(&mut s, "VACUUM av_t");
    rows(&mut s, "SELECT pg_stat_force_next_flush()");
    rows(&mut s, "SELECT 1");
    let running = "SELECT a.backend_type, replace(a.query, ':', ''), p.phase, p.sample_blks_total > 1 FROM pg_stat_activity a \
         JOIN pg_stat_progress_analyze p USING (pid) WHERE p.relid = 'av_t'::regclass";
    let want = ["autovacuum worker | autovacuum ANALYZE public.av_t | acquiring sample rows | t"];
    let got = wait_for(&mut s, running, &want, 20);
    let state = raw(&mut s, "SELECT relpages, reltuples, autoanalyze_count, n_mod_since_analyze, (SELECT count(*) FROM pg_stat_activity) FROM pg_class JOIN pg_stat_user_tables ON relid = oid WHERE relname = 'av_t'");
    assert_eq!(got, want, "{state:?}");
    let t = Instant::now();
    rows(&mut s, "BEGIN; SET LOCAL lock_timeout = '10s'; LOCK TABLE av_t IN ACCESS EXCLUSIVE MODE");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    std::thread::sleep(Duration::from_millis(1500));
    rows(&mut s, "COMMIT");
    assert_eq!(rows(&mut s, "SELECT autoanalyze_count FROM pg_stat_user_tables WHERE relname = 'av_t'"), ["0"]);
    // Analyzed on a later visit.
    assert_eq!(wait_for(&mut s, "SELECT autoanalyze_count FROM pg_stat_user_tables WHERE relname = 'av_t'", &["1"], 60), ["1"]);
    assert_eq!(rows(&mut s, "SELECT count(*) FROM pg_stat_progress_analyze"), ["0"]);
    drop(s);
    e.shutdown();
}

/// An ANALYZE of a partitioned table that waits for a partition another transaction is
/// analyzing (an autovacuum worker, which commits first) keeps the parent's statistics it
/// made before the wait: they were dropped with the catalog copy it refreshed, and the
/// table's plans changed (partition_join's units came and went with autovacuum's timing).
#[test]
fn an_analyze_that_waits_keeps_what_it_did_before() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path(), &[("autovacuum", "off")]);
    let mut s = e.connect(1, "postgres", "postgres", "t").unwrap();
    rows(
        &mut s,
        "CREATE TABLE aw_p (a int, b int) PARTITION BY RANGE (a);
         CREATE TABLE aw_p1 PARTITION OF aw_p FOR VALUES FROM (0) TO (100);
         CREATE TABLE aw_p2 PARTITION OF aw_p FOR VALUES FROM (100) TO (200);
         INSERT INTO aw_p SELECT i, i % 7 FROM generate_series(0, 199) i",
    );
    // The other transaction holds aw_p1's ANALYZE lock until it commits.
    rows(&mut s, "BEGIN; ANALYZE aw_p1");
    let e2 = e.clone();
    let waiter = std::thread::spawn(move || {
        let mut s2 = e2.connect(2, "postgres", "postgres", "t").unwrap();
        rows(&mut s2, "ANALYZE aw_p");
    });
    std::thread::sleep(Duration::from_millis(1000));
    rows(&mut s, "COMMIT");
    waiter.join().unwrap();
    assert_eq!(
        rows(&mut s, "SELECT relname, reltuples FROM pg_class WHERE relname LIKE 'aw\\_p%' ORDER BY 1"),
        ["aw_p | 200", "aw_p1 | 100", "aw_p2 | 100"]
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT tablename, inherited, count(*) FROM pg_stats WHERE tablename LIKE 'aw\\_p%' GROUP BY 1, 2 ORDER BY 1, 2"
        ),
        ["aw_p | t | 2", "aw_p1 | f | 2", "aw_p2 | f | 2"]
    );
    drop(s);
    e.shutdown();
}
