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
use std::time::{Duration, Instant};

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

/// A pushed join whose relation's whole row is read above it fetches that row as one ROW
/// (NULL for a null-extended row), and a value that doesn't convert names the whole-row
/// reference. A locking level with a local relation joins its foreign ones here, under
/// one LockRows over the whole level.
#[test]
fn whole_rows_and_locks_in_joins() {
    let (_data, engine) = loopback();
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    for q in [
        "CREATE TABLE t4 (c1 int, c2 int, c3 text)",
        "CREATE TABLE t5 (c1 int, c2 int, c3 text)",
        "INSERT INTO t4 SELECT i, i + 1, 'A' || i FROM generate_series(1, 6) i",
        "INSERT INTO t5 SELECT i * 2, i * 3, 'B' || i FROM generate_series(1, 3) i",
        "CREATE FOREIGN TABLE f4 (c1 int, c2 int, c3 text) SERVER loopback OPTIONS (table_name 't4')",
        "CREATE FOREIGN TABLE f5 (c1 int, c2 int, c3 text) SERVER loopback OPTIONS (table_name 't5')",
        "CREATE FOREIGN TABLE f4i (c1 int, c2 int, c3 int) SERVER loopback OPTIONS (table_name 't4')",
        "CREATE TABLE l (i int)",
        "INSERT INTO l VALUES (2), (3)",
    ] {
        run(&mut s, q);
    }
    let q = "SELECT f5, f5.c1, f4.c1 FROM f5 LEFT JOIN f4 ON f5.c1 = f4.c1 WHERE f4.c1 BETWEEN 2 AND 4 ORDER BY f5.c1, f4.c1";
    let plan = run(&mut s, &format!("EXPLAIN (VERBOSE, COSTS OFF) {q}"));
    assert!(
        plan.iter().any(|l| l.contains("Remote SQL: SELECT CASE WHEN (r1.*)::text IS NOT NULL THEN ROW(r1.c1, r1.c2, r1.c3) END, r1.c1, r2.c1 FROM")),
        "{plan:#?}"
    );
    assert_eq!(
        run(&mut s, q),
        vec![
            r#"[Record([Int4(2), Int4(3), Text("B1")]), Int4(2), Int4(2)]"#,
            r#"[Record([Int4(4), Int4(6), Text("B2")]), Int4(4), Int4(4)]"#,
        ]
    );
    let e = s.query("SELECT x.a, x FROM f4i x(a, b, c) JOIN f5 ON x.a = f5.c1 WHERE x.a = 2").expect_err("an error");
    assert_eq!(e.message, "invalid input syntax for type integer: \"A2\"");
    assert_eq!(e.context.as_deref(), Some("whole-row reference to foreign table \"x\""));
    let plan = run(&mut s, "EXPLAIN (COSTS OFF) SELECT l.i FROM l, f4, f5 WHERE l.i = f4.c1 AND f4.c1 = f5.c1 FOR UPDATE OF l");
    assert_eq!(plan.first().map(String::as_str), Some(r#"[Text("LockRows")]"#), "{plan:#?}");
}

#[test]
fn recheck_keeps_pushed_join_rows() {
    let (_data, engine) = loopback();
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    for q in [
        "CREATE TABLE l (i int PRIMARY KEY, v text)",
        "CREATE TABLE t1 (i int, v text)",
        "CREATE TABLE t2 (i int, v text)",
        "INSERT INTO l SELECT i, 'local' FROM generate_series(1, 1000) i",
        "INSERT INTO t1 SELECT i, 'a' || i FROM generate_series(1, 100) i",
        "INSERT INTO t2 SELECT i, 'b' || i FROM generate_series(1, 100) i",
        "CREATE FOREIGN TABLE f1 (i int, v text) SERVER loopback OPTIONS (table_name 't1')",
        "CREATE FOREIGN TABLE f2 (i int, v text) SERVER loopback OPTIONS (table_name 't2')",
        // (Rows costly to fetch: PostgreSQL then joins the foreign tables remotely too.)
        "ALTER SERVER loopback OPTIONS (ADD fdw_tuple_cost '10')",
        "ANALYZE l",
        "BEGIN",
        "UPDATE l SET v = 'updated'",
    ] {
        run(&mut s, q);
    }
    let e = engine.clone();
    let reader = std::thread::spawn(move || {
        let mut r = e.connect(2, "postgres", "postgres", "t").unwrap();
        run(&mut r, "SET enable_nestloop = off");
        run(&mut r, "SET enable_hashjoin = off");
        let q = "SELECT l.i, l.v, f1.i, f1.v, f2.v FROM l, f1, f2 WHERE f1.i = f2.i AND l.i = f1.i AND f1.i < 30 AND f2.i < 30 FOR UPDATE OF l";
        let plan = run(&mut r, &format!("EXPLAIN (VERBOSE, COSTS OFF) {q}"));
        (plan, run(&mut r, q))
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !s.state.db.locks.snapshot().iter().any(|l| l.pid == Some(2) && !l.granted) {
        assert!(Instant::now() < deadline, "reader never waited for the local row");
        std::thread::sleep(Duration::from_millis(10));
    }
    run(&mut s, "UPDATE t1 SET v = 'changed' WHERE i <= 3");
    run(&mut s, "DELETE FROM t2 WHERE i <= 3");
    run(&mut s, "COMMIT");
    let (plan, mut rows) = reader.join().unwrap();
    assert!(plan.iter().any(|l| l.contains("Relations:") && l.contains("f1") && l.contains("f2")), "{plan:#?}");
    rows.sort();
    let mut expected: Vec<_> = (1..30).map(|i| format!(r#"[Int4({i}), Text("updated"), Int4({i}), Text("a{i}"), Text("b{i}")]"#)).collect();
    expected.sort();
    assert_eq!(rows, expected);
}

#[test]
fn declare_opens_foreign_connections_before_fetch() {
    let (_data, engine) = loopback();
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    for q in [
        "CREATE TABLE t (i int)",
        "INSERT INTO t VALUES (1)",
        "CREATE FOREIGN TABLE ft (i int) SERVER loopback OPTIONS (table_name 't')",
        "BEGIN",
        "DECLARE c CURSOR FOR SELECT * FROM ft",
    ] {
        run(&mut s, q);
    }
    assert_eq!(run(&mut s, "SELECT server_name, used_in_xact FROM postgres_fdw_get_connections()"), vec![r#"[Text("loopback"), Bool(true)]"#]);
    run(&mut s, "ALTER SERVER loopback OPTIONS (SET dbname 'missing_database')");
    assert_eq!(run(&mut s, "FETCH ALL FROM c"), vec!["[Int4(1)]"]);
    run(&mut s, "ROLLBACK");
    run(&mut s, "BEGIN");
    run(&mut s, "ALTER SERVER loopback OPTIONS (SET dbname 'missing_database')");
    let e = s.query("DECLARE bad CURSOR FOR SELECT * FROM ft").expect_err("DECLARE must connect");
    assert_eq!(e.code, "08001");
    run(&mut s, "ROLLBACK");
    run(&mut s, "BEGIN");
    run(&mut s, "DECLARE good CURSOR FOR SELECT * FROM ft");
    assert_eq!(run(&mut s, "FETCH ALL FROM good"), vec!["[Int4(1)]"]);
    run(&mut s, "COMMIT");
}

#[test]
fn declared_cursor_uses_its_broken_connection() {
    let (_data, engine) = loopback();
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    for q in [
        "CREATE TABLE t (i int)",
        "INSERT INTO t VALUES (1)",
        "CREATE FOREIGN TABLE ft (i int) SERVER loopback OPTIONS (table_name 't')",
        "BEGIN",
        "DECLARE c CURSOR FOR SELECT i + 1 FROM ft WHERE random() >= 0",
        "SAVEPOINT s",
        "SELECT pg_terminate_backend(remote_backend_pid, 10000) FROM postgres_fdw_get_connections()",
        "ROLLBACK TO s",
    ] {
        run(&mut s, q);
    }
    s.query("SELECT * FROM ft").expect_err("new scans cannot reuse the broken transaction");
    run(&mut s, "ROLLBACK TO s");
    run(&mut s, "RELEASE s");
    let e = s.query("FETCH ALL FROM c").expect_err("the cursor's remote connection was terminated");
    assert_eq!(e.code, "08006", "{e:?}");
    assert!(e.context.as_deref().is_some_and(|c| c.contains("remote SQL command:")), "{e:?}");
    run(&mut s, "ROLLBACK");
}

/// A locking level with a local relation joins its foreign ones remotely (one remote query
/// returning each relation's whole row as its mark's copy), with the local join path as the
/// Foreign Scan's outer plan; an EvalPlanQual recheck after a concurrent update of the local
/// row runs that plan over the copies (it sends nothing: checked with the remote client's
/// writes traced, round 6).
#[test]
fn pushed_join_rechecks_with_outer_plan() {
    let (_data, engine) = loopback();
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    for q in [
        "CREATE TABLE l (i int, v text)",
        "CREATE TABLE t1 (i int, v text)",
        "CREATE TABLE t2 (i int, w text)",
        "CREATE FOREIGN TABLE ft1 (i int, v text) SERVER loopback OPTIONS (table_name 't1')",
        "CREATE FOREIGN TABLE ft2 (i int, w text) SERVER loopback OPTIONS (table_name 't2')",
        "INSERT INTO l VALUES (123, 'foo'), (456, 'bar')",
        "INSERT INTO t1 SELECT i, to_char(i, 'FM0000') FROM generate_series(1, 1000) i",
        "INSERT INTO t2 SELECT i, 'w' || i FROM generate_series(1, 1000, 3) i",
        "ANALYZE l",
        "ANALYZE ft1",
        "ANALYZE ft2",
        // (Rows costly to fetch: PostgreSQL then joins the foreign tables remotely too.)
        "ALTER SERVER loopback OPTIONS (ADD fdw_tuple_cost '10')",
    ] {
        run(&mut s, q);
    }
    run(&mut s, "BEGIN");
    run(&mut s, "UPDATE l SET v = v || 'x' WHERE i = 123");
    let e = engine.clone();
    let reader = std::thread::spawn(move || {
        let mut r = e.connect(2, "postgres", "postgres", "t").unwrap();
        run(&mut r, "SET enable_nestloop = off");
        run(&mut r, "SET enable_hashjoin = off");
        let q = "SELECT l.i, l.v, ft1.v, ft2.w FROM l, ft1, ft2 WHERE ft1.i = ft2.i AND ft1.i BETWEEN l.i AND l.i + 5 AND l.i = 123 FOR UPDATE OF l";
        let plan = run(&mut r, &format!("EXPLAIN (VERBOSE, COSTS OFF) {q}"));
        let rows = run(&mut r, q);
        (plan, rows)
    });
    std::thread::sleep(Duration::from_millis(500));
    run(&mut s, "COMMIT");
    let (plan, mut rows) = reader.join().unwrap();
    let text = plan.join("\n");
    assert!(text.contains("Relations: (public.ft1) INNER JOIN (public.ft2)"), "{plan:#?}");
    assert!(text.contains("Output: ft1.v, ft1.*, ft1.i, ft2.w, ft2.*"), "{plan:#?}");
    assert!(text.contains("->  Merge Join"), "{plan:#?}");
    rows.sort();
    assert_eq!(
        rows,
        vec![
            r#"[Int4(123), Text("foox"), Text("0124"), Text("w124")]"#,
            r#"[Int4(123), Text("foox"), Text("0127"), Text("w127")]"#,
        ]
    );
}

/// A never-analyzed foreign table is estimated as postgresGetForeignRelSize does: 10 pages
/// of rows as wide as the columns the query reads, costed with the wrapper's transfer
/// costs, and enable_seqscan doesn't disable its scan (PostgreSQL 18.6's numbers).
#[test]
fn unanalyzed_foreign_table_estimates() {
    let (_data, engine) = loopback();
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE TABLE base (id int PRIMARY KEY, v text, n numeric)");
    run(&mut s, "CREATE FOREIGN TABLE ft (id int, v text, n numeric) SERVER loopback OPTIONS (table_name 'base')");
    let mut plan = |q: &str| run(&mut s, &format!("EXPLAIN {q}")).join("\n");
    for (q, want) in [
        ("SELECT * FROM ft", "Foreign Scan on ft  (cost=100.00..305.80 rows=890 width=68)"),
        ("SELECT id FROM ft", "Foreign Scan on ft  (cost=100.00..753.50 rows=2925 width=4)"),
        ("SELECT * FROM ft WHERE id = 1", "Foreign Scan on ft  (cost=100.00..121.97 rows=4 width=68)"),
        ("SELECT count(*) FROM ft", "Foreign Scan  (cost=108.53..152.88 rows=1 width=8)"),
        ("SELECT count(*), sum(n) FROM ft WHERE id > 100", "Foreign Scan  (cost=102.44..130.93 rows=1 width=40)"),
    ] {
        let p = plan(q);
        assert!(p.contains(want), "{q}: {p}");
    }
    run(&mut s, "SET enable_seqscan = off");
    let p = run(&mut s, "EXPLAIN SELECT id FROM ft").join("\n");
    assert!(!p.contains("Disabled"), "{p}");
}

/// UPDATE ... FROM and DELETE ... USING over foreign tables of one server are sent as one
/// remote statement (postgresPlanDirectModify of a pushed join), RETURNING the other
/// relations' columns; a SET the remote side can't run updates per row over the pushed join
/// by `r1.ctid`. Plans and rows are PostgreSQL 18.6's.
#[test]
fn update_from_join_sent_whole() {
    let (_data, engine) = loopback();
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    for q in [
        "CREATE TABLE t1 (i int PRIMARY KEY, v text)",
        "CREATE TABLE t2 (i int PRIMARY KEY, w text)",
        "INSERT INTO t1 SELECT i, 'v' || i FROM generate_series(1, 100) i",
        "INSERT INTO t2 SELECT i, 'w' || i FROM generate_series(1, 50) i",
        "CREATE FOREIGN TABLE ft1 (i int, v text) SERVER loopback OPTIONS (table_name 't1')",
        "CREATE FOREIGN TABLE ft2 (i int, w text) SERVER loopback OPTIONS (table_name 't2')",
        "ANALYZE t1, t2, ft1, ft2",
    ] {
        run(&mut s, q);
    }
    let up = "UPDATE ft1 SET v = ft1.v || ft2.w FROM ft2 WHERE ft1.i = ft2.i AND ft2.i % 10 = 3 RETURNING ft1.i, ft1.v, ft2";
    let plan = run(&mut s, &format!("EXPLAIN (VERBOSE, COSTS OFF) {up}")).join("\n");
    assert!(plan.contains("->  Foreign Update\""), "{plan}");
    assert!(
        plan.contains("Remote SQL: UPDATE public.t1 r1 SET v = (r1.v || r2.w) FROM public.t2 r2 WHERE ((r1.i = r2.i)) AND (((r2.i % 10) = 3)) RETURNING r1.i, r1.v, CASE WHEN (r2.*)::text IS NOT NULL THEN ROW(r2.i, r2.w) END"),
        "{plan}"
    );
    let rows = run(&mut s, up);
    assert_eq!(rows.len(), 5, "{rows:#?}");
    assert!(rows[0].contains(r#"Int4(3), Text("v3w3")"#) && rows[0].contains("w3"), "{rows:#?}");
    let del = "DELETE FROM ft1 USING ft2 WHERE ft1.i = ft2.i + 50 AND ft2.i < 4 RETURNING ft1.*";
    let plan = run(&mut s, &format!("EXPLAIN (VERBOSE, COSTS OFF) {del}")).join("\n");
    assert!(
        plan.contains("Remote SQL: DELETE FROM public.t1 r1 USING public.t2 r2 WHERE ((r1.i = (r2.i + 50))) AND ((r2.i < 4)) RETURNING r1.i, r1.v"),
        "{plan}"
    );
    assert_eq!(run(&mut s, del).len(), 3);
    let per_row = "UPDATE ft1 SET v = CASE WHEN random() >= 0 THEN ft2.w ELSE '' END FROM ft2 WHERE ft1.i = ft2.i AND ft2.i < 3";
    let plan = run(&mut s, &format!("EXPLAIN (VERBOSE, COSTS OFF) {per_row}")).join("\n");
    assert!(plan.contains("Remote SQL: UPDATE public.t1 SET v = $2 WHERE ctid = $1"), "{plan}");
    assert!(plan.contains("Relations: (public.ft1) INNER JOIN (public.ft2)"), "{plan}");
    run(&mut s, per_row);
    let rows = run(&mut s, "SELECT * FROM t1 WHERE i < 5 OR i % 10 = 3 OR i BETWEEN 50 AND 54 ORDER BY i");
    let want = [
        (1, "w1"),
        (2, "w2"),
        (3, "v3w3"),
        (4, "v4"),
        (13, "v13w13"),
        (23, "v23w23"),
        (33, "v33w33"),
        (43, "v43w43"),
        (50, "v50"),
        (54, "v54"),
        (63, "v63"),
        (73, "v73"),
        (83, "v83"),
        (93, "v93"),
    ];
    let want: Vec<String> = want.iter().map(|(i, v)| format!(r#"[Int4({i}), Text("{v}")]"#)).collect();
    assert_eq!(rows, want);
    // (A condition that can't be sent: deleted per row by the pushed join's `r1.ctid`, the
    // RETURNING values those of the remote DELETE.)
    run(&mut s, "CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, PROCEDURE = int4eq, COMMUTATOR = ===)");
    let rows = run(&mut s, "DELETE FROM ft1 USING ft2 WHERE ft1.i = ft2.i AND ft2.i === 2 RETURNING ft1.i, ft1.v");
    assert_eq!(rows, vec![r#"[Int4(2), Text("w2")]"#.to_string()]);
}
