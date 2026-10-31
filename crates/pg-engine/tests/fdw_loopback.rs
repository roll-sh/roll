//! postgres_fdw over a loopback server (the engine itself), for what the contrib suite's
//! single session can't show: EvalPlanQual rechecks of rows joined to foreign rows under a
//! concurrent update (contrib/postgres_fdw/specs/eval_plan_qual.spec's case), and the
//! results of inherited UPDATE / DELETEs whose foreign members are changed by statements of
//! their own. Expected values are PostgreSQL 18.6's.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use pg_engine::session::Session;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn open(data: &Path) -> Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: data.to_path_buf(),
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
        outbound: vec!["local".into()],
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: Vec::new(),
    })
    .unwrap()
}

fn run(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last().unwrap().rows.iter().map(|r| format!("{r:?}")).collect()
}

/// An engine serving a loopback server `loopback` (and its user mapping) in database postgres.
fn loopback() -> (tempfile::TempDir, Arc<Engine>) {
    let data = tempfile::tempdir().unwrap();
    let engine = open(data.path());
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::Trust }).unwrap();
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE EXTENSION postgres_fdw");
    run(
        &mut s,
        &format!("CREATE SERVER loopback FOREIGN DATA WRAPPER postgres_fdw OPTIONS (dbname 'postgres', port '{}')", addr.port()),
    );
    run(&mut s, "CREATE USER MAPPING FOR PUBLIC SERVER loopback");
    (data, engine)
}

/// A row locked under a concurrent update is rechecked with the foreign rows its join read
/// (their row marks' copies, ROW_MARK_COPY), not by reading the foreign rows again: each of
/// the three joined rows comes back with its own foreign row.
#[test]
fn recheck_keeps_foreign_rows() {
    let (_data, engine) = loopback();
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE TABLE l (i int, v text)");
    run(&mut s, "CREATE TABLE t (i int, v text)");
    run(&mut s, "CREATE FOREIGN TABLE ft (i int, v text) SERVER loopback OPTIONS (table_name 't')");
    run(&mut s, "INSERT INTO l VALUES (123, 'foo'), (456, 'bar')");
    run(&mut s, "INSERT INTO t SELECT i, to_char(i, 'FM0000') FROM generate_series(1, 1000) i");
    run(&mut s, "BEGIN");
    run(&mut s, "UPDATE l SET v = v || 'x' WHERE i = 123");
    let e = engine.clone();
    let reader = std::thread::spawn(move || {
        let mut r = e.connect(2, "postgres", "postgres", "t").unwrap();
        let q = "SELECT l.i, l.v, ft.i, ft.v FROM l, ft WHERE ft.i BETWEEN l.i AND l.i + 2 AND l.i = 123 FOR UPDATE OF l";
        let plan = run(&mut r, &format!("EXPLAIN (VERBOSE, COSTS OFF) {q}"));
        (plan, run(&mut r, q))
    });
    std::thread::sleep(Duration::from_millis(500));
    run(&mut s, "COMMIT");
    let (plan, mut rows) = reader.join().unwrap();
    assert!(plan.iter().any(|l| l.contains("Output: l.i, l.v, ft.i, ft.v, l.ctid, ft.*")), "{plan:#?}");
    rows.sort();
    assert_eq!(
        rows,
        vec![
            r#"[Int4(123), Text("foox"), Int4(123), Text("0123")]"#,
            r#"[Int4(123), Text("foox"), Int4(124), Text("0124")]"#,
            r#"[Int4(123), Text("foox"), Int4(125), Text("0125")]"#,
        ]
    );
}

/// An inherited UPDATE / DELETE whose foreign member is changed by one remote statement
/// (its Append child's Foreign Update / Delete): the counts, RETURNING rows (the member's
/// tableoid too) and the rows left.
#[test]
fn direct_members() {
    let (_data, engine) = loopback();
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    for q in [
        "CREATE TABLE loct2 (f1 int, f2 int, f3 int)",
        "CREATE TABLE bar (f1 int, f2 int)",
        "CREATE FOREIGN TABLE bar2 (f3 int) INHERITS (bar) SERVER loopback OPTIONS (table_name 'loct2')",
        "INSERT INTO bar VALUES (1, 11), (2, 22)",
        "INSERT INTO bar2 VALUES (3, 33, 33), (4, 44, 44)",
    ] {
        run(&mut s, q);
    }
    let plan = run(&mut s, "EXPLAIN (VERBOSE, COSTS OFF) UPDATE bar SET f2 = f2 + 100 RETURNING *");
    assert!(plan.iter().any(|l| l.contains("Remote SQL: UPDATE public.loct2 SET f2 = (f2 + 100) RETURNING f1, f2")), "{plan:#?}");
    let mut rows = run(&mut s, "UPDATE bar SET f2 = f2 + 100 RETURNING tableoid::regclass::text, *");
    rows.sort();
    assert_eq!(
        rows,
        vec![
            r#"[Text("bar"), Int4(1), Int4(111)]"#,
            r#"[Text("bar"), Int4(2), Int4(122)]"#,
            r#"[Text("bar2"), Int4(3), Int4(133)]"#,
            r#"[Text("bar2"), Int4(4), Int4(144)]"#,
        ]
    );
    let r = s.query("UPDATE bar SET f2 = f2 + 1 WHERE f1 > 1").unwrap();
    assert_eq!(r.last().unwrap().tag, "UPDATE 3");
    run(&mut s, "PREPARE st(int) AS DELETE FROM bar WHERE f1 = $1 RETURNING *");
    assert_eq!(run(&mut s, "EXECUTE st(4)"), vec!["[Int4(4), Int4(145)]"]);
    assert_eq!(run(&mut s, "SELECT f1, f2, f3 FROM loct2 ORDER BY f1"), vec!["[Int4(3), Int4(134), Int4(33)]"]);
    let r = s.query("DELETE FROM bar").unwrap();
    assert_eq!(r.last().unwrap().tag, "DELETE 3");
    assert!(run(&mut s, "SELECT * FROM loct2").is_empty());
}
