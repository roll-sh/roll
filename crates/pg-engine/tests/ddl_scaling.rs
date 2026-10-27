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
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 0,
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

/// Whole catalogs read right after DDL (round 19, agent 62): pg_class and pg_attribute of the
/// new catalog version are spliced from the last version's rows (`catalog::sysdelta`), not
/// generated again, when the DDL changed only some relations. (Debug builds check every
/// splice against the full generation.)
#[test]
fn whole_catalogs_after_ddl_are_spliced() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    add_objects(&mut s, 0, 5);
    let q = "SELECT count(*) FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid";
    run(&mut s, q);
    for ddl in [
        "COMMENT ON TABLE other1 IS 'x'",
        "CREATE TABLE fresh (a int PRIMARY KEY, b text DEFAULT 'x')",
        "ALTER TABLE fresh ADD COLUMN c int",
        "DROP TABLE other2 CASCADE",
    ] {
        run(&mut s, ddl);
        let spliced = pg_engine::catalog::system::spliced_rows_count();
        let n = generated(&mut s, q);
        assert_eq!(n, 0, "{ddl}: catalogs generated whole after it");
        assert!(pg_engine::catalog::system::spliced_rows_count() > spliced, "{ddl}: nothing spliced");
    }
    engine.shutdown();
}

/// pg_constraint, pg_type and pg_index after DDL are spliced too (round 20, agent 76), and
/// pg_depend and information_schema's constraint views are kept whole across a version in
/// which nothing they read changed (COMMENT).
#[test]
fn more_catalogs_after_ddl_are_spliced_or_kept() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    add_objects(&mut s, 0, 5);
    let spliced_q = [
        "SELECT count(*) FROM pg_constraint c JOIN pg_class r ON r.oid = c.conrelid",
        "SELECT count(*) FROM pg_type t JOIN pg_class r ON r.oid = t.typrelid",
        "SELECT count(*) FROM pg_index i JOIN pg_class r ON r.oid = i.indexrelid",
    ];
    let kept_q = [
        "SELECT count(*) FROM pg_depend",
        "SELECT count(*) FROM information_schema.table_constraints",
    ];
    for q in spliced_q.iter().chain(&kept_q) {
        run(&mut s, q);
    }
    for ddl in [
        "COMMENT ON TABLE other1 IS 'x'",
        "CREATE TABLE fresh (a int PRIMARY KEY, b text CHECK (b <> ''))",
        "ALTER TABLE fresh ADD COLUMN c int UNIQUE",
        "CREATE INDEX fresh_c ON fresh (c)",
        "DROP TABLE other2 CASCADE",
    ] {
        run(&mut s, ddl);
        for q in spliced_q {
            assert_eq!(generated(&mut s, q), 0, "{ddl}: {q}: generated whole after it");
        }
    }
    run(&mut s, "COMMENT ON TABLE other1 IS 'y'");
    for q in kept_q {
        run(&mut s, q);
        run(&mut s, "COMMENT ON TABLE other1 IS 'z'");
        assert_eq!(generated(&mut s, q), 0, "{q}: generated again after a COMMENT");
    }
    engine.shutdown();
}
/// 62): a commit whose catalog's base is no longer current is merged onto the current one
/// under the commit lock, and that must cost what the transactions changed, not the catalog
/// (it reindexed and checked every relation's name, and merged the header's comment list
/// pair by pair). Timed: merged commits over 1,500 tables within 6x those over 100 (a debug
/// build, whose own checks grow with the catalog).
#[test]
fn racing_ddl_commits_dont_grow_with_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut a = engine.connect(1, "postgres", "postgres", "a").unwrap();
    let mut b = engine.connect(2, "postgres", "postgres", "b").unwrap();
    let mut made = 0;
    let mut timed = |a: &mut Session, b: &mut Session, upto: usize, round: usize| -> f64 {
        for i in made..upto {
            run(a, &format!("CREATE TABLE r{i} (a int, b int CHECK (b > 0));\nCOMMENT ON TABLE r{i} IS 'c'"));
        }
        made = upto;
        let t = std::time::Instant::now();
        for i in 0..20 {
            let (x, y) = (i + 20 * round, i + 20 * round + 50);
            run(a, &format!("BEGIN;\nCREATE INDEX ON r{x} (a);\nCOMMENT ON TABLE r{x} IS 'a'"));
            run(b, &format!("BEGIN;\nCREATE INDEX ON r{y} (b);\nCOMMENT ON TABLE r{y} IS 'b'"));
            run(a, "COMMIT");
            run(b, "COMMIT");
        }
        t.elapsed().as_secs_f64()
    };
    let small = timed(&mut a, &mut b, 100, 0);
    let large = timed(&mut a, &mut b, 1500, 1);
    eprintln!("racing commits: {small:.3}s at 100 tables, {large:.3}s at 1,500");
    assert!(large < small * 6.0 + 0.5, "racing commits: {small:.3}s at 100 tables, {large:.3}s at 1,500");
    engine.shutdown();
}

/// Concurrent CREATE / DROP TABLE (pgjdbc's TypeCacheDLLStressTest): the merged catalogs'
/// relation counts (`relindex::RelRows`, pg_class's planner estimate) stay those of their
/// entries; debug builds check them against generated rows at every scan.
#[test]
fn racing_create_and_drop_keep_catalog_counts() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut a = engine.connect(1, "postgres", "postgres", "a").unwrap();
    let mut b = engine.connect(2, "postgres", "postgres", "b").unwrap();
    let count = "SELECT count(*) FROM pg_class";
    for i in 0..30 {
        run(&mut a, &format!("BEGIN;\nCREATE TABLE ca{i} (x int PRIMARY KEY, t text)"));
        run(&mut b, &format!("BEGIN;\nCREATE TABLE cb{i} (x serial, t text)"));
        if i > 0 {
            run(&mut b, &format!("DROP TABLE ca{}", i - 1));
        }
        run(&mut a, "COMMIT");
        run(&mut a, count);
        run(&mut b, "COMMIT");
        run(&mut b, count);
        if i > 0 {
            run(&mut a, &format!("BEGIN;\nDROP TABLE cb{}", i - 1));
            run(&mut b, &format!("BEGIN;\nCREATE TYPE ty{i} AS (a int)"));
            run(&mut b, "COMMIT");
            run(&mut a, "COMMIT");
            run(&mut a, count);
        }
    }
    engine.shutdown();
}
