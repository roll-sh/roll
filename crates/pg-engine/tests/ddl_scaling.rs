//! DDL's work doesn't grow with the size of the rest of the catalog: a statement copies the
//! catalog a fixed number of times, however many other tables, indexes and views there are.
//! (ALTER TABLE once copied the whole catalog for each table, to look for index expressions
//! of the table's row type: over the catalogs a full regression run builds up, each ALTER
//! TABLE took seconds and alter_table ran past the harness's timeout.)

use pg_engine::catalog::catalog_copies;
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

fn run(s: &mut Session, sql: &str) {
    for stmt in sql.split(";\n").map(str::trim).filter(|q| !q.is_empty()) {
        s.query(stmt).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
}

/// Other objects: tables with expression indexes, views over them, functions.
fn add_objects(s: &mut Session, from: usize, to: usize) {
    for i in from..to {
        run(
            s,
            &format!(
                "CREATE TABLE other{i} (a int, b text, c int);\n\
                 CREATE INDEX other{i}_x ON other{i} ((a + c));\n\
                 CREATE VIEW other{i}_v AS SELECT a, b FROM other{i} WHERE c > {i};\n\
                 CREATE FUNCTION other{i}_f(x int) RETURNS int LANGUAGE sql RETURN x + {i}"
            ),
        );
    }
}

/// The catalog copies each statement makes, run on a table of its own.
fn copies(s: &mut Session) -> Vec<(&'static str, u64)> {
    let stmts = [
        "CREATE TABLE t (id int PRIMARY KEY, q text, y numeric(8,2))",
        "ALTER TABLE t ADD COLUMN x int NOT NULL DEFAULT 4",
        "ALTER TABLE t ADD COLUMN z int",
        "ALTER TABLE t DROP q",
        "ALTER TABLE t ALTER COLUMN y TYPE numeric(10,2)",
        "CREATE INDEX t_x ON t (x)",
        "ALTER TABLE t RENAME COLUMN z TO w",
        "DROP TABLE t",
    ];
    stmts
        .iter()
        .map(|q| {
            let before = catalog_copies();
            run(s, q);
            (*q, catalog_copies() - before)
        })
        .collect()
}

#[test]
fn ddl_copies_of_the_catalog_dont_grow_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    add_objects(&mut s, 0, 3);
    let small = copies(&mut s);
    add_objects(&mut s, 3, 60);
    let large = copies(&mut s);
    for ((q, a), (_, b)) in small.iter().zip(&large) {
        assert!(b <= a, "{q}: {a} catalog copies with 3 other tables, {b} with 60");
    }
    engine.shutdown();
}

/// The system relations a query generates.
fn generated(s: &mut Session, q: &str) -> u64 {
    let before = pg_engine::catalog::system::generated_rows_count();
    run(s, q);
    pg_engine::catalog::system::generated_rows_count() - before
}

/// A catalog scanned once per outer row (a nested loop's parameterized inner side, a
/// correlated sub-select) is generated once per statement, as Postgres reads it through its
/// index: information_schema.column_domain_usage and oidjoins' checks over a full run's
/// catalogs took minutes when each outer row generated pg_type again.
#[test]
fn catalog_rescans_dont_grow_with_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE DOMAIN d AS int;\nCREATE TABLE dt (x d)");
    let queries = [
        "SELECT * FROM information_schema.column_domain_usage WHERE domain_name = 'd'",
        "SELECT a.attname, (SELECT t.typname FROM pg_type t WHERE t.oid = a.atttypid) FROM pg_attribute a",
        "SELECT count(*) FROM pg_attribute a WHERE NOT EXISTS (SELECT 1 FROM pg_type t WHERE t.oid = a.atttypid)",
    ];
    let count = |s: &mut Session| -> Vec<u64> {
        run(s, "SET enable_hashjoin = off;\nSET enable_mergejoin = off");
        let n = queries.iter().map(|q| generated(s, q)).collect();
        run(s, "RESET enable_hashjoin;\nRESET enable_mergejoin");
        n
    };
    add_objects(&mut s, 0, 3);
    let small = count(&mut s);
    add_objects(&mut s, 3, 40);
    let large = count(&mut s);
    for ((q, a), b) in queries.iter().zip(&small).zip(&large) {
        assert!(b <= a, "{q}: {a} system relations generated with 3 other tables, {b} with 40");
    }
    engine.shutdown();
}
