//! The storage upgrade (see `storage_format.rs`) moves a table in transactions of a bounded
//! size, so a large table doesn't need its whole data in memory at once: a table that takes
//! many of them comes out as one moved at once would, and an upgrade stopped between any two
//! of them (an interrupted process) carries on where it stopped when the database is opened
//! again. (Its own test binary: the batch size and the stop are process-wide.)

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::time::Duration;

fn open(dir: &std::path::Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
    })
    .unwrap()
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last()
        .unwrap()
        .rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect()
}

/// Each statement on its own (VACUUM can't run in a multi-statement query).
fn run(s: &mut Session, sql: &str) {
    for stmt in sql.split(';').map(str::trim).filter(|q| !q.is_empty()) {
        s.query(stmt).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
}

fn err(s: &mut Session, sql: &str) -> String {
    match s.query(sql) {
        Ok(_) => panic!("{sql}: no error"),
        Err(e) => e.message.clone(),
    }
}

/// A few thousand rows (a couple of hundred kilobytes), written to so that their heap order
/// is not their key order, with indexes of several kinds; and a small table beside it.
const SETUP: &str = "
    CREATE TABLE big (id int PRIMARY KEY, v text, w int);
    CREATE INDEX big_v ON big (v);
    CREATE UNIQUE INDEX big_w ON big (w DESC);
    CREATE INDEX big_part ON big ((w % 7)) WHERE id % 2 = 0;
    INSERT INTO big SELECT g, repeat('x', g % 40) || g, g * 3 FROM generate_series(3000, 1, -1) g;
    UPDATE big SET v = 'u' || v WHERE id % 10 = 0;
    UPDATE big SET id = id + 10000 WHERE id % 97 = 0;
    DELETE FROM big WHERE id % 13 = 0;
    VACUUM big;
    INSERT INTO big VALUES (0, 'zero', NULL), (20000, 'last', 60000);
    CREATE TABLE small (k text PRIMARY KEY, n int);
    INSERT INTO small VALUES ('b', 2), ('a', 1), ('c', 3);
";

const READS: [&str; 8] = [
    "SELECT ctid, * FROM big",
    "SELECT ctid, * FROM small",
    "SELECT count(*), sum(id), sum(w) FROM big",
    "SET enable_seqscan = off; SET enable_bitmapscan = off; SELECT id, v FROM big WHERE id BETWEEN 1200 AND 1300 ORDER BY id",
    "SET enable_seqscan = off; SET enable_bitmapscan = off; SELECT id FROM big WHERE v = 'u' || repeat('x', 30) || '1230'",
    "SET enable_seqscan = off; SET enable_bitmapscan = off; SELECT w FROM big WHERE w < 600 ORDER BY w DESC",
    "SET enable_seqscan = off; SET enable_bitmapscan = off; SELECT id FROM big WHERE w % 7 = 3 AND id % 2 = 0 AND id < 400 ORDER BY id",
    "SET enable_seqscan = off; SET enable_bitmapscan = off; SELECT id FROM big WHERE id > 10000 ORDER BY id",
];

fn read_all(s: &mut Session) -> Vec<Vec<String>> {
    let out = READS.iter().map(|q| rows(s, q)).collect();
    s.query("RESET enable_seqscan; RESET enable_bitmapscan").unwrap();
    out
}

#[test]
fn large_table_rekeyed_in_batches() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let current = {
        let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
        s.query("CREATE DATABASE cur").unwrap();
        let mut c = engine.connect(2, "cur", "postgres", "t").unwrap();
        run(&mut c, SETUP);
        read_all(&mut c)
    };
    {
        let mut s = engine.connect(3, "postgres", "postgres", "t").unwrap();
        pg_engine::storage::keys::with_legacy_pk_storage(|| run(&mut s, SETUP));
        assert_eq!(engine.storage_format_for_test("postgres").unwrap(), (1, 2));
        assert_eq!(read_all(&mut s), current);
    }
    engine.downgrade_storage_for_test("postgres").unwrap();
    engine.shutdown();
    drop(engine);

    // Opened again with batches of a few kilobytes: stopped after every few, the next
    // session's upgrade goes on from there (through every step of the move).
    Engine::set_upgrade_batch_bytes_for_test(6 << 10);
    let engine = open(dir.path());
    let start = Engine::upgrade_batches_for_test(0);
    let mut stops = 0;
    let mut s = loop {
        let at = Engine::upgrade_batches_for_test(0);
        Engine::upgrade_batches_for_test(at + 7);
        match engine.connect(4, "postgres", "postgres", "t") {
            Ok(s) => break s,
            Err(e) => {
                assert!(e.message.contains("stopped for a test"), "{e:?}");
                stops += 1;
                assert!(stops < 200, "the upgrade doesn't finish");
            }
        }
    };
    Engine::upgrade_batches_for_test(0);
    let batches = Engine::upgrade_batches_for_test(0) - start;
    assert!(stops >= 5, "stopped {stops} times");
    assert!(batches > 20, "{batches} batches");
    assert_eq!(engine.storage_format_for_test("postgres").unwrap(), (1, 0));
    assert_eq!(read_all(&mut s), current);

    // The constraints hold, and new rows go after the old ones.
    assert!(err(&mut s, "INSERT INTO big VALUES (1, 'dup', -1)").contains("big_pkey"));
    assert!(err(&mut s, "INSERT INTO big VALUES (-1, 'dup', 60000)").contains("big_w"));
    assert!(err(&mut s, "INSERT INTO small VALUES ('a', 9)").contains("small_pkey"));
    s.query("INSERT INTO big VALUES (30000, 'new', 90000)").unwrap();
    assert_eq!(rows(&mut s, "SELECT id FROM big").last().unwrap(), "[Int4(30000)]");
    s.query("SET enable_seqscan = off; SET enable_bitmapscan = off").unwrap();
    assert_eq!(rows(&mut s, "SELECT v FROM big WHERE id = 30000"), ["[Text(\"new\")]"]);
    assert_eq!(rows(&mut s, "SELECT id FROM big WHERE w = 90000"), ["[Int4(30000)]"]);
}
