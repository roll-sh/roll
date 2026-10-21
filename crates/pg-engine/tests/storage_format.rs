//! Tables with a primary key used to store their rows keyed by the key's values (storage
//! format 0), a deferrable key's too, which could overwrite rows whose keys clashed until
//! the key was checked. Since format 1 every table stores its rows under a hidden row id
//! in heap order, the primary key an index of its own: a database from before is brought
//! up to date when it is first opened, its rows, order, ctids and constraints kept.

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

/// Run each statement of `sql` on its own (VACUUM can't run in a multi-statement query).
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

/// Tables of every kind of primary key, written to with inserts, updates (of the key and
/// not), deletes and VACUUM so their heap order is not their key order.
const SETUP: &str = "
    CREATE TABLE pk (id int PRIMARY KEY, v text, w int);
    CREATE INDEX pk_v ON pk (v);
    CREATE UNIQUE INDEX pk_w ON pk (w DESC NULLS LAST);
    INSERT INTO pk SELECT g, 'v' || (g % 7), g * 10 FROM generate_series(20, 1, -1) g;
    UPDATE pk SET v = 'upd' WHERE id IN (3, 17);
    UPDATE pk SET id = id + 100 WHERE id IN (5, 6);
    DELETE FROM pk WHERE id IN (10, 11);
    VACUUM pk;
    INSERT INTO pk VALUES (0, 'zero', NULL), (50, 'fifty', 500);
    CREATE TABLE fk (a int REFERENCES pk ON DELETE CASCADE, b text);
    INSERT INTO fk VALUES (1, 'x'), (105, 'y'), (20, 'z');
    CREATE TABLE multi (a text, b int, c date, PRIMARY KEY (b, a));
    INSERT INTO multi VALUES ('q', 2, '2020-01-01'), ('a', 2, NULL), ('m', 1, '2021-05-05');
    UPDATE multi SET b = 0 WHERE a = 'q';
    CREATE TABLE descpk (a int NOT NULL, b text);
    CREATE UNIQUE INDEX descpk_a ON descpk (a) INCLUDE (b);
    ALTER TABLE descpk ADD PRIMARY KEY USING INDEX descpk_a;
    INSERT INTO descpk VALUES (3, 'c'), (1, 'a'), (2, 'b');
    CREATE TABLE def (id int PRIMARY KEY DEFERRABLE INITIALLY DEFERRED, v text);
    INSERT INTO def VALUES (2, 'two'), (1, 'one'), (3, 'three');
    CREATE TABLE parted (k int, v text, PRIMARY KEY (k)) PARTITION BY RANGE (k);
    CREATE TABLE parted_1 PARTITION OF parted FOR VALUES FROM (0) TO (10);
    CREATE TABLE parted_2 PARTITION OF parted FOR VALUES FROM (10) TO (100);
    INSERT INTO parted VALUES (15, 'p'), (5, 'q'), (1, 'r'), (12, 's');
    UPDATE parted SET v = v || '!' WHERE k IN (1, 15);
    CREATE TABLE nopk (a int, b text);
    INSERT INTO nopk VALUES (2, 'b'), (1, 'a');
";

const READS: [&str; 12] = [
    "SELECT ctid, * FROM pk",
    "SELECT ctid, * FROM fk",
    "SELECT ctid, * FROM multi",
    "SELECT ctid, * FROM descpk",
    "SELECT ctid, * FROM def",
    "SELECT tableoid::regclass::text, ctid, * FROM parted",
    "SELECT ctid, * FROM nopk",
    "SET enable_seqscan = off; SET enable_bitmapscan = off; SELECT id, v FROM pk WHERE id = 105",
    "SET enable_seqscan = off; SET enable_bitmapscan = off; SELECT id FROM pk WHERE id BETWEEN 2 AND 9 ORDER BY id",
    "SET enable_seqscan = off; SET enable_bitmapscan = off; SELECT id, v FROM pk WHERE v = 'upd' ORDER BY id",
    "SET enable_seqscan = off; SET enable_bitmapscan = off; SELECT w FROM pk ORDER BY w DESC NULLS LAST",
    "SET enable_seqscan = off; SET enable_bitmapscan = off; SELECT a FROM descpk ORDER BY a DESC",
];

fn read_all(s: &mut Session) -> Vec<Vec<String>> {
    let out = READS.iter().map(|q| rows(s, q)).collect();
    s.query("RESET enable_seqscan; RESET enable_bitmapscan").unwrap();
    out
}

#[test]
fn primary_keyed_tables_are_rekeyed() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    // The same tables in the current layout, to compare with.
    let current = {
        let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
        s.query("CREATE DATABASE cur").unwrap();
        let mut c = engine.connect(2, "cur", "postgres", "t").unwrap();
        run(&mut c, SETUP);
        assert_eq!(engine.storage_format_for_test("cur").unwrap(), (1, 0));
        read_all(&mut c)
    };
    let before = {
        let mut s = engine.connect(3, "postgres", "postgres", "t").unwrap();
        pg_engine::storage::keys::with_legacy_pk_storage(|| run(&mut s, SETUP));
        // pk, multi, descpk, def, parted and its two partitions.
        assert_eq!(engine.storage_format_for_test("postgres").unwrap(), (1, 7));
        // Rows keyed by their primary key come out in heap order all the same.
        let ids = rows(&mut s, "SELECT id FROM pk");
        let mut sorted = ids.clone();
        sorted.sort_by_key(|r| r.trim_start_matches("[Int4(").trim_end_matches(")]").parse::<i32>().unwrap());
        assert_ne!(ids, sorted);
        read_all(&mut s)
    };
    assert_eq!(before, current);

    // As a database written by a build from before the format.
    engine.downgrade_storage_for_test("postgres").unwrap();
    assert_eq!(engine.storage_format_for_test("postgres").unwrap(), (0, 7));
    engine.shutdown();
    drop(engine);

    // Opened again: the first session upgrades it.
    let engine = open(dir.path());
    let mut s = engine.connect(4, "postgres", "postgres", "t").unwrap();
    assert_eq!(engine.storage_format_for_test("postgres").unwrap(), (1, 0));
    assert_eq!(read_all(&mut s), before);

    // The constraints hold.
    assert!(err(&mut s, "INSERT INTO pk VALUES (105, 'dup', 1)").contains("pk_pkey"));
    assert!(err(&mut s, "INSERT INTO pk VALUES (999, 'dup', 200)").contains("pk_w"));
    assert!(err(&mut s, "INSERT INTO multi VALUES ('a', 2, NULL)").contains("multi_pkey"));
    assert!(err(&mut s, "INSERT INTO descpk VALUES (2, 'x')").contains("descpk_a"));
    assert!(err(&mut s, "INSERT INTO fk VALUES (10, 'gone')").contains("fk_a_fkey"));
    assert!(err(&mut s, "INSERT INTO parted VALUES (5, 'dup')").contains("parted_1_pkey"));
    assert!(err(&mut s, "UPDATE pk SET id = 1 WHERE id = 2").contains("pk_pkey"));
    s.query("DELETE FROM pk WHERE id = 1").unwrap();
    assert_eq!(rows(&mut s, "SELECT a, b FROM fk ORDER BY b").len(), 2);

    // A deferrable key's values may clash until it is checked, without losing rows.
    s.query("BEGIN; INSERT INTO def VALUES (2, 'again'); UPDATE def SET id = 4 WHERE v = 'again'; COMMIT")
        .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT id, v FROM def ORDER BY id"),
        ["[Int4(1), Text(\"one\")]", "[Int4(2), Text(\"two\")]", "[Int4(3), Text(\"three\")]", "[Int4(4), Text(\"again\")]"]
    );
    assert!(err(&mut s, "BEGIN; INSERT INTO def VALUES (3, 'x'); COMMIT").contains("def_pkey"));
    s.query("ROLLBACK").ok();

    // New rows go after the old ones, under new row ids.
    s.query("INSERT INTO pk VALUES (7000, 'new', 7)").unwrap();
    let after = rows(&mut s, "SELECT id FROM pk");
    assert_eq!(after.last().unwrap(), "[Int4(7000)]");
    assert_eq!(rows(&mut s, "SELECT count(*) FROM pk"), ["[Int8(20)]"]);
    s.query("SET enable_seqscan = off; SET enable_bitmapscan = off").unwrap();
    assert_eq!(rows(&mut s, "SELECT v FROM pk WHERE id = 7000"), ["[Text(\"new\")]"]);
    assert_eq!(rows(&mut s, "SELECT id FROM pk WHERE v = 'new'"), ["[Int4(7000)]"]);
}

#[test]
fn legacy_deferrable_key_overwrites() {
    // The legacy layout (what the upgrade repairs) loses a row whose deferrable key
    // clashed until the key was checked.
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    pg_engine::storage::keys::with_legacy_pk_storage(|| {
        s.query("CREATE TABLE def (id int PRIMARY KEY DEFERRABLE INITIALLY DEFERRED, v text)")
            .unwrap();
    });
    s.query("INSERT INTO def VALUES (1, 'one'); BEGIN; INSERT INTO def VALUES (1, 'again'); UPDATE def SET id = 2 WHERE v = 'again'; COMMIT")
        .unwrap();
    assert_eq!(rows(&mut s, "SELECT count(*) FROM def"), ["[Int8(1)]"]);
    // In the current layout both rows stay.
    s.query("CREATE TABLE def2 (id int PRIMARY KEY DEFERRABLE INITIALLY DEFERRED, v text)")
        .unwrap();
    s.query("INSERT INTO def2 VALUES (1, 'one'); BEGIN; INSERT INTO def2 VALUES (1, 'again'); UPDATE def2 SET id = 2 WHERE v = 'again'; COMMIT")
        .unwrap();
    assert_eq!(rows(&mut s, "SELECT count(*) FROM def2"), ["[Int8(2)]"]);
}
