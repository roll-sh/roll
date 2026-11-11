//! Autovacuum's server log lines: log_autovacuum_min_duration's reports and a lock conflict's
//! "canceling autovacuum task" (its own test binary: the log sink is the process's).

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static LOG: Mutex<String> = Mutex::new(String::new());

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last().unwrap().rows.iter().map(|r| format!("{r:?}")).collect()
}

/// Wait until the log has `want` (or `secs` pass).
fn log_has(want: &str, secs: u64) -> String {
    let t = Instant::now();
    loop {
        let log = LOG.lock().unwrap().clone();
        if log.contains(want) || t.elapsed() > Duration::from_secs(secs) {
            return log;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn autovacuum_log_lines() {
    let dir = tempfile::tempdir().unwrap();
    let settings = [
        ("autovacuum_naptime", "1"),
        ("log_autovacuum_min_duration", "0"),
        ("autovacuum_vacuum_cost_delay", "100"),
        ("autovacuum_vacuum_cost_limit", "1"),
    ];
    let e = Engine::open(EngineOptions {
        data_dir: dir.path().to_path_buf(),
        sync: false,
        settings: settings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..EngineOptions::default()
    })
    .unwrap();
    pg_engine::log::set_sink(Some(Arc::new(|s: &str| LOG.lock().unwrap().push_str(s))));
    let mut s = e.connect(1, "postgres", "postgres", "t").unwrap();
    // A small table: vacuumed and analyzed, both reported.
    rows(&mut s, "CREATE TABLE av_small (a int) WITH (autovacuum_vacuum_cost_delay = 0)");
    rows(&mut s, "INSERT INTO av_small SELECT generate_series(1, 1500)");
    rows(&mut s, "SELECT pg_stat_force_next_flush()");
    let log = log_has("automatic analyze of table \"postgres.public.av_small\"", 30);
    assert!(log.contains("automatic vacuum of table \"postgres.public.av_small\": index scans: 0\n\tpages: 0 removed, "), "{log}");
    assert!(log.contains("\n\ttuples: 0 removed, 1500 remain, 0 are dead but not yet removable\n"), "{log}");
    assert!(log.contains("\n\tsystem usage: CPU: user: "), "{log}");
    // A slow ANALYZE canceled by a conflicting lock (ProcessInterrupts' error, with the
    // table as CONTEXT).
    rows(&mut s, "CREATE TABLE av_t (a int, b text) WITH (autovacuum_vacuum_insert_threshold = -1)");
    rows(&mut s, "INSERT INTO av_t SELECT i, repeat('x', 100) FROM generate_series(1, 2000) i");
    rows(&mut s, "VACUUM av_t");
    rows(&mut s, "SELECT pg_stat_force_next_flush()");
    let t = Instant::now();
    while rows(&mut s, "SELECT count(*) FROM pg_stat_progress_analyze") != ["[Int8(1)]"] && t.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(100));
    }
    rows(&mut s, "BEGIN; LOCK TABLE av_t IN ACCESS EXCLUSIVE MODE");
    rows(&mut s, "COMMIT");
    let log = log_has("canceling autovacuum task", 10);
    assert!(
        log.contains("ERROR:  canceling autovacuum task\n")
            && log.contains("CONTEXT:  automatic analyze of table \"postgres.public.av_t\"\n"),
        "{log}"
    );
    assert!(log.contains("] ERROR:  canceling autovacuum task"), "an autovacuum worker's line: {log}");
    pg_engine::log::set_sink(None);
    drop(s);
    e.shutdown();
}
