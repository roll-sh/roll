//! VACUUM measures the on-page heap representation of detoasted scan values.
use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::exec::sizing::{heap_writes, is_estimated, with_max_versions};
use pg_engine::session::Session;
use pg_engine::types::Value;
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

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last()
        .unwrap()
        .rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect()
}

// PostgreSQL18.6: each fixture has 33 main-heap pages after VACUUM, and its
// large payload remains intact. No model-state reopen shortcut is involved.
fn measured(expr: &str, chars: i64, bytes: i64, digest: &str) {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        s.query("CREATE FUNCTION mi_text(n int) RETURNS text LANGUAGE sql IMMUTABLE AS $$ SELECT left(string_agg(md5(g::text), ''), n) FROM generate_series(1, (n+31)/32) g $$").unwrap();
        s.query("CREATE TABLE vt (k int, payload text)").unwrap();
        s.query(&format!("INSERT INTO vt SELECT g, CASE WHEN g=6000 THEN {expr} ELSE 'small' END FROM generate_series(1,6000) g")).unwrap();
        let oid = s.query("SELECT 'vt'::regclass::oid").unwrap()[0].rows[0][0].clone();
        let Value::Oid(oid) = oid else {
            panic!("table oid")
        };
        assert!(is_estimated(oid, &s.state.db));
        assert!(heap_writes(oid, &s.state.db).is_none());
        let check = format!(
            "SELECT count(*), sum(length(payload)), sum(octet_length(payload)), count(*) FILTER (WHERE payload = CASE WHEN k=6000 THEN {expr} ELSE 'small' END) FROM vt"
        );
        let before = rows(&mut s, &check);
        assert_eq!(
            before,
            [format!(
                "[Int8(6000), Int8({}), Int8({}), Int8(6000)]",
                29995 + chars,
                29995 + bytes
            )]
        );
        s.query("VACUUM vt").unwrap();
        assert_eq!(
            rows(&mut s, "SELECT pg_relation_size('vt')"),
            ["[Int8(270336)]"]
        );
        assert_eq!(s.state.db.unchanged.valid(&s.state.db.store, oid).unwrap().live, 33);
        assert_eq!(rows(&mut s, &check), before);
        assert_eq!(
            rows(&mut s, "SELECT md5(payload) FROM vt WHERE k=6000"),
            [format!("[Text(\"{digest}\")]")]
        );
        assert!(is_estimated(oid, &s.state.db));
        assert!(heap_writes(oid, &s.state.db).is_none());
    });
}

#[test]
fn vacuum_measures_large_ascii_heap() {
    measured(
        "mi_text(2800)",
        2800,
        2800,
        "c53e0a9b2c9a82bd36cc1507decceb06",
    );
    measured(
        "mi_text(8300)",
        8300,
        8300,
        "83054f2dbee0679e1e8da46ff373e036",
    );
}

#[test]
fn vacuum_measures_large_unicode_heap() {
    // stored_tuple_len cannot use its ASCII shortcut, so this covers full decoding.
    measured(
        "mi_text(8298) || 'é'",
        8299,
        8300,
        "7dc4288c8ae8d92c82a2ab9816ff636a",
    );
}


#[test]
fn vacuum_keeps_dropped_stored_column_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    with_max_versions(500, || {
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        s.query("CREATE TABLE vd(k int, p text, n numeric)").unwrap();
        s.query("INSERT INTO vd SELECT g, repeat('x',800), 1.25 FROM generate_series(1,6000) g").unwrap();
        let oid = s.query("SELECT 'vd'::regclass::oid").unwrap()[0].rows[0][0].clone();
        let Value::Oid(oid) = oid else { panic!("table oid") };
        assert!(is_estimated(oid, &s.state.db));
        assert!(heap_writes(oid, &s.state.db).is_none());
        s.query("VACUUM vd").unwrap();
        let before = s.state.db.unchanged.valid(&s.state.db.store, oid).unwrap().live;
        assert_eq!(before, 667, "PG18.6 physical heap before DROP");
        s.query("ALTER TABLE vd DROP COLUMN p").unwrap();
        s.query("VACUUM vd").unwrap();
        let after = s.state.db.unchanged.valid(&s.state.db.store, oid).unwrap().live;
        assert_eq!(after, before, "DROP COLUMN does not rewrite old heap tuples");
        assert_eq!(rows(&mut s, "SELECT count(*)=6000, sum(k)=18003000, sum(n)=7500, min(n)=1.25, max(n)=1.25 FROM vd"), ["[Bool(true), Bool(true), Bool(true), Bool(true), Bool(true)]"]);
    });
}

fn load_large_row(main: bool) {
    let dir = tempfile::tempdir().unwrap();
    let eng = open(dir.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    s.query("CREATE FUNCTION mi_text(n int) RETURNS text LANGUAGE sql IMMUTABLE AS $$ SELECT left(string_agg(md5(g::text), ''), n) FROM generate_series(1, (n+31)/32) g $$").unwrap();
    let len = if main { 8150 } else { 2000 };
    let v = s.query(&format!("SELECT mi_text({len})")).unwrap()[0].rows[0][0].clone();
    let Value::Text(payload) = v else { panic!("text payload") };
    let (table, columns) = if main { ("lm", 1) } else { ("lc", 5) };
    if main {
        s.query("CREATE TABLE lm(k int, p text); ALTER TABLE lm ALTER COLUMN p SET STORAGE MAIN").unwrap();
    } else {
        s.query("CREATE TABLE lc(k int, a text, b text, c text, d text, e text)").unwrap();
    }
    s.query(&format!("BEGIN; TRUNCATE {table}")).unwrap();
    let mut data = String::from("1");
    for _ in 0..columns { data.push('\t'); data.push_str(&payload); }
    data.push('\n');
    s.query_copy(&format!("COPY {table} FROM STDIN"), data.as_bytes()).unwrap();
    s.query("COMMIT").unwrap();
    s.query(&format!("VACUUM {table}")).unwrap();
    let check = if main {
        "SELECT count(*)=1, min(k)=1, octet_length(min(p))=8150, md5(min(p))='49391456ca2ff38098d43965c88b25b5' FROM lm"
    } else {
        "SELECT count(*)=1, min(k)=1, bool_and(a=b AND b=c AND c=d AND d=e), octet_length(min(a))=2000, md5(min(a))='d97786365a1cc209284e0f70164b472c' FROM lc"
    };
    let expected = if main { "[Bool(true), Bool(true), Bool(true), Bool(true)]" } else { "[Bool(true), Bool(true), Bool(true), Bool(true), Bool(true)]" };
    assert_eq!(rows(&mut s, check), [expected]);
    assert_eq!(rows(&mut s, &format!("SELECT pg_relation_size('{table}')")), ["[Int8(8192)]"]);
}

#[test]
fn copy_load_sizes_combined_toasted_row() { load_large_row(false); }

#[test]
fn copy_load_sizes_main_toasted_row() { load_large_row(true); }
