//! The catalog is stored as a header and a record per object (`catalog::persist`): a commit
//! of DDL writes only the objects it changed, the catalog read back after a restart is the
//! one committed (whatever rolled back, savepoints included), and a database stored in the
//! format from before (the whole catalog as one value) is rewritten in this one when it is
//! first opened, its catalog unchanged.

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

fn run(s: &mut Session, sql: &str) {
    for stmt in sql.split(";\n").map(str::trim).filter(|q| !q.is_empty()) {
        s.query(stmt).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
}

/// Objects of many kinds, in every map and set of the catalog that DDL fills.
const SETUP: &str = "
    CREATE SCHEMA s;
    CREATE ROLE r1;
    CREATE TYPE s.mood AS ENUM ('sad', 'ok', 'happy');
    CREATE TYPE s.pair AS (a int, b text);
    CREATE DOMAIN s.posint AS int CHECK (VALUE > 0);
    CREATE TYPE s.fr AS RANGE (subtype = float8);
    CREATE SEQUENCE s.seq START 5;
    CREATE TABLE s.t (id serial PRIMARY KEY, m s.mood, p s.pair, d s.posint DEFAULT 3, x text COLLATE \"C\" CHECK (x <> ''), r s.fr);
    CREATE INDEX t_x ON s.t (lower(x)) WHERE id > 2;
    CREATE TABLE s.c (id int REFERENCES s.t ON DELETE CASCADE, v int UNIQUE);
    CREATE TABLE s.parted (k int, v text) PARTITION BY RANGE (k);
    CREATE TABLE s.parted_1 PARTITION OF s.parted FOR VALUES FROM (0) TO (10);
    CREATE VIEW s.v AS SELECT id, x FROM s.t WHERE id > 1;
    CREATE MATERIALIZED VIEW s.mv AS SELECT count(*) AS n FROM s.t;
    CREATE FUNCTION s.f(a int) RETURNS int LANGUAGE sql IMMUTABLE RETURN a + 1;
    CREATE FUNCTION s.trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
    CREATE TRIGGER t_trg BEFORE INSERT ON s.t FOR EACH ROW EXECUTE FUNCTION s.trg();
    ALTER TABLE s.t ENABLE ROW LEVEL SECURITY;
    CREATE POLICY pol ON s.t USING (id > 0);
    CREATE RULE r AS ON INSERT TO s.c WHERE NEW.v < 0 DO INSTEAD NOTHING;
    CREATE STATISTICS s.st ON id, d FROM s.t;
    CREATE OPERATOR s.=== (LEFTARG = int, RIGHTARG = int, FUNCTION = int4eq);
    COMMENT ON TABLE s.t IS 'a table';
    GRANT SELECT ON s.t TO r1;
    INSERT INTO s.t (m, x) VALUES ('ok', 'a'), ('sad', 'b'), ('happy', 'c');
    ANALYZE s.t;
    ALTER TABLE s.t ADD COLUMN later int;
    ALTER TABLE s.t DROP COLUMN later;
    CREATE TABLE s.gone (a int);
    DROP TABLE s.gone
";

/// The catalog as clients see it.
fn catalog(s: &mut Session) -> Vec<Vec<String>> {
    [
        "SELECT oid, relname, relnamespace, relkind, relnatts, relchecks, relhasindex, relhasrules, relhastriggers, relhassubclass, relrowsecurity, relacl, reltuples, relpages, relfilenode FROM pg_class WHERE oid >= 16384 ORDER BY oid",
        "SELECT attrelid, attnum, attname, atttypid, attnotnull, atthasdef, attisdropped, attcollation FROM pg_attribute WHERE attrelid >= 16384 ORDER BY 1, 2",
        "SELECT oid, typname, typtype, typrelid, typarray FROM pg_type WHERE oid >= 16384 ORDER BY oid",
        "SELECT oid, conname, contype, conrelid, pg_get_constraintdef(oid) FROM pg_constraint WHERE oid >= 16384 ORDER BY oid",
        "SELECT oid, proname, prosrc FROM pg_proc WHERE oid >= 16384 ORDER BY oid",
        "SELECT indexrelid, indrelid, indkey, pg_get_indexdef(indexrelid) FROM pg_index WHERE indexrelid >= 16384 ORDER BY 1",
        "SELECT classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype FROM pg_depend WHERE objid >= 16384 ORDER BY 1, 2, 3, 4, 5, 6",
        "SELECT oid, nspname FROM pg_namespace ORDER BY oid",
        "SELECT rolname FROM pg_roles ORDER BY oid",
        "SELECT tgname, tgrelid FROM pg_trigger ORDER BY oid",
        "SELECT polname FROM pg_policy",
        "SELECT rulename, ev_class FROM pg_rewrite WHERE ev_class >= 16384 ORDER BY 1",
        "SELECT stxname FROM pg_statistic_ext",
        "SELECT oprname FROM pg_operator WHERE oid >= 16384",
        "SELECT objoid, description FROM pg_description WHERE objoid >= 16384",
        "SELECT starelid, staattnum, stanullfrac FROM pg_statistic WHERE starelid >= 16384 ORDER BY 1, 2",
        "SELECT enumlabel FROM pg_enum ORDER BY oid",
        "SELECT rngtypid FROM pg_range WHERE rngtypid >= 16384",
        "SELECT last_value FROM s.seq",
        "SELECT * FROM s.t ORDER BY id",
        "SELECT * FROM s.v ORDER BY id",
    ]
    .iter()
    .map(|q| rows(s, q))
    .collect()
}

#[test]
fn catalog_survives_restarts_and_rollbacks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, SETUP);
    // Rolled back DDL, whole or to a savepoint, leaves nothing stored.
    run(
        &mut s,
        "BEGIN;
         CREATE TABLE s.rolled (a int);
         ALTER TABLE s.t RENAME COLUMN x TO y;
         ROLLBACK;
         BEGIN;
         CREATE TABLE s.kept (a int);
         SAVEPOINT p;
         CREATE TABLE s.undone (a int);
         DROP TABLE s.c;
         ROLLBACK TO SAVEPOINT p;
         ALTER TABLE s.kept ADD COLUMN b text;
         SAVEPOINT q;
         CREATE INDEX kept_b ON s.kept (b);
         RELEASE SAVEPOINT q;
         COMMIT",
    );
    let before = catalog(&mut s);
    assert!(engine.catalog_records_for_test("postgres").unwrap() > 0);
    drop(s);
    engine.shutdown();
    drop(engine);

    let engine = open(dir.path());
    let mut s = engine.connect(2, "postgres", "postgres", "t").unwrap();
    assert_eq!(catalog(&mut s), before);
    assert!(rows(&mut s, "SELECT relname FROM pg_class WHERE relname IN ('rolled', 'undone')").is_empty());
    // And it goes on from there: new objects get new oids, dropped ones stay gone.
    run(&mut s, "CREATE TABLE s.after (a int);\nDROP TABLE s.kept");
    let again = catalog(&mut s);
    drop(s);
    engine.shutdown();
    drop(engine);
    let engine = open(dir.path());
    let mut s = engine.connect(3, "postgres", "postgres", "t").unwrap();
    assert_eq!(catalog(&mut s), again);
    engine.shutdown();
}

#[test]
fn catalog_without_shutdown_is_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let before = {
        let engine = open(dir.path());
        let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, SETUP);
        catalog(&mut s)
        // (No shutdown: the store is reopened from its WAL.)
    };
    let engine = open(dir.path());
    let mut s = engine.connect(2, "postgres", "postgres", "t").unwrap();
    assert_eq!(catalog(&mut s), before);
    engine.shutdown();
}

#[test]
fn legacy_catalog_is_rewritten_as_records() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, SETUP);
    let before = catalog(&mut s);
    drop(s);
    // As a database written by a build from before the format.
    assert!(engine.downgrade_catalog_format_for_test("postgres").unwrap() > 0);
    assert_eq!(engine.catalog_records_for_test("postgres").unwrap(), 0);
    engine.shutdown();
    drop(engine);

    let engine = open(dir.path());
    let mut s = engine.connect(2, "postgres", "postgres", "t").unwrap();
    assert!(engine.catalog_records_for_test("postgres").unwrap() > 0);
    assert_eq!(catalog(&mut s), before);
    run(&mut s, "CREATE TABLE s.after (a int)");
    let again = catalog(&mut s);
    drop(s);
    engine.shutdown();
    drop(engine);
    let engine = open(dir.path());
    let mut s = engine.connect(3, "postgres", "postgres", "t").unwrap();
    assert_eq!(catalog(&mut s), again);
    engine.shutdown();
}

/// DDL committed while another transaction's DDL was in progress (`txn::catmerge` carries
/// the later commit's changes onto the current catalog): the stored catalog is the merged
/// one, whole, after a restart.
#[test]
fn merged_concurrent_ddl_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut a = engine.connect(1, "postgres", "postgres", "t").unwrap();
    let mut b = engine.connect(2, "postgres", "postgres", "t").unwrap();
    run(&mut a, "CREATE TABLE base_t (id int PRIMARY KEY, v text)");
    run(&mut a, "BEGIN;\nCREATE TABLE from_a (x int);\nCREATE INDEX base_t_v ON base_t (v)");
    run(&mut b, "CREATE TABLE from_b (y text DEFAULT 'b');\nCOMMENT ON TABLE base_t IS 'by b'");
    run(&mut a, "COMMIT");
    let before = catalog_of(&mut a);
    for t in ["from_a", "from_b", "base_t_v"] {
        assert!(before.iter().flatten().any(|r| r.contains(t)), "{t} missing:\n{before:?}");
    }
    // A later statement's commit after the merge, then a restart.
    run(&mut b, "CREATE TABLE after_merge (z int)");
    let before = catalog_of(&mut b);
    drop((a, b));
    engine.shutdown();
    drop(engine);
    let engine = open(dir.path());
    let mut s = engine.connect(3, "postgres", "postgres", "t").unwrap();
    assert_eq!(catalog_of(&mut s), before);
    engine.shutdown();
}

fn catalog_of(s: &mut Session) -> Vec<Vec<String>> {
    [
        "SELECT oid, relname, relkind, relnatts, relhasindex FROM pg_class WHERE oid >= 16384 ORDER BY oid",
        "SELECT attrelid, attnum, attname, atthasdef FROM pg_attribute WHERE attrelid >= 16384 ORDER BY 1, 2",
        "SELECT indexrelid, indrelid FROM pg_index WHERE indexrelid >= 16384 ORDER BY 1",
        "SELECT objoid, description FROM pg_description WHERE objoid >= 16384",
        "SELECT adrelid, pg_get_expr(adbin, adrelid) FROM pg_attrdef ORDER BY 1",
    ]
    .iter()
    .map(|q| rows(s, q))
    .collect()
}

/// The bytes a statement's commit writes for the catalog don't grow with the catalog.
#[test]
fn ddl_writes_dont_grow_with_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    let stmts = [
        "CREATE TABLE w (id int PRIMARY KEY, q text, y numeric(8,2))",
        "ALTER TABLE w ADD COLUMN x int NOT NULL DEFAULT 4",
        "CREATE INDEX w_x ON w (x)",
        "COMMENT ON TABLE w IS 'w'",
        "DROP TABLE w",
    ];
    let written = |s: &mut Session| -> Vec<u64> {
        stmts
            .iter()
            .map(|q| {
                let before = pg_engine::catalog::persist::bytes_written();
                run(s, q);
                pg_engine::catalog::persist::bytes_written() - before
            })
            .collect()
    };
    let add = |s: &mut Session, from: usize, to: usize| {
        for i in from..to {
            run(s, &format!("CREATE TABLE other{i} (a int PRIMARY KEY, b text DEFAULT 'x', c int CHECK (c > 0));\nCREATE INDEX ON other{i} (b)"));
        }
    };
    add(&mut s, 0, 5);
    let small = written(&mut s);
    add(&mut s, 5, 200);
    let large = written(&mut s);
    for ((q, a), b) in stmts.iter().zip(&small).zip(&large) {
        assert!(*a > 0, "{q}: nothing written");
        // (The header's counters may take a digit more.)
        assert!(*b <= a + 64, "{q}: {a} catalog bytes written with 5 other tables, {b} with 200");
    }
    engine.shutdown();
}
