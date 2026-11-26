//! Spills past work_mem (`exec::spill`, `extsort`, `hashspill`): the results of sorts and
//! groupings that go to disk are those of the same statements in memory; their temporary
//! files are gone after the statement, after an error in the middle of one and past
//! temp_file_limit; and pg_stat_database / pg_stat_statements count them.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use pg_engine::types::Value;
use std::time::Duration;

fn open(dir: &std::path::Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 256 << 10,
        sync: false,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: vec!["pg_stat_statements".into()],
        outbound: Vec::new(),
        locale: Default::default(),
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: Vec::new(),
    })
    .unwrap()
}

/// The statement's last result's rows, or its error.
fn q(s: &mut Session, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    match s.query(sql) {
        Ok(r) => Ok(r.last().map(|r| r.rows.clone()).unwrap_or_default()),
        Err(e) => Err(format!("{} {}", e.code, e.message)),
    }
}

fn one(s: &mut Session, sql: &str) -> Value {
    let rows = q(s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows[0][0].clone()
}

fn int(s: &mut Session, sql: &str) -> i64 {
    let v = one(s, sql);
    v.as_i64().unwrap_or_else(|| panic!("{sql}: {v:?}"))
}

/// Files left in any database's temporary spill directory.
fn temp_files(dir: &std::path::Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.parent().is_some_and(|x| x.ends_with("spill/tmp")) {
                n += 1;
            }
        }
    }
    n
}


fn assert_no_spill_files(dir: &std::path::Path) {
    assert_eq!(temp_files(dir), 0);
    // Unlinked files still hold disk space until the descriptor closes.
    #[cfg(target_os = "linux")]
    for e in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
        if let Ok(p) = std::fs::read_link(e.path()) {
            assert!(!(p.starts_with(dir) && p.to_string_lossy().contains("spill/tmp/")), "open spill file: {}", p.display());
        }
    }
}

/// A CTE's tuplestore past work_mem (`exec::tuplestore`): its rows go to a temporary file
/// and every scan of it reads them back in order, the leading one as they come, a lagging
/// one or a rescan from the file; the file counts against temp_file_limit, and it's gone
/// after the statement, an error and a LIMIT.
#[test]
fn cte_tuplestore_spills_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE TABLE ct (g int, s text)",
        "INSERT INTO ct SELECT g, md5(g::text) FROM generate_series(1, 20000) g",
        "ANALYZE ct",
        "SET max_parallel_workers_per_gather = 0",
    ] {
        q(&mut s, sql).unwrap();
    }
    let queries = [
        "WITH c AS MATERIALIZED (SELECT g, s FROM ct) SELECT count(*), sum(g), md5(string_agg(s, ',')) FROM c",
        // (Two scans at different places: a hash join reads one side whole first.)
        "WITH c AS MATERIALIZED (SELECT g, s FROM ct) SELECT count(*), md5(string_agg(a.s || b.s, ',' ORDER BY a.g)) FROM c a JOIN c b ON a.g = b.g + 7",
        // (A nested loop rescans the inner one.)
        "WITH c AS MATERIALIZED (SELECT g, s FROM ct) SELECT count(*), sum(b.g) FROM (SELECT * FROM c LIMIT 3) a, LATERAL (SELECT * FROM c WHERE c.g > 19990 + a.g OFFSET 0) b",
        "WITH c AS MATERIALIZED (SELECT g, s FROM ct) SELECT (SELECT count(*) FROM c), count(*), min(s) FROM c WHERE g > 10000",
    ];
    let in_memory: Vec<Vec<Vec<Value>>> = queries.iter().map(|sql| q(&mut s, sql).unwrap()).collect();
    q(&mut s, "SET work_mem = '64kB'").unwrap();
    for (sql, want) in queries.iter().zip(&in_memory) {
        assert_eq!(&q(&mut s, sql).unwrap(), want, "{sql}");
        assert_no_spill_files(dir.path());
    }
    assert_eq!(q(&mut s, "WITH c AS MATERIALIZED (SELECT g FROM ct) SELECT g FROM c LIMIT 2").unwrap().len(), 2);
    assert_no_spill_files(dir.path());
    let e = q(&mut s, "WITH c AS MATERIALIZED (SELECT g, s FROM ct) SELECT count(*) FROM c WHERE 1 / (g - 15000) > 0").unwrap_err();
    assert!(e.starts_with("22012"), "{e}");
    assert_no_spill_files(dir.path());
    q(&mut s, "SET temp_file_limit = '256kB'").unwrap();
    let e = q(&mut s, "WITH c AS MATERIALIZED (SELECT g, s FROM ct) SELECT count(*) FROM c").unwrap_err();
    assert!(e.contains("temp_file_limit"), "{e}");
    assert_no_spill_files(dir.path());
    drop(s);
    eng.shutdown();
}

const QUERIES: &[&str] = &[
    "SELECT md5(string_agg(a::text, ',')) FROM (SELECT a FROM sp ORDER BY t, a) s",
    "SELECT md5(string_agg(a::text, ',')) FROM (SELECT a FROM sp ORDER BY n, a DESC) s",
    "SELECT md5(string_agg(b || ':' || c, ',' ORDER BY b)) FROM (SELECT b, count(*) c FROM sp GROUP BY b) s",
    "SELECT md5(string_agg(t || ':' || c || ':' || s, ',' ORDER BY t)) FROM (SELECT t, count(*) c, sum(a) s FROM sp GROUP BY t) s",
    "SELECT count(*), sum(c) FROM (SELECT a, count(*) c FROM sp GROUP BY a) s",
    // Ordered aggregates' input sorted within work_mem: the first one's below the grouping,
    // the others' each in its own sort, DISTINCT and ordered-set ones too.
    "SELECT md5(string_agg(t, ',' ORDER BY a DESC) || array_agg(b ORDER BY t, a)::text) FROM sp",
    "SELECT md5(array_agg(DISTINCT b ORDER BY b DESC)::text || string_agg(t, ',' ORDER BY a)) FROM sp",
    "SELECT md5(string_agg(x, ';' ORDER BY b)) FROM (SELECT b, string_agg(t, ',' ORDER BY a DESC) x FROM sp GROUP BY b) s",
    "SELECT (mode() WITHIN GROUP (ORDER BY b))::text || percentile_cont(array[0, 0.37, 1]) WITHIN GROUP (ORDER BY n)::text \
       || percentile_disc(0.5) WITHIN GROUP (ORDER BY a DESC) FROM sp",
];

#[test]
fn spills_match_memory_and_clean_up() {
    // (A session's stack, as the server gives one: the plans of the ordered aggregates'
    // queries are deep for a debug build's frames.)
    std::thread::Builder::new().stack_size(64 << 20).spawn(spills_match_memory).unwrap().join().unwrap();
}

fn spills_match_memory() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE EXTENSION pg_stat_statements",
        "CREATE TABLE sp (a int, b int, t text, n numeric)",
        "INSERT INTO sp SELECT g, g % 5000, md5((g % 20000)::text), g % 1000 FROM generate_series(1, 40000) g",
        "ANALYZE sp",
        "SET enable_sort = off",
    ] {
        q(&mut s, sql).unwrap();
    }
    let in_memory: Vec<Value> = QUERIES.iter().map(|sql| one(&mut s, sql)).collect();
    q(&mut s, "SET work_mem = '64kB'").unwrap();
    q(&mut s, "SELECT pg_stat_statements_reset()").unwrap();
    for (sql, want) in QUERIES.iter().zip(&in_memory) {
        assert_eq!(&one(&mut s, sql), want, "{sql}");
        assert_eq!(temp_files(dir.path()), 0, "{sql}: files left");
    }
    // The spills wrote temporary blocks and files.
    let blks = int(&mut s, "SELECT sum(temp_blks_written)::int8 FROM pg_stat_statements WHERE query LIKE '%FROM sp%'");
    assert!(blks > 0, "temp_blks_written {blks}");
    // An error in the middle of a spilling sort: no files left.
    let e = q(&mut s, "SELECT count(*) FROM (SELECT a FROM sp ORDER BY t, 1 / (a - 39000)) s").unwrap_err();
    assert!(e.starts_with("22012"), "{e}");
    assert_eq!(temp_files(dir.path()), 0);
    // Past temp_file_limit: the limit's error, then the session goes on.
    q(&mut s, "SET temp_file_limit = '256kB'").unwrap();
    let e = q(&mut s, "SELECT count(*) FROM (SELECT * FROM sp ORDER BY t) s").unwrap_err();
    assert!(e.contains("temp_file_limit"), "{e}");
    assert_eq!(temp_files(dir.path()), 0);
    q(&mut s, "RESET temp_file_limit").unwrap();
    assert_eq!(int(&mut s, "SELECT count(*) FROM (SELECT * FROM sp ORDER BY t) s"), 40000);
    q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
    let files = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
    assert!(files >= 5, "temp_files {files}");
    drop(s);
    eng.shutdown();
}

#[test]
fn hash_join_batches_clean_up_after_completion_limit_and_error() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE EXTENSION pg_stat_statements",
        "CREATE TABLE hj (id int, t text)",
        "INSERT INTO hj SELECT g, md5(g::text) FROM generate_series(1, 12000) g",
        "ANALYZE hj",
        "SET max_parallel_workers_per_gather = 0",
        "SET enable_mergejoin = off",
        "SET enable_nestloop = off",
        "SET work_mem = '64kB'",
    ] {
        q(&mut s, sql).unwrap();
    }
    let sql = "SELECT count(*) FROM hj a JOIN hj b ON a.id = b.id";
    assert_eq!(int(&mut s, sql), 12000);
    assert_eq!(temp_files(dir.path()), 0);
    assert_no_spill_files(dir.path());
    assert!(int(&mut s, "SELECT sum(temp_blks_written)::int8 FROM pg_stat_statements WHERE query LIKE '%FROM hj%'") > 0);
    q(&mut s, "SELECT a.id FROM hj a JOIN hj b ON a.id = b.id LIMIT 1").unwrap();
    assert_no_spill_files(dir.path());
    let err = q(&mut s, "SELECT sum(1 / (a.id - 11000)) FROM hj a JOIN hj b ON a.id = b.id").unwrap_err();
    assert!(err.starts_with("22012"), "{err}");
    assert_no_spill_files(dir.path());
    q(&mut s, "SET temp_file_limit = '100kB'").unwrap();
    for _ in 0..2 {
        let err = q(&mut s, sql).unwrap_err();
        assert!(err.starts_with("53400") && err.contains("temp_file_limit"), "{err}");
        assert_no_spill_files(dir.path());
    }
    q(&mut s, "RESET temp_file_limit").unwrap();
    assert_eq!(int(&mut s, sql), 12000);
    assert_no_spill_files(dir.path());
    q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
    assert!(int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()") > 0);
    drop(s);
    eng.shutdown();
}

/// A hash join past work_mem whose batches have few outer rows: a reloaded batch keeps the
/// inner rows no outer row can match as stored, never decoded (`hashjoin::KEPT_STORED`).
/// Inner, semi, anti, left and right joins give the rows (and, in each, the row order) they
/// give with the inner side in memory, sorted alike.
#[test]
fn hash_join_batches_of_few_outer_rows() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    // (The inner side a function's rows, estimated at 1,000; the outer side a table whose
    // statistics count the 30,000 rows it had: the planner hashes the function's rows.)
    for sql in [
        "CREATE TABLE few (k int, w text) WITH (autovacuum_enabled = false)",
        "INSERT INTO few SELECT g * 37, 'w' || g FROM generate_series(1, 30000) g",
        "ANALYZE few",
        "DELETE FROM few WHERE k > 300 * 37",
        "INSERT INTO few VALUES (NULL, 'null'), (-5, 'none')",
        "SET max_parallel_workers_per_gather = 0",
        "SET enable_mergejoin = off",
        "SET enable_nestloop = off",
    ] {
        q(&mut s, sql).unwrap();
    }
    let big = "(SELECT g AS k, g % 97 AS g, md5(g::text) || repeat('x', g % 40) AS t FROM generate_series(1, (SELECT 30000)) g)";
    let queries = [
        format!("SELECT f.k, f.w, b.g, b.t FROM few f JOIN {big} b ON b.k = f.k ORDER BY 1, 2"),
        format!("SELECT f.k, f.w FROM few f WHERE EXISTS (SELECT 1 FROM {big} b WHERE b.k = f.k + 1 AND b.g <> 3) ORDER BY 1"),
        format!("SELECT f.k, f.w FROM few f WHERE NOT EXISTS (SELECT 1 FROM {big} b WHERE b.k = f.k) ORDER BY 1"),
        format!("SELECT f.k, f.w, b.t FROM few f LEFT JOIN {big} b ON b.k = f.k AND b.g > 10 ORDER BY 1, 2"),
        format!("SELECT b.k, f.w FROM few f RIGHT JOIN {big} b ON b.k = f.k WHERE b.k < 2000 ORDER BY 1"),
        format!("SELECT count(*), sum(length(b.t)) FROM few f JOIN {big} b ON b.g = f.k % 97"),
    ];
    let run = |s: &mut Session| queries.iter().map(|sql| q(s, sql).unwrap()).collect::<Vec<_>>();
    q(&mut s, "SET work_mem = '64MB'").unwrap();
    let memory = run(&mut s);
    q(&mut s, "SET work_mem = '64kB'").unwrap();
    let kept = pg_engine::exec::hashjoin::KEPT_STORED.load(std::sync::atomic::Ordering::Relaxed);
    let spilled = run(&mut s);
    assert!(pg_engine::exec::hashjoin::KEPT_STORED.load(std::sync::atomic::Ordering::Relaxed) > kept, "rows kept as stored");
    for ((sql, a), b) in queries.iter().zip(&memory).zip(&spilled) {
        assert_eq!(a, b, "{sql}");
    }
    assert!(memory[0].len() > 100 && memory[2].len() > 1, "{} {}", memory[0].len(), memory[2].len());
    assert_no_spill_files(dir.path());
    drop(s);
    eng.shutdown();
}

// Reference transcript: a150-resume-t3-864cc5fb/temp-reference.{sql,out} in ~/.cache.
#[test]
fn hash_join_limit_temp_accounting_matches_postgres() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE TABLE hj (id int, t text)",
        "INSERT INTO hj SELECT g, md5(g::text) FROM generate_series(1, 12000) g",
        "ANALYZE hj",
        "SET max_parallel_workers_per_gather = 0",
        "SET enable_mergejoin = off",
        "SET enable_nestloop = off",
        "SET work_mem = '64kB'",
        "SELECT count(*) FROM hj a JOIN hj b ON a.id = b.id",
        "SELECT a.id FROM hj a JOIN hj b ON a.id = b.id LIMIT 1",
        "SELECT pg_stat_force_next_flush()",
        "SELECT pg_stat_clear_snapshot()",
    ] {
        q(&mut s, sql).unwrap();
    }
    let actual = q(&mut s, "SELECT temp_files, temp_bytes FROM pg_stat_database WHERE datname = current_database()").unwrap();
    assert_eq!(actual, vec![vec![Value::Int8(27), Value::Int8(759192)]]);
    assert_no_spill_files(dir.path());
    q(&mut s, "SET temp_file_limit = '224kB'").unwrap();
    let err = q(&mut s, "SELECT a.id FROM hj a JOIN hj b ON a.id = b.id LIMIT 1").unwrap_err();
    assert_eq!(err, "53400 temporary file size exceeds \"temp_file_limit\" (224kB)");
    assert_no_spill_files(dir.path());
    // Execution already failed: abort discards buffers instead of replacing this error
    // with the tail flush's configuration-limit error.
    let err = q(&mut s, "SELECT 1 / (a.id - 14) FROM hj a JOIN hj b ON a.id = b.id LIMIT 1").unwrap_err();
    assert!(err.starts_with("22012"), "{err}");
    assert_no_spill_files(dir.path());
    // EOF on a cursor or suspended protocol portal does not end its executor.
    // PostgreSQL raises the buffered-tail error at CLOSE or COMMIT, after FETCH succeeds.
    for fetch in ["FETCH 1 c", "FETCH ALL c"] {
        q(&mut s, "BEGIN").unwrap();
        q(&mut s, "DECLARE c CURSOR FOR SELECT a.id FROM hj a JOIN hj b ON a.id = b.id LIMIT 1").unwrap();
        assert_eq!(int(&mut s, fetch), 14);
        let err = q(&mut s, "CLOSE c").unwrap_err();
        assert!(err.starts_with("53400"), "{err}");
        q(&mut s, "ROLLBACK").unwrap();
        assert_no_spill_files(dir.path());
    }
    q(&mut s, "BEGIN").unwrap();
    q(&mut s, "DECLARE c CURSOR FOR SELECT a.id FROM hj a JOIN hj b ON a.id = b.id LIMIT 1").unwrap();
    assert_eq!(int(&mut s, "FETCH 1 c"), 14);
    assert!(q(&mut s, "COMMIT").unwrap_err().starts_with("53400"));
    assert_no_spill_files(dir.path());
    q(&mut s, "BEGIN").unwrap();
    s.parse_message("spill", "SELECT a.id FROM hj a JOIN hj b ON a.id = b.id LIMIT 1", &[]).unwrap();
    s.bind_message("p", "spill", Vec::new(), &[], Vec::new(), false).unwrap();
    let (_, more) = s.execute_portal("p", 1).unwrap();
    assert!(more);
    let (_, more) = s.execute_portal("p", 0).unwrap();
    assert!(!more);
    assert_eq!(s.close_portal("p").unwrap_err().code, "53400");
    q(&mut s, "ROLLBACK").unwrap();
    assert_no_spill_files(dir.path());
    q(&mut s, "RESET temp_file_limit").unwrap();
    let cancel = s.cancel.clone();
    let err = s.query_each("SELECT a.id FROM hj a JOIN hj b ON a.id = b.id", move |_, row| {
        if row.is_some() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }).unwrap_err();
    assert_eq!(err.code, "57014");
    assert_no_spill_files(dir.path());
    assert_eq!(int(&mut s, "SELECT count(*) FROM hj a JOIN hj b ON a.id = b.id"), 12000);
    drop(s);
    eng.shutdown();
}

/// Window partitions past work_mem streamed (`exec::winstream`): every function and frame
/// it runs gives what the partition computed whole in memory does, a store past work_mem
/// spills and its files go. (Rows tied on ORDER BY b come in another order once the Sort
/// below spills, as PostgreSQL's do: what is read of them is equal within a peer group. The
/// in-memory path reads every row of a frame for first_value / last_value / nth_value:
/// partitions of 1,000 rows keep it short.)
const WINDOWS: &[&str] = &[
    "row_number() OVER (ORDER BY a DESC)",
    "rank() OVER w, dense_rank() OVER w, percent_rank() OVER w, cume_dist() OVER w FROM ws WINDOW w AS (ORDER BY b)",
    "sum(a) OVER w, count(*) FILTER (WHERE a % 3 = 0) OVER w, last_value(b % 7) OVER w, nth_value(b, 3) OVER w, first_value(b) OVER w \
       FROM ws WINDOW w AS (PARTITION BY a % 40 ORDER BY b)",
    "sum(a) OVER w, lag(t, 2) OVER w, lead(a, 3, -1) OVER w, max(t) OVER w FROM ws \
       WINDOW w AS (PARTITION BY b % 3 ORDER BY a ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)",
    "sum(n) OVER w, avg(a) OVER w, rank() OVER w, cume_dist() OVER w FROM ws WINDOW w AS (PARTITION BY b % 7)",
    "ntile(7) OVER w, lead(t, 3, 'x') OVER w, lag(a, 5000) OVER w FROM ws WINDOW w AS (ORDER BY a)",
    "last_value(a) OVER w, count(*) OVER w, nth_value(t, 999) OVER w FROM ws \
       WINDOW w AS (PARTITION BY a % 40 ORDER BY a ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)",
    "first_value(b) OVER w, nth_value(b, 2) OVER w FROM ws WINDOW w AS (PARTITION BY a % 40 ORDER BY b RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)",
    "sum(a) OVER w, last_value(b) OVER w FROM ws WINDOW w AS (PARTITION BY a % 40 ORDER BY b GROUPS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)",
    // Sliding frames (a frame head pointer), exclusions, a moving aggregate (msum).
    "sum(a) OVER w, count(*) OVER w, avg(n) OVER w, max(t) OVER w, first_value(t) OVER w, last_value(b) OVER w, nth_value(a, 3) OVER w \
       FROM ws WINDOW w AS (PARTITION BY a % 40 ORDER BY a ROWS BETWEEN 5 PRECEDING AND 5 FOLLOWING)",
    "sum(a) OVER w, first_value(a) OVER w, last_value(t) OVER w, nth_value(a, 3) OVER w, msum(a) OVER w FROM ws \
       WINDOW w AS (PARTITION BY a % 40 ORDER BY a ROWS BETWEEN 2 PRECEDING AND 2 FOLLOWING EXCLUDE CURRENT ROW)",
    "sum(a) OVER w, count(*) OVER w, first_value(a) OVER w, last_value(a) OVER w, msum(a) OVER w FROM ws \
       WINDOW w AS (PARTITION BY a % 40 ORDER BY a DESC RANGE BETWEEN 200 PRECEDING AND 80 FOLLOWING)",
    "sum(a) OVER w, count(*) OVER w, first_value(b) OVER w, last_value(b) OVER w, nth_value(b, 20) OVER w FROM ws \
       WINDOW w AS (PARTITION BY a % 40 ORDER BY b GROUPS BETWEEN 2 PRECEDING AND 1 FOLLOWING EXCLUDE TIES)",
];

/// The aggregate with a moving implementation the window cases use (PostgreSQL runs it with
/// its inverse transition over a frame whose head moves).
const MSUM: &str = "CREATE AGGREGATE msum(int) (sfunc = int4pl, stype = int, msfunc = int4pl, minvfunc = int4mi, mstype = int)";

#[test]
fn windows_stream_past_work_mem() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(windows_stream).unwrap().join().unwrap();
}

fn windows_stream() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE TABLE ws (a int, b int, t text, n numeric)",
        "INSERT INTO ws SELECT g, g % 3000, md5(g::text), g % 1000 FROM generate_series(1, 40000) g",
        "ANALYZE ws",
        MSUM,
        "SET work_mem = '256MB'",
    ] {
        q(&mut s, sql).unwrap();
    }
    let sql = |w: &str| {
        let sel = if w.contains(" FROM ws") { w.to_string() } else { format!("{w} FROM ws") };
        format!("SELECT md5(string_agg(x::text, ',' ORDER BY x::text)) FROM (SELECT a, b, {sel}) x")
    };
    let in_memory: Vec<Value> = WINDOWS.iter().map(|w| one(&mut s, &sql(w))).collect();
    q(&mut s, "SET work_mem = '64kB'").unwrap();
    for (w, want) in WINDOWS.iter().zip(&in_memory) {
        assert_eq!(&one(&mut s, &sql(w)), want, "{w}");
        assert_eq!(temp_files(dir.path()), 0, "{w}: files left");
    }
    // The window's own store spills (the Sort below spills too): lag's 5,000 rows back.
    q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
    let f0 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
    q(&mut s, "SELECT count(x) FROM (SELECT lag(t, 5000) OVER (ORDER BY a) x FROM ws) s").unwrap();
    q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
    let f1 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
    assert_eq!(f1 - f0, 2, "the Sort's file and the window's");
    // lag / lead a row away keep a few rows (the RANGE frame's tail at the peer group's end):
    // only the Sort's file.
    q(&mut s, "SELECT count(x) FROM (SELECT lag(t) OVER (ORDER BY a) x, lead(t, 2) OVER (ORDER BY a) FROM ws) s").unwrap();
    q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
    let f2 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
    assert_eq!(f2 - f1, 1, "the Sort's file alone");
    // An error in a streamed partition: no files left.
    let e = q(&mut s, "SELECT count(x) FROM (SELECT sum(1 / (a - 39000)) OVER (ORDER BY a ROWS UNBOUNDED PRECEDING) x FROM ws) s").unwrap_err();
    assert!(e.starts_with("22012"), "{e}");
    assert_no_spill_files(dir.path());
    drop(s);
    eng.shutdown();
}

/// Sliding frames past work_mem (`exec::winstream`: frame head and tail pointers): the results
/// PostgreSQL 18.6 gives for the same statements, and its temporary files (the Sort's, and the
/// window's store where its frames hold more than work_mem).
#[test]
fn sliding_frames_spill() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(sliding_frames).unwrap().join().unwrap();
}

fn sliding_frames() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE TABLE ws (a int, b int, t text, n numeric)",
        "INSERT INTO ws SELECT g, g % 3000, md5(g::text), g % 1000 FROM generate_series(1, 40000) g",
        "ANALYZE ws",
        MSUM,
        "SET work_mem = '64kB'",
    ] {
        q(&mut s, sql).unwrap();
    }
    // (PostgreSQL 18.6: md5 of the results, temp files of the statement.)
    for (inner, want, files) in [
        ("SELECT sum(a) OVER (ORDER BY a ROWS BETWEEN 5 PRECEDING AND 5 FOLLOWING) x FROM ws", "7dc0515f2ce854c32a858a27b5b4782a", 1),
        (
            "SELECT first_value(t) OVER w x, last_value(t) OVER w y, nth_value(a, 3) OVER w z FROM ws \
             WINDOW w AS (ORDER BY a ROWS BETWEEN 5000 PRECEDING AND UNBOUNDED FOLLOWING)",
            "6eeb3cd8156e5b199c277834770ebb25",
            2,
        ),
        ("SELECT msum(a) OVER (ORDER BY a ROWS BETWEEN 3000 PRECEDING AND 3000 FOLLOWING) x FROM ws", "3d23ac469a876fa2fcbd0099fc5573e8", 2),
        ("SELECT sum(a) OVER (ORDER BY a ROWS BETWEEN 600 PRECEDING AND 100 FOLLOWING) x FROM ws WHERE a <= 10000", "2c11311d375caa785231191b1dba7c6f", 1),
        (
            "SELECT count(*) OVER (ORDER BY b RANGE BETWEEN 50 PRECEDING AND 20 FOLLOWING EXCLUDE GROUP) x FROM ws WHERE a <= 10000",
            "c012e4d370e82b73004140fe3b03f5f0",
            1,
        ),
        ("SELECT sum(a) OVER (ORDER BY b GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) x FROM ws", "08a3e7833dd3729fe5194cb28a929ef7", 1),
        (
            "SELECT last_value(c) OVER (ORDER BY b GROUPS BETWEEN CURRENT ROW AND 200 FOLLOWING EXCLUDE TIES) x \
             FROM (SELECT b, md5(b::text) c FROM ws) w",
            "18d2f92989221ac95271f24747f21213",
            2,
        ),
    ] {
        let md5 = format!("SELECT md5(string_agg(x::text, ',' ORDER BY x::text)) FROM ({inner}) s");
        assert_eq!(one(&mut s, &md5), Value::Text(want.into()), "{inner}");
        q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
        let f0 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
        q(&mut s, &format!("SELECT count(x) FROM ({inner}) s")).unwrap();
        assert_eq!(temp_files(dir.path()), 0);
        q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
        let f1 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
        assert_eq!(f1 - f0, files, "{inner}: temp files");
    }
    drop(s);
    eng.shutdown();
}

/// A nested loop's inner side past work_mem (PostgreSQL's Materialize: a tuplestore): its
/// rows go to a temporary file read again for each outer row; the join's rows are those it
/// gives in memory.
#[test]
fn nested_loop_inner_spills() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(nested_loop_inner).unwrap().join().unwrap();
}

fn nested_loop_inner() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE TABLE nsmall (a int)",
        "INSERT INTO nsmall SELECT g FROM generate_series(1, 60) g",
        "CREATE TABLE nbig (a int, t text)",
        "INSERT INTO nbig SELECT g, md5(g::text) FROM generate_series(1, 20000) g",
        "ANALYZE nsmall",
        "ANALYZE nbig",
        "SET work_mem = '256MB'",
    ] {
        q(&mut s, sql).unwrap();
    }
    let sql = "SELECT count(*), md5(string_agg(x::text, ',' ORDER BY x::text)) FROM \
               (SELECT nsmall.a, nbig.a b, nbig.t FROM nsmall LEFT JOIN nbig ON nbig.a > nsmall.a * 330 AND nbig.a < nsmall.a * 333) x";
    let plan = q(&mut s, &format!("EXPLAIN {sql}")).unwrap();
    assert!(format!("{plan:?}").contains("Materialize"), "{plan:?}");
    let want = q(&mut s, sql).unwrap();
    q(&mut s, "SET work_mem = '64kB'").unwrap();
    q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
    let f0 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
    assert_eq!(q(&mut s, sql).unwrap(), want);
    assert_eq!(temp_files(dir.path()), 0);
    q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
    let f1 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
    assert_eq!(f1 - f0, 2, "the Materialize's file and the Sort's above");
    drop(s);
    eng.shutdown();
}

/// A DISTINCT run as a Unique over its input sorted (enable_hashagg off) past work_mem: the
/// sort's runs go to one temporary file, as PostgreSQL's Sort below the Unique writes; its
/// rows are those it gives in memory, in the same order.
#[test]
fn sorted_distinct_spills() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(sorted_distinct).unwrap().join().unwrap();
}

fn sorted_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE TABLE dup (a int, t text)",
        "INSERT INTO dup SELECT g % 7000, md5((g % 7000)::text) FROM generate_series(1, 20000) g",
        "ANALYZE dup",
        "SET enable_hashagg = off",
        "SET work_mem = '256MB'",
    ] {
        q(&mut s, sql).unwrap();
    }
    let sql = "SELECT DISTINCT t, a FROM dup";
    let plan = q(&mut s, &format!("EXPLAIN {sql}")).unwrap();
    assert!(format!("{plan:?}").contains("Unique"), "{plan:?}");
    let want = q(&mut s, sql).unwrap();
    assert_eq!(want.len(), 7000);
    q(&mut s, "SET work_mem = '64kB'").unwrap();
    q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
    let f0 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
    assert_eq!(q(&mut s, sql).unwrap(), want);
    assert_eq!(temp_files(dir.path()), 0);
    q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
    let f1 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
    assert_eq!(f1 - f0, 1, "the Sort's file below the Unique");
    drop(s);
    eng.shutdown();
}

/// Sorted EXCEPT and INTERSECT ALL (enable_hashagg off) past work_mem: each input's Sort goes
/// to a temporary file, two as PostgreSQL's; the rows are those they give in memory, in the
/// same order.
#[test]
fn sorted_setops_spill() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(sorted_setops).unwrap().join().unwrap();
}

fn sorted_setops() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE TABLE so (a int, t text)",
        "INSERT INTO so SELECT g % 9000, md5((g % 9000)::text) FROM generate_series(1, 20000) g",
        "ANALYZE so",
        "SET enable_hashagg = off",
    ] {
        q(&mut s, sql).unwrap();
    }
    for (sql, rows) in [
        ("SELECT t, a FROM so EXCEPT SELECT t, a FROM so WHERE a % 3 = 0", 6000),
        ("SELECT t, a FROM so INTERSECT ALL SELECT t, a FROM so WHERE a % 3 = 0", 6666),
    ] {
        q(&mut s, "SET work_mem = '256MB'").unwrap();
        let plan = q(&mut s, &format!("EXPLAIN {sql}")).unwrap();
        assert!(format!("{plan:?}").contains("SetOp"), "{plan:?}");
        let want = q(&mut s, sql).unwrap();
        assert_eq!(want.len(), rows);
        q(&mut s, "SET work_mem = '64kB'").unwrap();
        q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
        let f0 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
        assert_eq!(q(&mut s, sql).unwrap(), want, "{sql}");
        assert_eq!(temp_files(dir.path()), 0);
        q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
        let f1 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
        assert_eq!(f1 - f0, 2, "{sql}: the two Sorts' files");
    }
    drop(s);
    eng.shutdown();
}

/// Several windows, and a window over input not sorted for it, past work_mem: computed as
/// PostgreSQL's plan computes them, a WindowAgg a window over a Sort for it, each Sort's runs in
/// a temporary file (PostgreSQL 18.6's counts for the same statements); the rows are those they
/// give in memory, in the same order.
#[test]
fn several_windows_spill() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(several_windows).unwrap().join().unwrap();
}

fn several_windows() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE TABLE wm (a int, b int, t text)",
        "INSERT INTO wm SELECT g, (g::int8 * 7919 % 20011)::int, md5(g::text) FROM generate_series(1, 20000) g",
        "ANALYZE wm",
    ] {
        q(&mut s, sql).unwrap();
    }
    for (sql, files) in [
        ("SELECT row_number() OVER (ORDER BY b) r, rank() OVER (ORDER BY t) k, a FROM wm", 2),
        (
            "SELECT a, sum(a) OVER (PARTITION BY a % 7 ORDER BY b) s, count(*) OVER (ORDER BY t) c, \
             lag(t) OVER (PARTITION BY a % 7 ORDER BY b) l FROM wm",
            2,
        ),
        ("SELECT a, row_number() OVER (ORDER BY t) FROM (SELECT * FROM wm OFFSET 0) s", 1),
        ("SELECT a, rank() OVER (PARTITION BY a % 3 ORDER BY b), dense_rank() OVER (PARTITION BY a % 3 ORDER BY b, t) FROM wm", 1),
    ] {
        q(&mut s, "SET work_mem = '256MB'").unwrap();
        let want = q(&mut s, sql).unwrap();
        assert_eq!(want.len(), 20000);
        q(&mut s, "SET work_mem = '64kB'").unwrap();
        q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
        let f0 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
        assert!(q(&mut s, sql).unwrap() == want, "{sql}");
        assert_eq!(temp_files(dir.path()), 0);
        q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
        let f1 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
        assert_eq!(f1 - f0, files, "{sql}: the Sorts' files");
    }
    drop(s);
    eng.shutdown();
}

/// count / sum / avg (DISTINCT x) of a plain aggregation over a table larger than work_mem:
/// each aggregate's values sorted as PostgreSQL's per-aggregate sort sorts them (a temporary
/// file each past work_mem, PostgreSQL 18.6's count here), not kept in a hash set; the
/// results are those of the hash set's.
#[test]
fn distinct_aggregates_spill() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(distinct_aggregates).unwrap().join().unwrap();
}

fn distinct_aggregates() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE TABLE cd (a int, t text)",
        "INSERT INTO cd SELECT g % 7000, md5((g % 9000)::text) FROM generate_series(1, 20000) g",
        "ANALYZE cd",
    ] {
        q(&mut s, sql).unwrap();
    }
    for (sql, files) in [
        ("SELECT count(DISTINCT t), count(DISTINCT a) FROM cd", 2),
        ("SELECT count(DISTINCT t) FROM cd WHERE a > 10", 1),
    ] {
        q(&mut s, "SET work_mem = '256MB'").unwrap();
        let want = q(&mut s, sql).unwrap();
        q(&mut s, "SET work_mem = '64kB'").unwrap();
        q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
        let f0 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
        assert_eq!(q(&mut s, sql).unwrap(), want, "{sql}");
        assert_eq!(temp_files(dir.path()), 0);
        q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
        let f1 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
        assert_eq!(f1 - f0, files, "{sql}: the aggregates' sorts' files");
    }
    drop(s);
    eng.shutdown();
}

/// Grouping sets past work_mem: sorted ones as PostgreSQL's GroupAggregate runs them, a phase
/// per rollup over its input sorted (the Sort below, then each phase's sort of the rows for the
/// next: their files), hashed ones with every set's table fed as the input comes, no row kept.
/// The rows, in their order, and the temporary files are PostgreSQL 18.6's.
#[test]
fn grouping_sets_spill() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(grouping_sets).unwrap().join().unwrap();
}

fn grouping_sets() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "postgres").unwrap();
    for sql in [
        "CREATE TABLE gs (id int, a int, s text)",
        "INSERT INTO gs SELECT g, (g::int8 * 7919 % 20011)::int, md5(g::text) FROM generate_series(1, 20000) g",
        "ANALYZE gs",
    ] {
        q(&mut s, sql).unwrap();
    }
    let two = "SELECT a % 1000, id % 7, count(*), max(s) FROM gs GROUP BY GROUPING SETS ((a % 1000), (id % 7))";
    let mixed = "SELECT a % 100, count(*), sum(id) FROM gs GROUP BY GROUPING SETS ((a % 100), ())";
    for (work_mem, sql, want, files) in [
        ("64kB", two, "bc80177bd17f6bb7ea93189ef4399be4", 2),
        ("64kB", mixed, "bea3a42c21c4f54f36253118f21ba1b0", 1),
        ("1MB", mixed, "dff49e034896a6d474d59f57a0e35a3e", 0),
    ] {
        q(&mut s, &format!("SET work_mem = '{work_mem}'")).unwrap();
        q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
        let f0 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
        let got = one(&mut s, &format!("SELECT md5(string_agg(x::text, ',')) FROM ({sql}) x"));
        assert_eq!(got, Value::Text(want.into()), "{work_mem} {sql}");
        assert_eq!(temp_files(dir.path()), 0);
        q(&mut s, "SELECT pg_stat_force_next_flush()").unwrap();
        let f1 = int(&mut s, "SELECT temp_files FROM pg_stat_database WHERE datname = current_database()");
        assert_eq!(f1 - f0, files, "{work_mem} {sql}: temp files");
    }
    drop(s);
    eng.shutdown();
}
