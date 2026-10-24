//! Parallel sequential scans (`exec::parscan`): a big table (past the heap model's limit, so
//! read in storage order) is read by worker threads, which decode only the columns needed
//! and filter by kernels (`exec::kernel`). Whatever they do, a query gives what the serial
//! scan gives: the same rows in the same order, the same errors, the same statistics.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::time::Duration;

fn open(dir: &std::path::Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 64 << 20,
        memtable_bytes: 8 << 20,
        sync: false,
        idle_close: Duration::from_secs(600),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
    })
    .unwrap()
}

/// The last result's rows, or the error.
fn out(s: &mut Session, sql: &str) -> Vec<String> {
    match s.query(sql) {
        Ok(r) => r
            .last()
            .map(|q| q.rows.iter().map(|row| format!("{row:?}")).collect())
            .unwrap_or_default(),
        Err(e) => vec![format!("ERROR: {} {:?} {:?}", e.message, e.detail, e.code)],
    }
}

const QUERIES: &[&str] = &[
    // Kernel filters (comparisons, LIKE, IN, NULL tests, CASE, arithmetic).
    "SELECT id, s FROM big WHERE s LIKE '%ab%' AND g IN (1, 5, 7)",
    "SELECT count(*), sum(x), avg(x), min(s), max(f) FROM big WHERE g < 10 AND x > 50",
    "SELECT id, x FROM big WHERE x IS NULL OR (x * 2 > 190 AND s NOT LIKE 'a%')",
    "SELECT id FROM big WHERE CASE WHEN g = 3 THEN f > 1000 ELSE id < 50 END",
    "SELECT id, g FROM big WHERE c = 'b' AND id + 5 > 419990",
    // Filters left to the executor's thread (id % 3: no kernel).
    "SELECT g, count(*), sum(f) FROM big WHERE id % 3 = 0 GROUP BY g ORDER BY g",
    "SELECT id FROM big WHERE x * 2 > 150 LIMIT 7",
    // No filter; all columns; a projection over few.
    "SELECT * FROM big WHERE id > 419995",
    "SELECT count(*) FROM big",
    "SELECT sum(id::bigint), count(s) FROM big",
    // Errors: a kernel's overflow, a bad LIKE pattern, a division by zero, at a late row.
    "SELECT count(*) FROM big WHERE id * 100000 > 0",
    "SELECT count(*) FROM big WHERE s LIKE 'ab\\'",
    "SELECT count(*) FROM big WHERE id = 400000 AND 1 / (id - 400000) > 0",
    "SELECT count(*) FROM big WHERE g > 200 OR (id > 419000 AND id * 10000 > 0)",
    // Aggregates over the scan, aggregated in parts by the workers (`exec::partial`): the
    // hash table's group order, exact sums, the first min / max, NULLs, a row a kernel
    // can't do (x / 0 below an OR, left to the serial path) and errors.
    "SELECT g, count(*), sum(x), avg(x), min(s), max(s), count(x), sum(id), avg(id), max(f) FROM big WHERE id > 1000 GROUP BY g",
    "SELECT c, g % 2 = 0, sum(x * 2), min(x), max(c) FROM big GROUP BY c, g % 2 = 0",
    "SELECT count(*), sum(x), min(id) FROM big WHERE s LIKE '%a%'",
    "SELECT g, sum(id * 100000) FROM big GROUP BY g",
    "SELECT s, count(*) FROM big WHERE id < 100 OR id > 419900 GROUP BY s",
    "SELECT sum(f), count(*) FROM big WHERE g = 1",
    // min / max over values that compare equal but differ (3 and 3.00): which one a tie
    // keeps shows.
    "SELECT g, max(n), min(n), count(n) FROM big GROUP BY g",
];

fn run_all(s: &mut Session) -> Vec<Vec<String>> {
    QUERIES.iter().map(|q| out(s, q)).collect()
}

fn seq_read(s: &mut Session) -> String {
    out(s, "SELECT seq_scan, seq_tup_read FROM pg_stat_user_tables WHERE relname = 'big'").join(",")
}

#[test]
fn parallel_scans_match_serial() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    s.query(
        "CREATE TABLE big (id int NOT NULL, g int, x numeric(12,2), s text, f float8, c char(1), n numeric)",
    )
    .unwrap();
    s.query(
        "INSERT INTO big SELECT i, i % 97, CASE WHEN i % 11 = 0 THEN NULL ELSE (i % 1000) / 10.0 END, \
         md5(i::text), i / 7.0, chr(97 + i % 3), \
         CASE WHEN i % 2 = 0 THEN (i % 50)::numeric ELSE (i % 50)::numeric(10, 2) END \
         FROM generate_series(1, 420000) i",
    )
    .unwrap();
    s.query("SET max_parallel_workers_per_gather = 0").unwrap();
    let before = pg_engine::exec::parscan::started();
    let serial = run_all(&mut s);
    assert_eq!(pg_engine::exec::parscan::started(), before, "no parallel scan with 0 workers");
    let stat_serial = seq_read(&mut s);
    s.query("SET max_parallel_workers_per_gather = 2").unwrap();
    let parallel = run_all(&mut s);
    assert!(pg_engine::exec::parscan::started() > before, "the scans ran in parallel");
    let stat_parallel = seq_read(&mut s);
    for ((q, a), b) in QUERIES.iter().zip(&serial).zip(&parallel) {
        assert_eq!(a, b, "{q}");
    }
    // Each run read the table as often and as far: the counts doubled.
    let n = |st: &str| -> Vec<i64> {
        st.split('(')
            .skip(1)
            .map(|x| x.split(')').next().unwrap().parse().unwrap())
            .collect()
    };
    let (a, b) = (n(&stat_serial), n(&stat_parallel));
    assert_eq!(a.len(), 2, "{stat_serial}");
    // (seq_scan and seq_tup_read: twice the serial run's, less the stats query itself.)
    assert_eq!(b[0], 2 * a[0], "{stat_serial} {stat_parallel}");
    assert_eq!(b[1], 2 * a[1], "{stat_serial} {stat_parallel}");
    // Errors came out as errors.
    assert!(serial[10][0].starts_with("ERROR: integer out of range"), "{:?}", serial[10]);
    assert!(serial[11][0].starts_with("ERROR: LIKE pattern must not end"), "{:?}", serial[11]);
    assert!(serial[12][0].starts_with("ERROR: division by zero"), "{:?}", serial[12]);
    assert!(serial[17][0].starts_with("ERROR: integer out of range"), "{:?}", serial[17]);
    assert_eq!(serial[14].len(), 97, "{:?}", serial[14]);
    assert_eq!(serial[15].len(), 6, "{:?}", serial[15]);
}

#[test]
fn split_keys_cover_in_order() {
    use pg_engine::exec::parscan::split_keys;
    let lo = vec![0, 0, 0, 7, 1, 2, 3];
    let hi = vec![0, 0, 0, 7, 9, 0, 0, 0, 0, 0, 1];
    let cuts = split_keys(&lo, &hi, 16);
    assert!(!cuts.is_empty());
    let mut prev = lo.clone();
    for c in &cuts {
        assert!(*c > prev && *c <= hi, "{c:?}");
        prev = c.clone();
    }
    assert!(split_keys(&lo, &lo, 4).is_empty());
}
