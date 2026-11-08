//! Catalog format 2 (round 19, agent 76): comments are a record per object instead of a list
//! in the catalog header. A database stored in format 1 is rewritten when it is first opened,
//! its comments unchanged and in pg_description's order; `Engine::downgrade_catalogs`
//! (`roll-engine --downgrade-catalog`) stores it as format 1 again; and a DDL commit writes
//! the comments of the objects it changed, not every comment.
//!
//! Catalog format 3 (round 21, agent 92): a record leaves out the fields at their default
//! values; a downgrade stores every field again, and the next open rewrites the records.

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
        auth_files: Default::default(),
        settings: Vec::new(),
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

/// The stored record of table `name` (its JSON), and the bytes of all the objects' records
/// (format 4's index records, tags from 128 on, which a downgrade deletes, left out).
fn table_record(engine: &Engine, name: &str) -> (String, usize) {
    let all = engine.catalog_record_bytes_for_test("postgres").unwrap();
    let all: Vec<_> = all.into_iter().filter(|(k, _)| k.get(8).is_some_and(|t| *t < 128)).collect();
    let bytes = all.iter().map(|(k, v)| k.len() + v.len()).sum();
    let needle = format!("\"name\":\"{name}\"");
    let rec = all
        .iter()
        .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
        .find(|v| v.contains(&needle) && v.contains("\"columns\""))
        .unwrap();
    (rec, bytes)
}

/// Format 3 records leave default values out; `downgrade_catalogs` stores every field (what
/// a format 2 or older build requires), and opening the database again stores it compactly
/// once more, the catalog alike throughout.
#[test]
fn records_leave_defaults_out_and_downgrade_whole() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let what = "SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, a.attislocal, a.attinhcount, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid IN ('wide'::regclass, 'kid'::regclass) AND a.attnum > 0 ORDER BY a.attrelid, a.attnum";
    let cons = "SELECT conname, contype, convalidated, conislocal, coninhcount, condeferrable FROM pg_constraint WHERE conrelid IN ('wide'::regclass, 'kid'::regclass) ORDER BY conname";
    let expected = {
        let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, SETUP);
        run(&mut s, "CREATE TABLE wide (a int PRIMARY KEY, b text DEFAULT 'x' COLLATE \"C\", c numeric(5,2) NOT NULL, d int[], e int UNIQUE DEFERRABLE);\nCREATE TABLE kid (f int) INHERITS (wide);\nCREATE INDEX wide_bc ON wide (b DESC NULLS LAST, c) WHERE c > 0");
        (rows(&mut s, what), rows(&mut s, cons), comments(&mut s))
    };
    let (rec, compact) = table_record(&engine, "wide");
    assert!(!rec.contains("\"dropped\"") && !rec.contains("\"identity\""), "{rec}");
    assert!(rec.contains("\"collation\":\"C\""), "{rec}");
    assert!(engine.downgrade_catalogs().unwrap().iter().any(|d| d == "postgres"));
    let (rec, whole) = table_record(&engine, "wide");
    assert!(rec.contains("\"dropped\":false") && rec.contains("\"islocal\":true"), "{rec}");
    assert!(whole > compact * 3 / 2, "{whole} bytes whole, {compact} compact");
    engine.shutdown();
    drop(engine);

    let engine = open(dir.path());
    let mut s = engine.connect(2, "postgres", "postgres", "t").unwrap();
    assert_eq!((rows(&mut s, what), rows(&mut s, cons), comments(&mut s)), expected);
    let (rec, again) = table_record(&engine, "wide");
    assert!(!rec.contains("\"dropped\""), "{rec}");
    assert_eq!(again, compact);
    engine.shutdown();
}

/// Catalog format 4 (round 24, agent 146): a data directory a format-3 build made and
/// stopped (`tests/data/catalog_format3.tar.gz`: tables with defaults, checks, inheritance,
/// partitions, a foreign key, an expression index, a view, a materialized view, a sequence,
/// a function, a composite type and its users, row security, comments, statistics, a
/// truncated partition's file number, and 300 more tables, enough for the tables' and
/// indexes' maps to be read on demand once upgraded) opens; its catalog is rewritten with its
/// indexes, and opened again it reads alike, through DDL and more opens.
#[test]
fn a_format_3_data_directory_opens_and_upgrades() {
    let dir = tempfile::tempdir().unwrap();
    let tgz = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/catalog_format3.tar.gz");
    let st = std::process::Command::new("tar").arg("xzf").arg(tgz).arg("-C").arg(dir.path()).status().unwrap();
    assert!(st.success());
    let reads = [
        "SELECT relkind, count(*) FROM pg_class WHERE relnamespace = 's1'::regnamespace GROUP BY 1 ORDER BY 1",
        "SELECT * FROM s1.parent ORDER BY id",
        "SELECT * FROM s1.v ORDER BY id",
        "SELECT * FROM s1.mv ORDER BY id",
        "SELECT * FROM s1.part ORDER BY k",
        "SELECT p, length(big) FROM s1.ref",
        "SELECT s1.f(41), last_value FROM s1.seq",
        "SELECT inhrelid::regclass::text FROM pg_inherits WHERE inhparent IN ('s1.parent'::regclass, 's1.part'::regclass) ORDER BY 1",
        "SELECT conname, contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE connamespace = 's1'::regnamespace AND conrelid IN ('s1.parent'::regclass, 's1.ref'::regclass, 's1.child'::regclass) ORDER BY 1, 3",
        "SELECT pg_get_indexdef('s1.ref_lower'::regclass), pg_get_viewdef('s1.v'::regclass)",
        "SELECT obj_description('s1.parent'::regclass, 'pg_class'), col_description('s1.parent'::regclass, 2)",
        "SELECT polname, pg_get_expr(polqual, polrelid) FROM pg_policy WHERE polrelid = 's1.ref'::regclass",
        "SELECT a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 's1.t150'::regclass",
        "SELECT count(*), count(DISTINCT c.reltype), sum(c.relnatts) FROM pg_class c WHERE c.relnamespace = 's1'::regnamespace",
        "SELECT attrelid::regclass::text, attname FROM pg_attribute WHERE atttypid = 's1.pair'::regtype OR atttypid = 's1.pair[]'::regtype ORDER BY 1, 2",
        "SELECT pg_relation_filenode('s1.part1') <> 's1.part1'::regclass::oid",
    ];
    let read_all = |s: &mut Session| reads.iter().map(|q| rows(s, q)).collect::<Vec<_>>();
    let engine = open(dir.path());
    let first = {
        let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
        read_all(&mut s)
    };
    assert_eq!(first[0].len(), 7, "{:?}", first[0]);
    assert!(first[0][5].contains("305"), "{:?}", first[0]);
    assert_eq!(first[1].len(), 3, "{:?}", first[1]);
    assert!(first[12][0].contains("d150"), "{:?}", first[12]);
    engine.shutdown();
    drop(engine);

    // Opened in format 4: the same, then DDL over relations read on demand.
    let engine = open(dir.path());
    let mut s = engine.connect(2, "postgres", "postgres", "t").unwrap();
    assert_eq!(read_all(&mut s), first);
    run(&mut s, "CREATE TABLE s1.t301 (id int PRIMARY KEY);\nDROP TABLE s1.t7;\nALTER TABLE s1.t8 RENAME TO t8r;\nCREATE TABLE s1.child2 () INHERITS (s1.parent);\nINSERT INTO s1.child2 VALUES (4, 'd', 4.5)");
    let names = "SELECT to_regclass('s1.t7') IS NULL, to_regclass('s1.t8r') IS NOT NULL, to_regclass('s1.t8') IS NULL, (SELECT count(*) FROM s1.parent), (SELECT count(*) FROM pg_inherits WHERE inhparent = 's1.parent'::regclass)";
    let after = (read_all(&mut s), rows(&mut s, names));
    assert_eq!(after.1, ["[Bool(true), Bool(true), Bool(true), Int8(4), Int8(2)]"]);
    engine.shutdown();
    drop(engine);
    let engine = open(dir.path());
    let mut s = engine.connect(3, "postgres", "postgres", "t").unwrap();
    assert_eq!((read_all(&mut s), rows(&mut s, names)), after);
    engine.shutdown();
}

/// The one text value `sql` returns.
fn text(s: &mut Session, sql: &str) -> String {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let v = &r.last().unwrap().rows[0][0];
    v.as_str().unwrap_or_else(|| panic!("{sql}: {v:?}")).to_string()
}

/// Catalog format 5 (round 26, agent 173): a data directory a format-4 build made and
/// stopped (`tests/data/catalog_format4.tar.gz`, from `catalog_format4.sql`: every kind of
/// relation, with the options, row security, replica identity, persistence, partitions,
/// triggers and rules pg_class shows, and 300 more tables) opens; each relation's directory
/// record is made once and stored, and pg_class and the statistics views read as the format-4
/// build showed them (`catalog_format4.pgclass`), then alike through DDL and more opens.
/// (Debug builds check every directory record read against its entry: `reldir::check`.)
#[test]
fn a_format_4_data_directory_opens_and_upgrades() {
    let dir = tempfile::tempdir().unwrap();
    let tgz = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/catalog_format4.tar.gz");
    let st = std::process::Command::new("tar").arg("xzf").arg(tgz).arg("-C").arg(dir.path()).status().unwrap();
    assert!(st.success());
    let class = "SELECT string_agg(c::text, E'\\n' ORDER BY c.oid) FROM pg_class c WHERE c.oid >= 16384";
    let stats = "SELECT string_agg(format('%s %s %s', relid, schemaname, relname), E'\\n' ORDER BY relid) FROM pg_stat_user_tables";
    let dt = "SELECT string_agg(c.relname || ' ' || c.relkind::text, ',' ORDER BY c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('r', 'p', 'v', 'm', 'S', 'i', 'I', 't') AND n.nspname = 's1'";
    let want = include_str!("data/catalog_format4.pgclass");
    let reads = |s: &mut Session| [text(s, class), text(s, stats), text(s, dt)];
    let engine = open(dir.path());
    let first = {
        let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
        // (The statistics view first: its directory read comes before pg_class's rows.)
        let st = text(&mut s, stats);
        let mut r = reads(&mut s);
        assert_eq!(r[1], st);
        assert_eq!(format!("{}\n", r[0]), want);
        r[0].clear();
        r
    };
    engine.shutdown();
    drop(engine);

    // Opened in format 5: alike, then DDL over the stored records.
    let engine = open(dir.path());
    let mut s = engine.connect(2, "postgres", "postgres", "t").unwrap();
    let mut again = reads(&mut s);
    assert_eq!(format!("{}\n", again[0]), want);
    again[0].clear();
    assert_eq!(again, first);
    run(
        &mut s,
        "CREATE TABLE s1.t301 (id int PRIMARY KEY, x text);\nDROP TABLE s1.t7;\nALTER TABLE s1.t8 RENAME TO t8r;\nALTER TABLE s1.t9 SET UNLOGGED;\nALTER TABLE s1.t10 ENABLE ROW LEVEL SECURITY;\nALTER TABLE s1.t11 ADD CONSTRAINT c11 CHECK (id > 0);\nALTER INDEX s1.ref_lower SET (fillfactor = 90);\nALTER VIEW s1.plain RENAME TO plain2;\nALTER MATERIALIZED VIEW s1.mv SET SCHEMA public;\nCREATE INDEX t12_a ON s1.t12 (a)",
    );
    let after = reads(&mut s);
    assert!(after[2].contains("t8r r") && !after[2].contains(",t7 r,") && after[2].contains("t301_pkey i"), "{}", after[2]);
    assert!(after[0].contains("plain2") && after[1].contains(" public mv\n") && !after[1].contains(" s1 mv\n"), "{}", after[1]);
    engine.shutdown();
    drop(engine);
    let engine = open(dir.path());
    let mut s = engine.connect(3, "postgres", "postgres", "t").unwrap();
    // (Its statistics view first again, then the rows: both from the stored records.)
    assert_eq!(text(&mut s, stats), after[1]);
    assert_eq!(reads(&mut s), after);
    engine.shutdown();
}
