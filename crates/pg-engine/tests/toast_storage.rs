//! Out-of-line storage of large values (`storage::toast`): values of every type round-trip
//! through the write paths (INSERT, COPY, UPDATE, DELETE, TRUNCATE, rewrites, rollbacks,
//! savepoints, cursors opened before a change, a restart); an UPDATE that doesn't change a
//! large value doesn't write it again; a scan reads only the values of the columns it
//! decodes, and only for the rows its filter keeps.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::exec::toast::VALUES_WRITTEN;
use pg_engine::session::Session;
use pg_engine::storage::toast::VALUES_READ;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};
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
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: Vec::new(),
    })
    .unwrap()
}

/// The counters are the process's: the tests that read them run one at a time.
static COUNTERS: Mutex<()> = Mutex::new(());

fn counters() -> MutexGuard<'static, ()> {
    COUNTERS.lock().unwrap_or_else(|e| e.into_inner())
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

fn one(s: &mut Session, sql: &str) -> String {
    let r = rows(s, sql);
    assert_eq!(r.len(), 1, "{sql}: {r:?}");
    r[0].clone()
}

fn run(s: &mut Session, sql: &str) {
    for stmt in sql.split(";\n").map(str::trim).filter(|q| !q.is_empty()) {
        s.query(stmt).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
}

/// Values written and read while `f` runs.
fn counted(f: impl FnOnce()) -> (u64, u64) {
    let (w, r) = (VALUES_WRITTEN.load(Ordering::Relaxed), VALUES_READ.load(Ordering::Relaxed));
    f();
    (VALUES_WRITTEN.load(Ordering::Relaxed) - w, VALUES_READ.load(Ordering::Relaxed) - r)
}

/// The table's big values, checked against the expressions that made them: how many rows
/// have every value as made (from `id` and `gen`).
const CHECK: &str = "SELECT count(*) FILTER (WHERE t = repeat(md5(id::text || gen::text), 80)
        AND b = decode(repeat(md5(id::text), 140), 'hex')
        AND j = jsonb_build_object('k', id, 's', repeat('j' || gen::text, 1500))
        AND a = (SELECT array_agg(i * id) FROM generate_series(1, 900) i)), count(*) FROM big";

const FILL: &str = "INSERT INTO big SELECT id, id % 10, 0, repeat(md5(id::text || '0'), 80),
        decode(repeat(md5(id::text), 140), 'hex'),
        jsonb_build_object('k', id, 's', repeat('j0', 1500)),
        (SELECT array_agg(i * id) FROM generate_series(1, 900) i)
    FROM generate_series(1, 200) id";

#[test]
fn index_fetches_read_only_the_values_they_decode() {
    let _g = counters();
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE TABLE big (id int PRIMARY KEY, cat int, gen int, t text, b bytea, j jsonb, a int[]);\nCREATE INDEX big_cat ON big (cat)");
    run(&mut s, FILL);
    run(&mut s, "ANALYZE big;\nSET enable_seqscan = off;\nSET enable_bitmapscan = off");
    // A point lookup and an index scan: the values of the columns decoded only.
    let (_, r) = counted(|| assert_eq!(one(&mut s, "SELECT id, cat FROM big WHERE id = 7"), "[Int4(7), Int4(7)]"));
    assert_eq!(r, 0);
    let (_, r) = counted(|| assert_eq!(one(&mut s, "SELECT length(t) FROM big WHERE id = 7"), "[Int4(2560)]"));
    assert_eq!(r, 1);
    let (_, r) = counted(|| assert_eq!(one(&mut s, "SELECT count(*), sum(gen) FROM big WHERE cat = 3"), "[Int8(20), Int8(0)]"));
    assert_eq!(r, 0);
    // A bitmap heap scan's fetches likewise.
    run(&mut s, "SET enable_indexscan = off;\nSET enable_bitmapscan = on");
    let (_, r) = counted(|| assert_eq!(one(&mut s, "SELECT count(*), sum(length(b)) FROM big WHERE cat = 3"), "[Int8(20), Int8(44800)]"));
    assert_eq!(r, 20);
    // Inside a transaction that wrote the rows: its own values.
    run(&mut s, "BEGIN;\nUPDATE big SET t = t || 'x', gen = 1 WHERE cat = 3");
    let (_, r) = counted(|| assert_eq!(one(&mut s, "SELECT count(*), sum(length(t)) FROM big WHERE cat = 3"), "[Int8(20), Int8(51220)]"));
    assert_eq!(r, 0, "the transaction's own values");
    run(&mut s, "COMMIT;\nRESET enable_indexscan;\nRESET enable_bitmapscan;\nRESET enable_seqscan");
    assert_eq!(one(&mut s, CHECK), "[Int8(180), Int8(200)]");
}

#[test]
fn values_round_trip_through_writes() {
    let _g = counters();
    let dir = tempfile::tempdir().unwrap();
    {
        let eng = open(dir.path());
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, "CREATE TABLE big (id int PRIMARY KEY, cat int, gen int, t text, b bytea, j jsonb, a int[])");
        let (w, _) = counted(|| run(&mut s, FILL));
        assert_eq!(w, 800, "every big value written out of line");
        assert_eq!(one(&mut s, CHECK), "[Int8(200), Int8(200)]");

        // An UPDATE of a small column keeps the values: none written again.
        let (w, _) = counted(|| run(&mut s, "UPDATE big SET cat = cat + 1"));
        assert_eq!(w, 0);
        assert_eq!(one(&mut s, CHECK), "[Int8(200), Int8(200)]");

        // One that changes one of them writes that one.
        let (w, _) = counted(|| run(&mut s, "UPDATE big SET gen = 1, t = repeat(md5(id::text || '1'), 80), j = jsonb_build_object('k', id, 's', repeat('j1', 1500)) WHERE id <= 50"));
        assert_eq!(w, 100);
        assert_eq!(one(&mut s, CHECK), "[Int8(200), Int8(200)]");

        // Rolled back, to a savepoint, and seen by a cursor opened before the change.
        run(&mut s, "BEGIN;\nUPDATE big SET t = 'small', gen = 9 WHERE id <= 10;\nROLLBACK");
        assert_eq!(one(&mut s, CHECK), "[Int8(200), Int8(200)]");
        run(&mut s, "BEGIN;\nSAVEPOINT a;\nDELETE FROM big WHERE id > 100;\nUPDATE big SET gen = 2, t = repeat(md5(id::text || '2'), 80) WHERE id <= 20;\nROLLBACK TO a");
        run(&mut s, "DECLARE c CURSOR FOR SELECT md5(t), md5(j::text) FROM big WHERE id = 7");
        let before = rows(&mut s, "SELECT md5(t), md5(j::text) FROM big WHERE id = 7");
        run(&mut s, "UPDATE big SET gen = 3, t = repeat(md5(id::text || '3'), 80), j = jsonb_build_object('k', id, 's', repeat('j3', 1500)) WHERE id = 7");
        assert_eq!(rows(&mut s, "FETCH ALL FROM c"), before);
        assert_eq!(one(&mut s, CHECK), "[Int8(200), Int8(200)]");
        run(&mut s, "COMMIT");
        assert_eq!(one(&mut s, CHECK), "[Int8(200), Int8(200)]");

        // A value made small, and back.
        run(&mut s, "UPDATE big SET t = 'x' WHERE id = 3");
        assert_eq!(one(&mut s, "SELECT t FROM big WHERE id = 3"), "[Text(\"x\")]");
        run(&mut s, "UPDATE big SET t = repeat(md5(id::text || gen::text), 80) WHERE id = 3");

        // Rewrites, deletes, COPY.
        run(&mut s, "ALTER TABLE big ADD COLUMN extra int DEFAULT 5;\nALTER TABLE big ALTER COLUMN cat TYPE bigint;\nVACUUM FULL big;\nCLUSTER big USING big_pkey");
        assert_eq!(one(&mut s, CHECK), "[Int8(200), Int8(200)]");
        run(&mut s, "DELETE FROM big WHERE id % 3 = 0;\nCREATE TABLE big2 (LIKE big);\nINSERT INTO big2 SELECT * FROM big");
        assert_eq!(one(&mut s, CHECK), "[Int8(134), Int8(134)]");
        assert_eq!(one(&mut s, "SELECT count(*) FROM big2 JOIN big USING (id) WHERE big2.t = big.t AND big2.j = big.j"), "[Int8(134)]");
    }
    // After a restart.
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    assert_eq!(one(&mut s, CHECK), "[Int8(134), Int8(134)]");
    run(&mut s, "TRUNCATE big;\nINSERT INTO big (id, cat, gen, t) VALUES (1, 1, 0, repeat('z', 5000))");
    assert_eq!(one(&mut s, "SELECT length(t), count(*) OVER () FROM big"), "[Int4(5000), Int8(1)]");
}

#[test]
fn scans_read_only_the_values_they_decode() {
    let _g = counters();
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE TABLE big (id int PRIMARY KEY, cat int, gen int, t text, b bytea, j jsonb, a int[])");
    run(&mut s, FILL);
    // Columns that aren't decoded: no value read.
    let (_, r) = counted(|| assert_eq!(one(&mut s, "SELECT count(*), sum(cat) FROM big WHERE cat = 3"), "[Int8(20), Int8(60)]"));
    assert_eq!(r, 0);
    // A big column for the rows the filter keeps: those rows' values only.
    let (_, r) = counted(|| assert_eq!(one(&mut s, "SELECT count(*), sum(length(t)) FROM big WHERE cat = 3"), "[Int8(20), Int8(51200)]"));
    assert_eq!(r, 20);
    // A filter over a big column reads it for every row.
    let (_, r) = counted(|| assert_eq!(one(&mut s, "SELECT count(*) FROM big WHERE length(t) > 0"), "[Int8(200)]"));
    assert_eq!(r, 200);
    // PLAIN storage keeps a column's values in their rows.
    run(&mut s, "CREATE TABLE inl (id int, t text);\nALTER TABLE inl ALTER COLUMN t SET STORAGE PLAIN");
    let (w, _) = counted(|| run(&mut s, "INSERT INTO inl SELECT i, repeat('p', 4000) FROM generate_series(1, 10) i"));
    assert_eq!(w, 0);
    assert_eq!(one(&mut s, "SELECT sum(length(t)) FROM inl"), "[Int8(40000)]");
}

#[test]
fn vectors_out_of_line() {
    let _g = counters();
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE EXTENSION vector;\nCREATE TABLE v (id int PRIMARY KEY, cat int, e vector(768));\nCREATE TABLE vp (id int PRIMARY KEY, cat int, e vector(768));\nALTER TABLE vp ALTER COLUMN e SET STORAGE PLAIN");
    let fill = |t: &str| format!("INSERT INTO {t} SELECT i, i % 50, (SELECT array_agg(sin(i * 7 + d))::vector FROM generate_series(1, 768) d) FROM generate_series(1, 400) i");
    let (w, _) = counted(|| run(&mut s, &fill("v")));
    assert_eq!(w, 400);
    let (w, _) = counted(|| run(&mut s, &fill("vp")));
    assert_eq!(w, 0);
    let q = "(SELECT array_agg(cos(d))::vector FROM generate_series(1, 768) d)";
    for sql in [
        format!("SELECT id FROM %t WHERE cat = 7 ORDER BY e <-> {q} LIMIT 5"),
        format!("SELECT id FROM %t ORDER BY e <=> {q} LIMIT 5"),
        format!("SELECT sum(e <#> {q}) FROM %t"),
        "SELECT md5(e::text) FROM %t WHERE id = 17".to_string(),
    ] {
        assert_eq!(rows(&mut s, &sql.replace("%t", "v")), rows(&mut s, &sql.replace("%t", "vp")), "{sql}");
    }
    let (_, r) = counted(|| {
        rows(&mut s, &format!("SELECT id FROM v WHERE cat = 7 ORDER BY e <-> {q} LIMIT 5"));
    });
    assert_eq!(r, 8, "only the filtered rows' vectors");
    run(&mut s, "CREATE INDEX ON v USING hnsw (e vector_l2_ops);\nCREATE INDEX ON vp USING hnsw (e vector_l2_ops)");
    let sql = format!("SELECT id FROM %t ORDER BY e <-> {q} LIMIT 5");
    assert_eq!(rows(&mut s, &sql.replace("%t", "v")), rows(&mut s, &sql.replace("%t", "vp")));
}
