//! Indexes whose NULLS FIRST / LAST isn't their direction's default changed their key
//! encoding (index key format 1): a database from before is brought up to date when it is
//! first opened, its affected indexes rebuilt.

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
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 0,
    })
    .unwrap()
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap();
    r.last()
        .unwrap()
        .rows
        .iter()
        .map(|row| format!("{:?}", row[0]))
        .collect()
}

const ORDERED: [(&str, &str); 2] = [
    ("SELECT f1 FROM t ORDER BY f1 DESC NULLS LAST", "desc"),
    ("SELECT f1 FROM t ORDER BY f1 NULLS FIRST", "asc"),
];

fn ordered(s: &mut Session) -> Vec<Vec<String>> {
    s.query("SET enable_seqscan = off; SET enable_sort = off; SET enable_bitmapscan = off")
        .unwrap();
    ORDERED.iter().map(|(q, _)| rows(s, q)).collect()
}

#[test]
fn null_ordering_index_keys_are_upgraded() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let expected = {
        let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
        s.query(
            "CREATE TABLE t (f1 int); \
             INSERT INTO t VALUES (42), (3), (NULL), (10), (NULL), (1); \
             CREATE INDEX t_desc ON t (f1 DESC NULLS LAST); \
             CREATE INDEX t_asc ON t (f1 ASC NULLS FIRST)",
        )
        .unwrap();
        // The scans use the indexes.
        let plan = rows(&mut s, "SET enable_seqscan = off; SET enable_sort = off; EXPLAIN (COSTS OFF) SELECT f1 FROM t ORDER BY f1 DESC NULLS LAST");
        assert!(plan.iter().any(|l| l.contains("Index Only Scan")), "{plan:?}");
        ordered(&mut s)
    };
    assert_eq!(expected[0].last().unwrap(), "Null");
    assert_eq!(expected[1].first().unwrap(), "Null");
    assert_eq!(engine.index_key_format_for_test("postgres").unwrap(), pg_engine::storage::keys::INDEX_KEY_FORMAT);

    // As a database written by a build from before the format: legacy keys, format 0.
    engine.downgrade_index_keys_for_test("postgres").unwrap();
    assert_eq!(engine.index_key_format_for_test("postgres").unwrap(), 0);
    engine.shutdown();
    drop(engine);

    // Opened again: the first session upgrades it.
    let engine = open(dir.path());
    let mut s = engine.connect(2, "postgres", "postgres", "t").unwrap();
    assert_eq!(engine.index_key_format_for_test("postgres").unwrap(), pg_engine::storage::keys::INDEX_KEY_FORMAT);
    assert_eq!(ordered(&mut s), expected);
    // Writes after the upgrade keep the order.
    s.query("INSERT INTO t VALUES (NULL), (7)").unwrap();
    let after = ordered(&mut s);
    assert_eq!(after[0].last().unwrap(), "Null");
    assert_eq!(after[1].first().unwrap(), "Null");
    assert_eq!(after[0].len(), 8);
}

#[test]
fn legacy_keys_misorder_nulls() {
    // The legacy encoding (what the upgrade repairs) puts the NULLs on the wrong end.
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    pg_engine::storage::keys::with_legacy_index_keys(|| {
        s.query(
            "CREATE TABLE t (f1 int); \
             INSERT INTO t VALUES (42), (NULL), (1); \
             CREATE INDEX t_desc ON t (f1 DESC NULLS LAST)",
        )
        .unwrap();
    });
    s.query("SET enable_seqscan = off; SET enable_sort = off; SET enable_bitmapscan = off")
        .unwrap();
    let r = rows(&mut s, "SELECT f1 FROM t ORDER BY f1 DESC NULLS LAST");
    assert_eq!(r.first().unwrap(), "Null", "{r:?}");
}

/// Format 2: an index on a column under a collation that doesn't sort by bytes (ICU) is
/// keyed by its sort keys, in the collation's order; one written in byte order is rebuilt.
#[test]
fn collated_index_keys_are_upgraded() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let q = "SELECT s FROM c ORDER BY s";
    let unique = "INSERT INTO n VALUES ('ABC')";
    let off = "SET enable_seqscan = off; SET enable_sort = off; SET enable_bitmapscan = off";
    let expected = {
        let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
        s.query(
            "CREATE TABLE c (s text COLLATE \"en-x-icu\"); \
             INSERT INTO c VALUES ('b'), ('A'), ('a'), ('B'), ('_'), ('é'), ('e'); \
             CREATE INDEX c_s ON c (s); \
             CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false); \
             CREATE TABLE n (s text COLLATE ci UNIQUE); \
             INSERT INTO n VALUES ('abc')",
        )
        .unwrap();
        s.query(off).unwrap();
        let plan = rows(&mut s, &format!("EXPLAIN (COSTS OFF) {q}"));
        assert!(plan.iter().any(|l| l.contains("Index Only Scan")), "{plan:?}");
        let r = rows(&mut s, q);
        assert_eq!(
            r.join(" "),
            r#"Text("_") Text("a") Text("A") Text("b") Text("B") Text("e") Text("é")"#
        );
        assert!(s.query(unique).is_err());
        r
    };
    // Written by a build from before: byte-order keys, format 0.
    engine.downgrade_index_keys_for_test("postgres").unwrap();
    engine.shutdown();
    drop(engine);
    let engine = open(dir.path());
    let mut s = engine.connect(2, "postgres", "postgres", "t").unwrap();
    assert_eq!(
        engine.index_key_format_for_test("postgres").unwrap(),
        pg_engine::storage::keys::INDEX_KEY_FORMAT
    );
    s.query(off).unwrap();
    assert_eq!(rows(&mut s, q), expected);
    assert!(s.query(unique).is_err());
}
