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
        auth_files: Default::default(),
        settings: Vec::new(),
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
    // TRUNCATE and DROP TABLE (range deletions): the transaction's own writes before them,
    // savepoints around them, writes after them.
    "BEGIN",
    "INSERT INTO t SELECT g, 'w', 1 FROM generate_series(40000, 42000) g",
    "SAVEPOINT tr",
    "TRUNCATE t",
    "SELECT count(*) FROM t",
    "INSERT INTO t SELECT g, 'after', 2 FROM generate_series(1, 1500) g",
    "SET enable_seqscan = off",
    "SELECT count(*) FROM t WHERE n = 2",
    "SELECT id, v FROM t WHERE id = 7",
    "RESET enable_seqscan",
    "ROLLBACK TO tr",
    "SELECT count(*), sum(n) FROM t",
    "TRUNCATE t",
    "INSERT INTO t SELECT g, 'kept', g % 5 FROM generate_series(1, 2500) g",
    "SELECT count(*), sum(id) FROM t",
    "SELECT id, v FROM t ORDER BY id DESC LIMIT 2",
    "COMMIT",
    "SELECT count(*), sum(id), sum(n) FROM t",
    "CREATE TABLE t2 AS SELECT g AS id FROM generate_series(1, 3000) g",
    "BEGIN",
    "DROP TABLE t2",
    "ROLLBACK",
    "SELECT count(*) FROM t2",
    "DROP TABLE t2",
    "CREATE TABLE t2 (id int)",
    "INSERT INTO t2 VALUES (1)",
    "SELECT count(*) FROM t2",
    // New rows of a table without indexes, and an index build: their runs are linked into
    // the commits as they are.
    "CREATE TABLE b (id int, v text)",
    "INSERT INTO b SELECT g, repeat('b', 40) || g FROM generate_series(1, 5000) g",
    "CREATE INDEX b_v ON b (v)",
    "SET enable_seqscan = off",
    "SELECT id FROM b WHERE v = repeat('b', 40) || 4321",
    "RESET enable_seqscan",
    "SELECT count(*), sum(id), sum(length(v)) FROM b",
    // Values stored out of line go with their rows' range.
    "CREATE TABLE tv (id int, v text)",
    "INSERT INTO tv SELECT g, repeat(md5(g::text), 300) FROM generate_series(1, 40) g",
    "BEGIN",
    "TRUNCATE tv",
    "INSERT INTO tv VALUES (1, repeat('z', 9000))",
    "SELECT count(*), sum(length(v)) FROM tv",
    "ROLLBACK",
    "SELECT count(*), sum(length(v)), min(left(v, 3)) FROM tv",
    "TRUNCATE tv",
    "INSERT INTO tv VALUES (2, repeat('y', 9000))",
    "SELECT count(*), sum(length(v)), min(left(v, 3)) FROM tv",
    // A small table scanned whole again and again (a hot range: `storage::hot`), truncated
    // in a transaction and for good.
    "CREATE TABLE h (id int, v int)",
    "INSERT INTO h SELECT g, g FROM generate_series(1, 10) g",
    "SELECT sum(v) FROM h", "SELECT sum(v) FROM h", "SELECT sum(v) FROM h", "SELECT sum(v) FROM h",
    "SELECT sum(v) FROM h", "SELECT sum(v) FROM h", "SELECT sum(v) FROM h", "SELECT sum(v) FROM h",
    "SELECT sum(v) FROM h", "SELECT sum(v) FROM h",
    "UPDATE h SET v = v + 1 WHERE id = 1",
    "BEGIN",
    "TRUNCATE h",
    "SELECT count(*) AS truncated_here FROM h",
    "INSERT INTO h VALUES (11, 100)",
    "SELECT sum(v) AS after_insert FROM h",
    "ROLLBACK",
    "SELECT sum(v) AS rolled_back FROM h",
    "TRUNCATE h",
    "SELECT count(*) AS truncated FROM h",
    "INSERT INTO h VALUES (12, 7)",
    "SELECT sum(v) AS refilled FROM h", "SELECT sum(v) AS refilled FROM h",
];

const AFTER_REOPEN: &[&str] = &[
    "SELECT count(*), sum(id), sum(length(v)), sum(n) FROM t",
    "SELECT count(*) FROM t WHERE n = 3",
    "SELECT count(*) FROM t2",
    "SELECT count(*), sum(id), sum(length(v)) FROM b",
    "SET enable_seqscan = off",
    "SELECT id, v FROM b WHERE v = repeat('b', 40) || 77",
    "SELECT count(*), sum(length(v)), min(left(v, 3)) FROM tv",
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
    let linked = pg_engine::storage::LINKED_RUNS.load(Ordering::Relaxed);
    let spilled = script_on(a.path(), 2048);
    let spills = TXN_SPILLS.load(Ordering::Relaxed) - before;
    // (Runs of new rows and of index builds go into their commits as they are.)
    let linked = pg_engine::storage::LINKED_RUNS.load(Ordering::Relaxed) - linked;
    assert!(linked > 10, "{linked} runs linked");
    let buffered = script_on(b.path(), 0);
    assert!(spills > 100, "only {spills} spills");
    for (x, y) in spilled.iter().zip(&buffered) {
        assert_eq!(x, y);
    }
    assert_eq!(spilled.len(), buffered.len());
    let line = |q: &str| spilled.iter().find(|l| l.starts_with(&format!("{q} =>"))).unwrap().clone();
    assert!(line("SELECT count(*) FROM t WHERE n = 2").contains("Int8(1500)"));
    assert!(line("SELECT count(*), sum(id), sum(n) FROM t").contains("[Int8(2500), Int8(3126250), Int8(5000)]"));
    assert!(line("SELECT count(*) FROM t2").contains("Int8(3000)"));
    assert!(line("SELECT id FROM b WHERE v = repeat('b', 40) || 4321").contains("Int4(4321)"));
    assert!(line("SELECT count(*), sum(length(v)), min(left(v, 3)) FROM tv").contains("[Int8(40), Int8(384000)"));
    assert!(line("SELECT count(*) AS truncated_here FROM h").contains("Int8(0)"));
    assert!(line("SELECT sum(v) AS after_insert FROM h").contains("Int8(100)"));
    assert!(line("SELECT sum(v) AS rolled_back FROM h").contains("Int8(56)"));
    assert!(line("SELECT count(*) AS truncated FROM h").contains("Int8(0)"));
    assert!(line("SELECT sum(v) AS refilled FROM h").contains("Int8(7)"));
    let after = &spilled[spilled.len() - 2];
    assert!(after.contains("Int4(77)"), "{after}");
    assert!(spilled.last().unwrap().contains("[Int8(1), Int8(9000), Text(\"yyy\")]"), "{:?}", spilled.last());
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
