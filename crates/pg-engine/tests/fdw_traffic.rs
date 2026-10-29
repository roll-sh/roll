//! postgres_fdw's traffic on a loopback workload: remote writes (round trips), bytes each
//! way, time and peak memory, for comparing changes to the wrapper's remote protocol use.
//!
//! ```sh
//! cargo test -p pg-engine --test fdw_traffic -- --ignored --nocapture
//! ```

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use pg_engine::session::Session;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn open(data: &std::path::Path) -> Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: data.to_path_buf(),
        cache_bytes: 16 << 20,
        memtable_bytes: 4 << 20,
        sync: false,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: vec!["local".into()],
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: Vec::new(),
    })
    .unwrap()
}

fn run(s: &mut Session, sql: &str) -> usize {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last().map_or(0, |r| r.rows.len())
}

fn peak_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0)
}

/// One phase: its name, statements run, rows read; then the traffic it made.
fn phase(s: &mut Session, name: &str, sqls: &[String]) {
    let before = pg_engine::remote::conn::stats();
    let t = Instant::now();
    let mut rows = 0;
    for q in sqls {
        rows += run(s, q);
    }
    let el = t.elapsed();
    let after = pg_engine::remote::conn::stats();
    let d: Vec<u64> = (0..4).map(|i| after[i] - before[i]).collect();
    println!(
        "{name:<22} stmts {:>5} rows {:>7} writes {:>6} ({:.2}/stmt) out {:>9} B in {:>10} B reads {:>6} {:>8.1} ms",
        sqls.len(),
        rows,
        d[0],
        d[0] as f64 / sqls.len() as f64,
        d[1],
        d[3],
        d[2],
        el.as_secs_f64() * 1000.0
    );
}

#[test]
#[ignore]
fn loopback_traffic() {
    let data = tempfile::tempdir().unwrap();
    let engine = open(data.path());
    // (The server the wrapper's connections without a host reach.)
    pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::Trust }).unwrap();
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    for q in [
        "CREATE EXTENSION postgres_fdw".to_string(),
        "CREATE SERVER loop FOREIGN DATA WRAPPER postgres_fdw OPTIONS (dbname 'postgres')".into(),
        "CREATE USER MAPPING FOR CURRENT_USER SERVER loop".into(),
        "CREATE TABLE base (id int PRIMARY KEY, v text, n numeric)".into(),
        "INSERT INTO base SELECT g, 'value ' || g, g * 1.5 FROM generate_series(1, 20000) g".into(),
        "CREATE TABLE side (id int PRIMARY KEY, w text)".into(),
        "INSERT INTO side SELECT g, 'w' || g FROM generate_series(1, 2000) g".into(),
        "CREATE FOREIGN TABLE ft (id int, v text, n numeric) SERVER loop OPTIONS (table_name 'base')".into(),
        "CREATE FOREIGN TABLE fs (id int, w text) SERVER loop OPTIONS (table_name 'side')".into(),
        "CREATE TABLE loc (id int)".into(),
        "INSERT INTO loc SELECT g * 7 FROM generate_series(1, 200) g".into(),
        "ANALYZE loc".into(),
    ] {
        run(&mut s, &q);
    }
    let base = peak_kb();
    let lookups: Vec<String> = (1..=500).map(|i| format!("SELECT * FROM ft WHERE id = {}", i * 37 % 20000 + 1)).collect();
    phase(&mut s, "point lookups", &lookups);
    let mut tx = vec!["BEGIN".to_string()];
    tx.extend((1..=300).map(|i| format!("SELECT v FROM ft WHERE id = {}", i * 13 % 20000 + 1)));
    tx.push("COMMIT".into());
    phase(&mut s, "lookups in one txn", &tx);
    let scans: Vec<String> = (0..10).map(|_| "SELECT * FROM ft".to_string()).collect();
    phase(&mut s, "full scans (20k rows)", &scans);
    let ranges: Vec<String> = (0..200).map(|i| format!("SELECT * FROM ft WHERE id BETWEEN {} AND {}", i * 50, i * 50 + 30)).collect();
    phase(&mut s, "range scans (~31 rows)", &ranges);
    let joins: Vec<String> = (0..100).map(|i| format!("SELECT ft.v, fs.w FROM ft JOIN fs ON ft.id = fs.id WHERE fs.id < {}", 10 + i)).collect();
    phase(&mut s, "foreign joins", &joins);
    let aggs: Vec<String> = (0..100).map(|i| format!("SELECT count(*), sum(n) FROM ft WHERE id > {}", i * 100)).collect();
    phase(&mut s, "remote aggregates", &aggs);
    let nl: Vec<String> = (0..5).map(|_| "SELECT count(*) FROM loc JOIN ft ON ft.id = loc.id".to_string()).collect();
    phase(&mut s, "local join foreign", &nl);
    let ins: Vec<String> = (0..300).map(|i| format!("INSERT INTO ft VALUES ({}, 'x', 1)", 100000 + i)).collect();
    phase(&mut s, "single-row inserts", &ins);
    let upd: Vec<String> = (0..100).map(|i| format!("UPDATE ft SET v = 'y' WHERE id = {}", 100000 + i)).collect();
    phase(&mut s, "direct updates", &upd);
    println!("peak RSS {} kB (after setup {} kB)", peak_kb(), base);
}
