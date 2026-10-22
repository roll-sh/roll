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
