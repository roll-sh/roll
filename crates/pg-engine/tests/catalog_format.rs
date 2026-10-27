//! Catalog format 2 (round 19, agent 76): comments are a record per object instead of a list
//! in the catalog header. A database stored in format 1 is rewritten when it is first opened,
//! its comments unchanged and in pg_description's order; `Engine::downgrade_catalogs`
//! (`roll-engine --downgrade-catalog`) stores it as format 1 again; and a DDL commit writes
//! the comments of the objects it changed, not every comment.

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
        outbound: Vec::new(),
        max_prepared_transactions: 0,
        server_programs: false,
    })
    .unwrap()
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last().unwrap().rows.iter().map(|row| format!("{row:?}")).collect()
}

fn run(s: &mut Session, sql: &str) {
    for stmt in sql.split(";\n").map(str::trim).filter(|q| !q.is_empty()) {
        s.query(stmt).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
}

/// Comments of every kind: tables, columns, constraints, indexes, functions, a role (shared),
/// a built-in object's hidden by an empty one, set again (a new row at the end), removed.
const SETUP: &str = "
    CREATE ROLE cr1;
    CREATE TABLE t1 (id int PRIMARY KEY, a text CHECK (a <> ''));
    CREATE TABLE t2 (id int);
    CREATE INDEX t2_id ON t2 (id);
    CREATE FUNCTION f1() RETURNS int LANGUAGE sql AS 'SELECT 1';
    COMMENT ON TABLE t1 IS 'one';
    COMMENT ON COLUMN t1.a IS 'col a';
    COMMENT ON TABLE t2 IS 'two';
    COMMENT ON INDEX t2_id IS 'index';
    COMMENT ON FUNCTION f1() IS 'fn';
    COMMENT ON ROLE cr1 IS 'a role';
    COMMENT ON FUNCTION pg_catalog.now() IS '';
    COMMENT ON TABLE t1 IS 'one again';
    COMMENT ON CONSTRAINT t1_a_check ON t1 IS 'check';
    COMMENT ON TABLE t2 IS NULL";

/// What reads comments: pg_description's user rows in their order, the description
/// functions, pg_shdescription.
fn comments(s: &mut Session) -> Vec<Vec<String>> {
    vec![
        rows(s, "SELECT objoid::regclass::text, objsubid, description FROM pg_description WHERE objoid >= 16384 AND classoid = 'pg_class'::regclass"),
        rows(s, "SELECT classoid::regclass, objsubid, description FROM pg_description WHERE objoid >= 16384"),
        rows(s, "SELECT obj_description('t1'::regclass, 'pg_class'), col_description('t1'::regclass, 2), obj_description('t2'::regclass), obj_description('now()'::regprocedure::oid, 'pg_proc')"),
        rows(s, "SELECT description FROM pg_shdescription d JOIN pg_roles r ON r.oid = d.objoid WHERE r.rolname = 'cr1'"),
        rows(s, "SELECT count(*) FROM pg_description WHERE objoid = 'now()'::regprocedure"),
    ]
}

#[test]
fn comments_upgrade_from_header_and_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let expected = {
        let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, SETUP);
        comments(&mut s)
    };
    assert_eq!(expected[0], [r#"[Text("t1"), Int4(2), Text("col a")]"#, r#"[Text("t2_id"), Int4(0), Text("index")]"#, r#"[Text("t1"), Int4(0), Text("one again")]"#]);
    assert_eq!(expected[4], ["[Int8(0)]"]);
    let records = engine.catalog_records_for_test("postgres").unwrap();
    // Stored as format 1 had it: the comments in the header, none as records.
    assert!(engine.downgrade_catalogs().unwrap().iter().any(|d| d == "postgres"));
    let downgraded = engine.catalog_records_for_test("postgres").unwrap();
    assert!(downgraded < records, "{downgraded} records after the downgrade, {records} before");
    engine.shutdown();
    drop(engine);

    // Opened again: rewritten in format 2, comments alike and in their order.
    let engine = open(dir.path());
    let mut s = engine.connect(2, "postgres", "postgres", "t").unwrap();
    assert_eq!(comments(&mut s), expected);
    assert_eq!(engine.catalog_records_for_test("postgres").unwrap(), records);
    // A comment set after the upgrade is a new row at the end; one removed goes.
    run(&mut s, "COMMENT ON TABLE t2 IS 'two again';\nCOMMENT ON COLUMN t1.a IS NULL");
    assert_eq!(
        rows(&mut s, "SELECT objoid::regclass::text, objsubid, description FROM pg_description WHERE objoid >= 16384 AND classoid = 'pg_class'::regclass"),
        [r#"[Text("t2_id"), Int4(0), Text("index")]"#, r#"[Text("t1"), Int4(0), Text("one again")]"#, r#"[Text("t2"), Int4(0), Text("two again")]"#]
    );
    let after = comments(&mut s);
    engine.shutdown();
    drop(engine);
    let engine = open(dir.path());
    let mut s = engine.connect(3, "postgres", "postgres", "t").unwrap();
    assert_eq!(comments(&mut s), after);
    engine.shutdown();
}

/// A COMMENT's commit writes that object's record (and the header), whatever the number of
/// other comments.
#[test]
fn comment_writes_dont_grow_with_the_comments() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    let written = |s: &mut Session, q: &str| {
        let before = pg_engine::catalog::persist::bytes_written();
        run(s, q);
        pg_engine::catalog::persist::bytes_written() - before
    };
    let add = |s: &mut Session, from: usize, to: usize| {
        for i in from..to {
            run(s, &format!("CREATE TABLE c{i} (a int);\nCOMMENT ON TABLE c{i} IS 'a fairly long comment on table number {i}'"));
        }
    };
    add(&mut s, 0, 5);
    let small = written(&mut s, "COMMENT ON TABLE c1 IS 'changed'");
    add(&mut s, 5, 300);
    let large = written(&mut s, "COMMENT ON TABLE c2 IS 'changed'");
    assert!(large <= small + 64, "{small} catalog bytes written with 5 comments, {large} with 300");
    engine.shutdown();
}
