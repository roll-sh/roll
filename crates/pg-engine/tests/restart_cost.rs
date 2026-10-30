//! What a restart costs the first statements on a table (`#[ignore]`d; a measurement, not a
//! check). Two processes, so the second's peak memory is its own:
//!
//!     RESTART_DIR=<dir> RESTART_PHASE=load RESTART_ROWS=300000 \
//!       cargo test --release -p pg-engine --test restart_cost -- --ignored --nocapture
//!     RESTART_DIR=<dir> RESTART_PHASE=open cargo test ... (same)
//!
//! `load` makes `acc` (pgbench_accounts' shape, RESTART_ROWS rows, a primary key), updates a
//! tenth of its rows and shuts down cleanly (RESTART_CRASH=1: exits without `shutdown`, as a
//! crash would). `open` reopens it and times the open, then each statement of
//! RESTART_STATEMENTS (`;`-separated, default a point SELECT, ctid, EXPLAIN, UPDATE), and
//! prints the process's peak RSS and CPU time.

use pg_engine::engine::{Engine, EngineOptions};
use std::time::{Duration, Instant};

// (The server's allocator: these numbers are of allocation-heavy work.)
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn open(dir: &std::path::Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 64 << 20,
        memtable_bytes: 8 << 20,
        // (A crash keeps only what was synced.)
        sync: std::env::var("RESTART_CRASH").is_ok(),
        idle_close: Duration::from_secs(600),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: Vec::new(),
    })
    .unwrap()
}

fn status(field: &str) -> String {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines().find(|l| l.starts_with(field)).unwrap_or("").split_whitespace().nth(1).unwrap_or("").to_string()
}

fn cpu_secs() -> f64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap();
    let f: Vec<&str> = s.rsplit(')').next().unwrap().split_whitespace().collect();
    (f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap()) / 100.0
}

#[test]
#[ignore]
fn restart_cost() {
    let dir = std::path::PathBuf::from(std::env::var("RESTART_DIR").expect("RESTART_DIR"));
    let phase = std::env::var("RESTART_PHASE").unwrap_or_else(|_| "open".into());
    if phase == "load" {
        let n: u64 = std::env::var("RESTART_ROWS").map_or(300_000, |v| v.parse().unwrap());
        let eng = open(&dir);
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        s.query(
            "CREATE TABLE acc (aid int PRIMARY KEY, bid int, abalance int, filler char(84))",
        )
        .unwrap();
        let mut i = 1;
        while i <= n {
            let j = (i + 99_999).min(n);
            s.query(&format!(
                "INSERT INTO acc SELECT g, g / 100000, 0, '' FROM generate_series({i}, {j}) g"
            ))
            .unwrap();
            i = j + 1;
        }
        s.query("UPDATE acc SET abalance = abalance + 1 WHERE aid % 10 = 0").unwrap();
        s.query("CHECKPOINT").unwrap();
        drop(s);
        if std::env::var("RESTART_CRASH").is_ok() {
            std::process::exit(0);
        }
        eng.shutdown();
        return;
    }
    let t0 = Instant::now();
    let eng = open(&dir);
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    let opened = t0.elapsed();
    println!("open+connect {:.3} s", opened.as_secs_f64());
    let stmts = std::env::var("RESTART_STATEMENTS").unwrap_or_else(|_| {
        "SELECT abalance FROM acc WHERE aid = 777;\
         SELECT ctid FROM acc WHERE aid = 778;\
         EXPLAIN SELECT * FROM acc WHERE bid = 1;\
         UPDATE acc SET abalance = abalance + 1 WHERE aid = 779;\
         SELECT pg_relation_size('acc')"
            .into()
    });
    for q in stmts.split(';').filter(|q| !q.trim().is_empty()) {
        let t = Instant::now();
        let r = s.query(q).unwrap_or_else(|e| panic!("{q}: {e:?}"));
        let first = r.last().and_then(|r| r.rows.first().map(|row| format!("{row:?}")));
        println!("{:.3} s  {q}  -> {}", t.elapsed().as_secs_f64(), first.unwrap_or_default());
    }
    println!("total {:.3} s, cpu {:.2} s, VmHWM {} kB", t0.elapsed().as_secs_f64(), cpu_secs(), status("VmHWM:"));
    drop(s);
    // (For profiling the statements alone.)
    if std::env::var("RESTART_NO_CLOSE").is_ok() {
        std::process::exit(0);
    }
    let t1 = Instant::now();
    eng.shutdown();
    println!("shutdown {:.3} s", t1.elapsed().as_secs_f64());
}
