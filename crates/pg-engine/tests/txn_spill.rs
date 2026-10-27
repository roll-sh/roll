//! Transactions bigger than their write buffer (`txn::spill`): past `txn_spill_bytes` the
//! buffered writes go to private runs on disk. The same scripts run with a buffer of a few kB
//! (a spill every few writes) and with the default one must give the same results:
//! reads of the transaction's own writes, statement snapshots and cursors from before a
//! spill, savepoints rolled back past one, unique checks, reverse scans, commits and
//! rollbacks, and the data after the database reopens.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use pg_engine::storage::set_default_txn_spill_bytes;
use pg_engine::txn::TXN_SPILLS;
use std::sync::atomic::Ordering;
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
        shared_preload_libraries: Vec::new(),
        outbound: Vec::new(),
        locale: Default::default(),
        max_prepared_transactions: 0,
    })
    .unwrap()
}

/// Each statement's rows, or its error.
fn run(s: &mut Session, script: &[&str]) -> Vec<String> {
    script
        .iter()
        .map(|q| match s.query(q) {
            Ok(r) => format!("{q} => {:?}", r.last().map(|r| (&r.tag, &r.rows))),
            Err(e) => format!("{q} => ERROR {} {}", e.code, e.message),
        })
        .collect()
}

const SCRIPT: &[&str] = &[
    "CREATE TABLE t (id int PRIMARY KEY, v text, n int)",
    "CREATE INDEX t_n ON t (n)",
    "BEGIN",
    "INSERT INTO t SELECT g, 'v' || g, g % 97 FROM generate_series(1, 3000) g",
    "SELECT count(*), sum(id), min(v), max(v) FROM t",
    "SELECT * FROM t WHERE id = 1234",
    "SELECT count(*) FROM t WHERE n = 5",
    "DECLARE c CURSOR FOR SELECT id, v FROM t ORDER BY id",
    "FETCH 3 FROM c",
    "UPDATE t SET v = v || 'x', n = n + 1 WHERE id % 3 = 0",
    "FETCH 3 FROM c",
    "SELECT count(*) FROM t WHERE v LIKE '%x'",
    "SAVEPOINT s1",
    "DELETE FROM t WHERE id > 1500",
    "INSERT INTO t SELECT g, 'new', 0 FROM generate_series(5000, 7000) g",
    "SELECT count(*), max(id) FROM t",
    "ROLLBACK TO s1",
    "SELECT count(*), max(id), sum(n) FROM t",
    "SAVEPOINT s2",
    "UPDATE t SET v = 'y' WHERE id < 10",
    "RELEASE s2",
    "SAVEPOINT s3",
    "INSERT INTO t VALUES (5, 'dup', 0)",
    "ROLLBACK TO s3",
    "SELECT id, v FROM t ORDER BY id DESC LIMIT 4",
    "SELECT id, v FROM t WHERE id BETWEEN 1 AND 12 ORDER BY id",
    "FETCH ALL FROM c",
    "CLOSE c",
    "SELECT count(*), sum(length(v)) FROM t",
    "COMMIT",
    "SELECT count(*), sum(length(v)), sum(n) FROM t",
    // A savepoint before a big subtransaction, rolled back.
    "BEGIN",
    "UPDATE t SET n = -1 WHERE id <= 10",
    "SAVEPOINT a",
    "UPDATE t SET v = 'gone'",
    "INSERT INTO t SELECT g, 'z', 1 FROM generate_series(10000, 12000) g",
    "SELECT count(*) FROM t WHERE v = 'gone'",
    "ROLLBACK TO a",
    "SELECT count(*), sum(n) FROM t",
    "SELECT count(*) FROM t WHERE v = 'gone'",
    "COMMIT",
    "SELECT count(*), sum(n) FROM t",
    // A whole big transaction rolled back.
    "BEGIN",
    "DELETE FROM t",
    "SELECT count(*) FROM t",
    "ROLLBACK",
    "SELECT count(*) FROM t",
    // Snapshot isolation, an index built after a spill, and a big delete.
    "BEGIN ISOLATION LEVEL REPEATABLE READ",
    "INSERT INTO t SELECT g, 'r' || g, g % 7 FROM generate_series(20000, 23000) g",
    "CREATE INDEX t_v ON t (v)",
    "SET enable_seqscan = off",
    "SELECT id FROM t WHERE v = 'r20500'",
    "RESET enable_seqscan",
    "DELETE FROM t WHERE id % 2 = 0",
    "SELECT count(*) FROM t",
    "COMMIT",
    "SELECT count(*), sum(id) FROM t",
    // A unique violation of a key written before a spill fails the statement.
    "BEGIN",
    "INSERT INTO t SELECT g, 'q', 0 FROM generate_series(30000, 32000) g",
    "INSERT INTO t VALUES (30001, 'dup', 0)",
    "ROLLBACK",
    "SELECT count(*) FROM t WHERE id >= 30000",
];

const AFTER_REOPEN: &[&str] = &[
    "SELECT count(*), sum(id), sum(length(v)), sum(n) FROM t",
    "SELECT count(*) FROM t WHERE n = 3",
];

fn script_on(dir: &std::path::Path, spill: usize) -> Vec<String> {
    set_default_txn_spill_bytes(spill);
    let mut out = {
        let eng = open(dir);
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        let out = run(&mut s, SCRIPT);
        drop(s);
        eng.shutdown();
        out
    };
    let eng = open(dir);
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    out.extend(run(&mut s, AFTER_REOPEN));
    drop(s);
    eng.shutdown();
    set_default_txn_spill_bytes(0);
    out
}

#[test]
fn spilled_transactions_read_like_buffered_ones() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let before = TXN_SPILLS.load(Ordering::Relaxed);
    let spilled = script_on(a.path(), 2048);
    let spills = TXN_SPILLS.load(Ordering::Relaxed) - before;
    let buffered = script_on(b.path(), 0);
    assert!(spills > 100, "only {spills} spills");
    for (x, y) in spilled.iter().zip(&buffered) {
        assert_eq!(x, y);
    }
    assert_eq!(spilled.len(), buffered.len());
    // Only the two duplicate keys fail.
    let errors: Vec<_> = spilled.iter().filter(|l| l.contains("=> ERROR")).collect();
    assert_eq!(errors.len(), 2, "{errors:#?}");
    assert!(errors.iter().all(|e| e.contains("23505")), "{errors:#?}");
    // Nothing is left of the runs.
    for d in std::fs::read_dir(a.path().join("base")).unwrap() {
        let spill = d.unwrap().path().join("spill");
        let left = std::fs::read_dir(&spill).map(|d| d.count()).unwrap_or(0);
        assert_eq!(left, 0, "{}", spill.display());
    }
}
