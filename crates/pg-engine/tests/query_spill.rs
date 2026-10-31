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

const QUERIES: &[&str] = &[
    "SELECT md5(string_agg(a::text, ',')) FROM (SELECT a FROM sp ORDER BY t, a) s",
    "SELECT md5(string_agg(a::text, ',')) FROM (SELECT a FROM sp ORDER BY n, a DESC) s",
    "SELECT md5(string_agg(b || ':' || c, ',' ORDER BY b)) FROM (SELECT b, count(*) c FROM sp GROUP BY b) s",
    "SELECT md5(string_agg(t || ':' || c || ':' || s, ',' ORDER BY t)) FROM (SELECT t, count(*) c, sum(a) s FROM sp GROUP BY t) s",
    "SELECT count(*), sum(c) FROM (SELECT a, count(*) c FROM sp GROUP BY a) s",
];

#[test]
fn spills_match_memory_and_clean_up() {
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
