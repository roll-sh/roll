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
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 0,
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
    // (Ordered where the parallel plan groups sorted and the serial one hashed: their groups
    // come out in different orders, as Postgres's do; the values are the serial plan's.)
    "SELECT g, count(*), sum(x), avg(x), min(s), max(s), count(x), sum(id), avg(id), max(f) FROM big WHERE id > 1000 GROUP BY g ORDER BY g",
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

/// How the plan's top grouping groups (EXPLAIN's): "hashed", "sorted" or "".
fn grouping(s: &mut Session, q: &str) -> &'static str {
    let plan = out(s, &format!("EXPLAIN (COSTS OFF) {q}"));
    match plan.iter().find(|l| l.contains("Aggregate")) {
        Some(l) if l.contains("HashAggregate") => "hashed",
        Some(l) if l.contains("GroupAggregate") => "sorted",
        _ => "",
    }
}

/// The serial and parallel runs' rows agree: in the same order, or (when the parallel plan
/// groups otherwise than the serial one, as Postgres's plans for the two settings do) as
/// sets, the groups coming out in each plan's order.
fn assert_same(q: &str, a: &[String], b: &[String], plans: (&str, &str)) {
    if a == b {
        return;
    }
    assert_ne!(plans.0, plans.1, "{q}: the rows differ under the same grouping\n{a:?}\n{b:?}");
    let (mut x, mut y) = (a.to_vec(), b.to_vec());
    x.sort();
    y.sort();
    assert_eq!(x, y, "{q}");
}

fn seq_read(s: &mut Session) -> String {
    // (The counts reach the shared statistics when flushed: forced, as within a second of
    // the last flush they wait.)
    out(s, "SELECT pg_stat_force_next_flush()");
    out(s, "SELECT seq_scan, seq_tup_read FROM pg_stat_user_tables WHERE relname = 'big'").join(",")
}

/// Run a test's sessions on a big stack, as the server gives its session threads (an
/// unoptimized build's frames don't fit the 2 MB of a test thread).
fn on_session_thread(f: fn()) {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            pg_engine::stack::set_limit(60 << 20);
            f()
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn parallel_scans_match_serial() {
    on_session_thread(parallel_scans_match_serial_inner);
}

fn parallel_scans_match_serial_inner() {
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
    let serial_plans: Vec<&str> = QUERIES.iter().map(|q| grouping(&mut s, q)).collect();
    let before = pg_engine::exec::parscan::started();
    let serial = run_all(&mut s);
    assert_eq!(pg_engine::exec::parscan::started(), before, "no parallel scan with 0 workers");
    let stat_serial = seq_read(&mut s);
    s.query("SET max_parallel_workers_per_gather = 2").unwrap();
    let parallel = run_all(&mut s);
    assert!(pg_engine::exec::parscan::started() > before, "the scans ran in parallel");
    let stat_parallel = seq_read(&mut s);
    let parallel_plans: Vec<&str> = QUERIES.iter().map(|q| grouping(&mut s, q)).collect();
    for (i, ((q, a), b)) in QUERIES.iter().zip(&serial).zip(&parallel).enumerate() {
        assert_same(q, a, b, (serial_plans[i], parallel_plans[i]));
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

/// Joins over the big table, whose hash join probes (and aggregates above them) run in the
/// parallel scan's workers (`exec::pipe`): rows in the serial order, errors at the same row.
const JOIN_QUERIES: &[&str] = &[
    // Inner joins: rows (their order shows), and aggregated in parts.
    "SELECT b.id, d.name, b.x FROM big b JOIN dim d ON b.g = d.k WHERE b.id > 419000",
    // (Ordered: the serial plan hashes, the parallel one sorts, so their groups come out
    // in different orders, as Postgres's do.)
    "SELECT d.name, count(*), sum(b.x), avg(b.id), min(b.s), max(d.w) FROM big b JOIN dim d ON b.g = d.k GROUP BY d.name ORDER BY 1",
    "SELECT d.w, count(*), sum(b.f) FROM big b JOIN dim d ON b.g = d.k WHERE b.s LIKE '%a%' GROUP BY d.w ORDER BY 1",
    // Several inner rows per key; a join condition beyond the keys.
    "SELECT b.id, m.k, m.t FROM big b JOIN multi m ON b.g = m.k AND m.t <> b.c WHERE b.id < 2000",
    "SELECT m.t, count(*), sum(b.id) FROM big b JOIN multi m ON b.g = m.k AND b.x > m.k GROUP BY m.t",
    // Outer, semi and anti joins (keys that match nothing: dim has 60 of g's 97 values).
    "SELECT b.id, d.name FROM big b LEFT JOIN dim d ON b.g = d.k WHERE b.id % 5000 = 1",
    "SELECT d.name IS NULL, count(*), sum(b.x) FROM big b LEFT JOIN dim d ON b.g = d.k GROUP BY 1",
    "SELECT count(*), sum(b.x) FROM big b WHERE EXISTS (SELECT 1 FROM dim d WHERE d.k = b.g AND d.w < 3)",
    "SELECT b.g, count(*) FROM big b WHERE NOT EXISTS (SELECT 1 FROM dim d WHERE d.k = b.g) GROUP BY b.g",
    // Two joins stacked over the scan.
    "SELECT d2.t, count(*), sum(b.x) FROM big b JOIN dim d ON b.g = d.k JOIN multi d2 ON d.w = d2.k GROUP BY d2.t",
    "SELECT b.id, d.name, d2.t FROM big b JOIN dim d ON b.g = d.k JOIN multi d2 ON d.w = d2.k WHERE b.id > 419900",
    // Keys of other types: a numeric meeting an integer, text.
    "SELECT d.name, count(*) FROM big b JOIN dim d ON b.n = d.k GROUP BY d.name",
    "SELECT count(*), min(b.id) FROM big b JOIN dim d ON b.c = d.c1",
    // Errors at a late row: in a key, in the join condition; a filter left to the
    // executor's thread (id % 3).
    "SELECT count(*) FROM big b JOIN dim d ON b.id * 10000 = d.k",
    "SELECT count(*) FROM big b JOIN dim d ON b.g = d.k AND 1 / (b.id - 419990) > -5",
    "SELECT d.name, count(*) FROM big b JOIN dim d ON b.g = d.k WHERE b.id % 3 = 0 GROUP BY d.name",
    // A LIMIT stopping early.
    "SELECT b.id, d.name FROM big b JOIN dim d ON b.g = d.k LIMIT 5",
];

#[test]
fn parallel_joins_match_serial() {
    on_session_thread(parallel_joins_match_serial_inner);
}

fn parallel_joins_match_serial_inner() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    s.query(
        "CREATE TABLE big (id int NOT NULL, g int, x numeric(12,2), s text, f float8, c char(1), n numeric)",
    )
    .unwrap();
    s.query(
        "INSERT INTO big SELECT i, i % 97, CASE WHEN i % 11 = 0 THEN NULL ELSE (i % 1000) / 10.0 END, \
         md5(i::text), i / 7.0, chr(97 + i % 3), (i % 50)::numeric(10, 2) \
         FROM generate_series(1, 420000) i",
    )
    .unwrap();
    s.query(
        "CREATE TABLE dim (k int PRIMARY KEY, name text, w int, c1 char(1)); \
         INSERT INTO dim SELECT k, 'name ' || (k % 13), k % 7, chr(97 + k % 2) FROM generate_series(0, 59) k; \
         CREATE TABLE multi (k int, t text); \
         INSERT INTO multi SELECT k % 40, 't' || k FROM generate_series(1, 120) k; \
         ANALYZE big; ANALYZE dim; ANALYZE multi",
    )
    .unwrap();
    let run = |s: &mut Session| -> Vec<Vec<String>> { JOIN_QUERIES.iter().map(|q| out(s, q)).collect() };
    s.query("SET max_parallel_workers_per_gather = 0").unwrap();
    let serial_plans: Vec<&str> = JOIN_QUERIES.iter().map(|q| grouping(&mut s, q)).collect();
    let before = pg_engine::exec::parscan::pipelined();
    let serial = run(&mut s);
    assert_eq!(pg_engine::exec::parscan::pipelined(), before, "no parallel scan with 0 workers");
    let stat_serial = seq_read(&mut s);
    s.query("SET max_parallel_workers_per_gather = 2").unwrap();
    let parallel = run(&mut s);
    assert!(pg_engine::exec::parscan::pipelined() > before, "join probes ran in the workers");
    let stat_parallel = seq_read(&mut s);
    let parallel_plans: Vec<&str> = JOIN_QUERIES.iter().map(|q| grouping(&mut s, q)).collect();
    for (i, ((q, a), b)) in JOIN_QUERIES.iter().zip(&serial).zip(&parallel).enumerate() {
        assert_same(q, a, b, (serial_plans[i], parallel_plans[i]));
    }
    // The big table was read as often and as far (rows the joins dropped too).
    let n = |st: &str| -> Vec<i64> {
        st.split('(')
            .skip(1)
            .map(|x| x.split(')').next().unwrap().parse().unwrap())
            .collect()
    };
    let (a, b) = (n(&stat_serial), n(&stat_parallel));
    assert_eq!(a.len(), 2, "{stat_serial}");
    assert_eq!(b[0], 2 * a[0], "{stat_serial} {stat_parallel}");
    assert_eq!(b[1], 2 * a[1], "{stat_serial} {stat_parallel}");
    assert!(serial[13][0].starts_with("ERROR: integer out of range"), "{:?}", serial[13]);
    assert!(serial[14][0].starts_with("ERROR: division by zero"), "{:?}", serial[14]);
    assert!(serial[0].len() > 300, "{:?}", serial[0].len());
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
