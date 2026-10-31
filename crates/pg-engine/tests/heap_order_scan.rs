//! Sequential scans of a modelled heap whose rows aren't stored in heap order (rows an update
//! moved, rows put in free space) stream them in heap order (`exec::heaporder`), holding only
//! the displaced rows: what they return is what `ORDER BY ctid` returns, in and out of
//! transactions, under LIMIT, counted, and while the scan feeds an UPDATE of its own table.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::exec::scan::{HEAP_ORDER_HELD, HEAP_ORDER_STREAMED};
use pg_engine::session::Session;
use std::sync::atomic::Ordering::Relaxed;
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
        auth_files: Default::default(),
        settings: vec![("autovacuum".into(), "off".into())],
    })
    .unwrap()
}

fn out(s: &mut Session, sql: &str) -> Vec<String> {
    match s.query(sql) {
        Ok(r) => r
            .last()
            .map(|q| q.rows.iter().map(|row| format!("{row:?}")).collect())
            .unwrap_or_default(),
        Err(e) => vec![format!("ERROR: {}", e.message)],
    }
}

fn same_as_ctid_order(s: &mut Session, what: &str) {
    for (q, oracle) in [
        ("SELECT a, b FROM t", "SELECT a, b FROM t ORDER BY ctid"),
        ("SELECT a FROM t WHERE b > 5", "SELECT a FROM t WHERE b > 5 ORDER BY ctid"),
        ("SELECT a, ctid FROM t LIMIT 37", "SELECT a, ctid FROM t ORDER BY ctid LIMIT 37"),
        ("SELECT count(*) FROM t WHERE b % 3 = 1", "SELECT count(*) FROM (SELECT b FROM t ORDER BY ctid) x WHERE b % 3 = 1"),
    ] {
        let (got, want) = (out(s, q), out(s, oracle));
        assert!(!want.is_empty(), "{what}: {oracle} returned nothing");
        assert_eq!(got, want, "{what}: {q}");
    }
}

#[test]
fn heap_order_scans_stream_displaced_rows() {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            pg_engine::stack::set_limit(60 << 20);
            run();
        })
        .unwrap()
        .join()
        .unwrap();
}

fn run() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    s.query("SET max_parallel_workers_per_gather = 0").unwrap();
    s.query("CREATE TABLE t (a int PRIMARY KEY, b int, pad text)").unwrap();
    s.query("INSERT INTO t SELECT i, i % 11, repeat('x', 40) FROM generate_series(1, 3000) i").unwrap();
    // A key range moved to the end, scattered rows moved, rows deleted and their space
    // reused by new rows of greater keys.
    s.query("UPDATE t SET b = b + 1 WHERE a <= 300").unwrap();
    s.query("UPDATE t SET b = b + 2 WHERE a % 97 = 0").unwrap();
    s.query("DELETE FROM t WHERE a % 50 = 1").unwrap();
    s.query("VACUUM t").unwrap();
    s.query("INSERT INTO t SELECT i, i % 7, 'y' FROM generate_series(3001, 3040) i").unwrap();

    let (streamed, held) = (HEAP_ORDER_STREAMED.load(Relaxed), HEAP_ORDER_HELD.load(Relaxed));
    same_as_ctid_order(&mut s, "committed");
    assert!(HEAP_ORDER_STREAMED.load(Relaxed) > streamed, "the scans streamed");
    let per_scan = (HEAP_ORDER_HELD.load(Relaxed) - held) / (HEAP_ORDER_STREAMED.load(Relaxed) - streamed);
    assert!(per_scan < 1000, "only the displaced rows held: {per_scan} of ~2,980");

    // The transaction's own writes, and a scan feeding an UPDATE of its own table.
    s.query("BEGIN").unwrap();
    s.query("UPDATE t SET b = b + 1 WHERE a % 10 = 3").unwrap();
    s.query("INSERT INTO t VALUES (5000, 1, 'z')").unwrap();
    same_as_ctid_order(&mut s, "own writes");
    let before = out(&mut s, "SELECT count(*), sum(b)::bigint FROM t");
    s.query("UPDATE t SET b = b + 1").unwrap();
    let after = out(&mut s, "SELECT count(*), (sum(b) - count(*))::bigint FROM t");
    assert_eq!(before.len(), 1);
    assert_eq!(before, after, "each row updated once");
    same_as_ctid_order(&mut s, "after updating every row");
    s.query("ROLLBACK").unwrap();
    same_as_ctid_order(&mut s, "rolled back");
}
