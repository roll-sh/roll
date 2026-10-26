//! State that outlives a statement can't make a statement's output depend on what ran
//! before it: on this thread, in another database or engine, or earlier in the session.
//!
//! Each case runs a script `b` on a fresh thread (the reference output) and again after a
//! script `a` ran on the same thread: in a database of another store (process-wide caches
//! keyed by oids, which every new store hands out from the same start), in another database
//! of the same engine, or earlier in the same session (when `a` leaves nothing `b` can see).
//! The outputs must be the same.

mod common;

use common::{engine_query, engine_session, open_store, render, statements};
use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::time::Duration;

fn run(s: &mut Session, sql: &str) -> String {
    let mut out = String::new();
    for stmt in statements(sql) {
        out.push_str(&render(&stmt, &engine_query(s, &stmt)));
    }
    out
}

/// Run `f` on a fresh thread with a session's stack.
fn on_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            pg_engine::stack::set_limit(60 << 20);
            f()
        })
        .unwrap()
        .join()
        .unwrap()
}

/// `sql` in a database of its own (a store of its own, as the golden tests open).
fn alone(sql: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "b");
    run(&mut s, sql)
}

fn engine(dir: &std::path::Path) -> std::sync::Arc<Engine> {
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
    })
    .unwrap()
}

/// `b` in database `b` of an engine of its own, after `a` (when given) in its database `a`.
fn in_engine(a: Option<&str>, b: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    let mut admin = e.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut admin, "CREATE DATABASE a; CREATE DATABASE b");
    let sa = a.map(|a| {
        let mut sa = e.connect(2, "a", "postgres", "t").unwrap();
        run(&mut sa, a);
        sa
    });
    let mut sb = e.connect(3, "b", "postgres", "t").unwrap();
    let out = run(&mut sb, b);
    drop((admin, sa, sb));
    e.shutdown();
    out
}

/// `b`'s output: alone on a fresh thread, and on threads that ran `a` before it (1) in a
/// database of another store, (2) in another database of the same engine, and (3) when
/// `same_session`, earlier in the same session.
fn check(a: &'static str, b: &'static str, same_session: bool, expect: &[&str]) {
    let want = on_thread(move || alone(b));
    for e in expect {
        assert!(want.contains(e), "expected {e:?} in\n{want}");
    }
    let other_store = on_thread(move || {
        let _ = alone(a);
        alone(b)
    });
    assert_eq!(other_store, want, "after another store's\n{a}");
    let want_db = on_thread(move || in_engine(None, b));
    let other_db = on_thread(move || in_engine(Some(a), b));
    assert_eq!(other_db, want_db, "after the same engine's other database's\n{a}");
    if same_session {
        let after = on_thread(move || {
            let dir = tempfile::tempdir().unwrap();
            let mut s = engine_session(open_store(dir.path()), "b");
            let _ = run(&mut s, a);
            run(&mut s, b)
        });
        assert_eq!(after, want, "after, in the same session,\n{a}");
    }
}

/// The compression method of a stored value (pg_column_compression) is the one its own
/// database's column gave it.
#[test]
fn column_compression_per_database() {
    check(
        "CREATE TABLE t (c text COMPRESSION lz4);
         INSERT INTO t VALUES (repeat('abcdefgh', 1000));
         SELECT pg_column_compression(c) FROM t;",
        "CREATE TABLE t (c text COMPRESSION pglz);
         INSERT INTO t VALUES (repeat('abcdefgh', 1000));
         SELECT pg_column_compression(c) FROM t;
         CREATE TABLE u (c text COMPRESSION pglz);
         INSERT INTO u SELECT c FROM t;
         SELECT pg_column_compression(c) FROM u;",
        false,
        &["pglz"],
    );
}

/// A system catalog's estimated rows follow the catalog as DDL changes it (they were cached
/// by the catalog's address, which a later catalog can take once the earlier is dropped).
#[test]
fn catalog_estimates_follow_ddl() {
    on_thread(|| {
        let dir = tempfile::tempdir().unwrap();
        let mut s = engine_session(open_store(dir.path()), "b");
        let mut stale = Vec::new();
        for i in 0..16 {
            run(&mut s, &format!("CREATE TABLE t{i} (a int)"));
            let plan = run(&mut s, "EXPLAIN SELECT * FROM pg_class");
            let count = run(&mut s, "SELECT count(*) FROM pg_class");
            let rows = plan
                .split("rows=")
                .nth(1)
                .and_then(|r| r.split(' ').next())
                .unwrap_or_else(|| panic!("{plan}"))
                .to_string();
            if !count.lines().any(|l| l.trim() == rows) {
                stale.push(format!("after {} tables: estimate {rows}, actual\n{count}", i + 1));
            }
        }
        assert!(stale.is_empty(), "{}", stale.join("\n"));
    });
}

/// A session's statements reuse the system catalogs' rows an earlier one generated
/// (`SessionState::sys_rows`) only under the same catalog, roles, search path and settings:
/// DDL, a rolled-back DDL, SET ROLE, search_path and a setting each show their own rows.
#[test]
fn catalog_rows_follow_ddl_roles_and_settings() {
    check(
        "CREATE TABLE t0 (a int, b text DEFAULT 'x');
         SELECT attname FROM pg_attribute WHERE attrelid = 't0'::regclass AND attnum > 0;
         SELECT count(*) FROM pg_type WHERE typname = 'mood';
         SELECT relacl FROM pg_class WHERE relname = 't0';",
        "CREATE TABLE t (a int, b text DEFAULT 'x');
         SELECT attname, atttypid::regtype FROM pg_attribute WHERE attrelid = 't'::regclass AND attnum > 0 ORDER BY 1;
         ALTER TABLE t ADD COLUMN c numeric DEFAULT 1.5;
         SELECT attname, atttypid::regtype FROM pg_attribute WHERE attrelid = 't'::regclass AND attnum > 0 ORDER BY 1;
         SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 't'::regclass ORDER BY adnum;
         BEGIN;
         ALTER TABLE t DROP COLUMN b;
         SELECT attname FROM pg_attribute WHERE attrelid = 't'::regclass AND attnum > 0 AND NOT attisdropped ORDER BY 1;
         ROLLBACK;
         SELECT attname FROM pg_attribute WHERE attrelid = 't'::regclass AND attnum > 0 AND NOT attisdropped ORDER BY 1;
         CREATE SCHEMA s2;
         CREATE TYPE s2.mood AS ENUM ('ok');
         SELECT typname::regtype FROM pg_type WHERE typname = 'mood';
         SET search_path = s2, public;
         SELECT typname::regtype FROM pg_type WHERE typname = 'mood';
         RESET search_path;
         CREATE ROLE regress_sysrows_r;
         GRANT SELECT ON t TO regress_sysrows_r;
         SELECT relacl FROM pg_class WHERE relname = 't';
         SET ROLE regress_sysrows_r;
         SELECT relacl, has_table_privilege('t', 'SELECT') FROM pg_class WHERE relname = 't';
         RESET ROLE;
         REVOKE ALL ON t FROM regress_sysrows_r;
         DROP ROLE regress_sysrows_r;
         CREATE TABLE u (x int);
         SELECT relname, relkind FROM pg_class WHERE relname IN ('t', 'u') ORDER BY 1;",
        true,
        &["numeric", "1.5", "s2.mood", "regress_sysrows_r=r/"],
    );
}

/// A PL/pgSQL function's cached plans (`PLANS`), a SQL function's statements and a DO block's
/// follow the search path its statements set, and its CONTEXT signature is the one its
/// first run in the session formatted (`SessionState::pl_signatures`): `a` runs the same
/// bodies with its tables in the other schemas, under other paths, before `b`.
#[test]
fn routine_plans_follow_search_path() {
    check(
        "CREATE SCHEMA s1; CREATE SCHEMA s2;
         CREATE TABLE s1.t (x int); INSERT INTO s1.t VALUES (7);
         CREATE TABLE s2.t (x int); INSERT INTO s2.t VALUES (8);
         CREATE FUNCTION public.f() RETURNS text LANGUAGE plpgsql AS $$
         DECLARE r text := '';
         BEGIN
           FOR i IN 1..3 LOOP
             PERFORM set_config('search_path', CASE WHEN i % 2 = 0 THEN 's1' ELSE 's2' END, true);
             r := r || (SELECT x FROM t);
             RAISE NOTICE 'at %', r;
           END LOOP;
           RETURN r;
         END $$;
         CREATE FUNCTION public.g() RETURNS int LANGUAGE sql AS $$
           SELECT set_config('search_path', 's1', true);
           SELECT x FROM t
         $$;
         SET search_path = s2, public;
         BEGIN; SELECT public.f(), public.g(); ROLLBACK;
         DO $$ BEGIN PERFORM set_config('search_path', 's1', true); RAISE NOTICE '%', (SELECT x FROM t); END $$;
         SET search_path = s1, public;
         BEGIN; SELECT public.f(), public.g(); ROLLBACK;",
        "CREATE SCHEMA u1; CREATE SCHEMA u2;
         SET search_path = u1, public;
         CREATE TABLE u1.t (x int); INSERT INTO u1.t VALUES (1);
         CREATE TABLE u2.t (x int); INSERT INTO u2.t VALUES (2);
         CREATE FUNCTION public.f2() RETURNS text LANGUAGE plpgsql AS $$
         DECLARE r text := '';
         BEGIN
           FOR i IN 1..3 LOOP
             PERFORM set_config('search_path', CASE WHEN i % 2 = 0 THEN 'u1' ELSE 'u2' END, true);
             r := r || (SELECT x FROM t);
             RAISE NOTICE 'at %', r;
           END LOOP;
           RETURN r;
         END $$;
         CREATE FUNCTION public.g2() RETURNS int LANGUAGE sql AS $$
           SELECT set_config('search_path', 'u2', true);
           SELECT x FROM t
         $$;
         BEGIN; SELECT public.f2(), public.g2(); ROLLBACK;
         BEGIN; SELECT public.f2(); ROLLBACK;
         DO $$ BEGIN PERFORM set_config('search_path', 'u2', true); RAISE NOTICE '%', (SELECT x FROM t); END $$;
         SET search_path = u2;
         SELECT public.f2();",
        true,
        &["212", "function f2() line"],
    );
}

/// The system catalogs' rows a database's sessions share (`Database::sys_rows`) are one
/// session's only for another under the same catalog, roles, login, path, temporary schema
/// and setting values: each script of `scripts` runs in a session of its own, one after the
/// other, all in one database, and each must show what it shows in a database of its own
/// after only `setup`.
fn shared_rows_check(setup: &str, scripts: &[(&str, &str)]) {
    let fresh = |user: &str| {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(dir.path());
        let mut admin = e.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut admin, setup);
        (dir, e, admin, user.to_string())
    };
    // Together: every script in a session of the same database.
    let (_dir, e, admin, _) = fresh("postgres");
    let mut together = Vec::new();
    let mut sessions = Vec::new();
    for (i, (user, sql)) in scripts.iter().enumerate() {
        let mut s = e.connect(10 + i as i32, "postgres", user, "t").unwrap();
        together.push(run(&mut s, sql));
        // (Kept open: their temporary schemas and settings stay.)
        sessions.push(s);
    }
    drop((admin, sessions));
    e.shutdown();
    for ((user, sql), got) in scripts.iter().zip(together) {
        let (_dir, e, admin, user) = fresh(user);
        let mut s = e.connect(2, "postgres", &user, "t").unwrap();
        let want = run(&mut s, sql);
        drop((admin, s));
        e.shutdown();
        assert_eq!(got, want, "after the other sessions' scripts\n{sql}");
    }
}

#[test]
fn catalog_rows_shared_by_sessions_follow_their_state() {
    on_thread(|| {
        shared_rows_check(
            "CREATE TABLE p (k timestamptz, v text DEFAULT 'x') PARTITION BY RANGE (k);
             CREATE TABLE p1 PARTITION OF p FOR VALUES FROM ('2020-01-01 00:00+00') TO ('2021-01-01 00:00+00');
             CREATE SCHEMA s2;
             CREATE TABLE s2.p (a int);
             CREATE ROLE regress_shared_r LOGIN;
             GRANT SELECT ON p TO regress_shared_r;
             GRANT USAGE ON SCHEMA s2 TO regress_shared_r",
            &[
                (
                    "postgres",
                    "SET TimeZone = 'UTC';
                     SELECT relname, pg_get_expr(relpartbound, oid) FROM pg_class WHERE relname = 'p1';
                     SELECT oid::regclass, relacl FROM pg_class WHERE relname = 'p' ORDER BY 1;
                     CREATE TEMP TABLE tt (a int);
                     SELECT oid::regclass FROM pg_class WHERE oid IN ('tt'::regclass, 'p'::regclass) ORDER BY 1;
                     SELECT attrelid::regclass, attname FROM pg_attribute WHERE attname = 'a' AND attrelid IN ('tt'::regclass, 's2.p'::regclass) ORDER BY 1;",
                ),
                (
                    "postgres",
                    "SET TimeZone = 'Asia/Tokyo';
                     SET DateStyle = 'SQL, DMY';
                     SELECT relname, pg_get_expr(relpartbound, oid) FROM pg_class WHERE relname = 'p1';
                     CREATE TEMP TABLE tt (b int);
                     SELECT oid::regclass FROM pg_class WHERE oid IN ('tt'::regclass, 'p'::regclass) ORDER BY 1;
                     SET search_path = s2, public;
                     SELECT oid::regclass, relname FROM pg_class WHERE oid IN ('tt'::regclass, 's2.p'::regclass, 'public.p'::regclass) ORDER BY 2, 1;
                     SELECT attrelid::regclass, attname FROM pg_attribute WHERE attname IN ('a', 'b') AND attnum > 0 AND attrelid IN ('tt'::regclass, 's2.p'::regclass) ORDER BY 1, 2;",
                ),
                (
                    "regress_shared_r",
                    "SELECT relname, pg_get_expr(relpartbound, oid) FROM pg_class WHERE relname = 'p1';
                     SELECT oid::regclass, relacl, has_table_privilege(oid, 'SELECT') FROM pg_class WHERE relname = 'p' ORDER BY 1;
                     SELECT current_user, session_user;",
                ),
            ],
        );
    });
}

/// A made range type's constructors are its own database's type's.
#[test]
fn range_constructors_per_database() {
    check(
        "CREATE TYPE r AS RANGE (subtype = int4);
         SELECT r(1, 5);",
        "CREATE TYPE r AS RANGE (subtype = text);
         SELECT r('a', 'b');
         SELECT r('a', 'b', '[]');",
        false,
        &["[a,b)", "[a,b]"],
    );
}

/// A subscription's statistics reset time is its own cluster's (subscription oids repeat in
/// another engine or store of the process: `golden` and `golden_wire` raced on it). (Within
/// one engine the statistics view shows every database's subscriptions: `b` looks at its own.)
#[test]
fn subscription_stats_reset_per_cluster() {
    check(
        "CREATE SUBSCRIPTION s CONNECTION 'dbname=nowhere' PUBLICATION p WITH (connect = false);
         SELECT pg_stat_reset_subscription_stats(oid) FROM pg_subscription WHERE subname = 's';",
        "CREATE SUBSCRIPTION s CONNECTION 'dbname=nowhere' PUBLICATION p WITH (connect = false);
         SELECT subname, stats_reset IS NULL AS never FROM pg_stat_subscription_stats
           WHERE subid IN (SELECT s.oid FROM pg_subscription s JOIN pg_database d ON d.oid = s.subdbid
                           WHERE d.datname = current_database());",
        false,
        &["s | t"],
    );
}

/// Objects the panic cases below use.
const PANIC_SETUP: &str = "
CREATE TABLE t (a int, b text);
INSERT INTO t SELECT g, 'x' || g FROM generate_series(1, 20) g;
CREATE VIEW v AS SELECT a, CASE WHEN a > 1 THEN 'big' ELSE 'small' END AS c
  FROM (SELECT a FROM t WHERE a < 10) s;
CREATE DOMAIN d AS int;
CREATE FUNCTION sqlv(int) RETURNS int LANGUAGE sql VOLATILE AS $$ SELECT 1; SELECT $1 + 1 $$;
CREATE ROLE ru;
CREATE TABLE r (a int);
ALTER TABLE r ENABLE ROW LEVEL SECURITY;
CREATE POLICY p ON r USING (a > 0);
GRANT SELECT, UPDATE ON r TO ru;
INSERT INTO r VALUES (1), (2), (-1);
CREATE TABLE pt (a int, b int GENERATED ALWAYS AS (a * 2) STORED) PARTITION BY RANGE (a);
CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);
CREATE TABLE pt2 PARTITION OF pt FOR VALUES FROM (10) TO (20);
INSERT INTO pt (a) VALUES (1), (15);
CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'trg % %', TG_OP, NEW; RETURN NEW; END $$;
CREATE TRIGGER pt_b BEFORE INSERT OR UPDATE ON pt FOR EACH ROW EXECUTE FUNCTION trg();
CREATE TABLE inh_p (a int);
CREATE TABLE inh_c () INHERITS (inh_p);
CREATE INDEX ON inh_p (a);
CREATE INDEX ON inh_c (a);
CREATE TABLE rt (a int);
CREATE TABLE rt2 (a int);
CREATE RULE rr AS ON INSERT TO rt DO INSTEAD INSERT INTO rt2 VALUES (new.a) RETURNING *;
CREATE FUNCTION evt() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'evt %', tg_tag; END $$;
CREATE EVENT TRIGGER e ON ddl_command_start EXECUTE FUNCTION evt();
CREATE TABLE ct (a int);
CREATE INDEX ct_i ON ct (a);
ALTER TABLE ct OWNER TO ru;
";

/// What the session runs after the panic: statements that read the flags a panic could
/// leave set.
const PANIC_AFTER: &str = "
RESET ROLE;
RESET enable_seqscan;
SELECT 1;
SELECT current_user;
SHOW search_path;
ALTER TABLE pt DISABLE TRIGGER pt_b;
ALTER TABLE pt ENABLE TRIGGER pt_b;
EXPLAIN (COSTS OFF) SELECT * FROM t WHERE a = 1 AND b = 'x1';
SELECT pg_get_viewdef('v');
SELECT a, b FROM t WHERE b ~ 'x1' ORDER BY a;
SELECT sqlv(a) FROM t WHERE a < 3 ORDER BY 1;
SELECT CASE 1::d WHEN 1 THEN 'one' END;
SELECT * FROM t WHERE EXISTS (SELECT 1 FROM t t2 WHERE t2.a = t.a + 18) ORDER BY a;
UPDATE pt SET a = 5 WHERE a = 15;
INSERT INTO pt (a) VALUES (3);
SELECT * FROM pt ORDER BY a;
VACUUM t;
CREATE TABLE z (a int);
DROP TABLE z;
SET ROLE ru;
UPDATE r SET a = a + 10 WHERE a < 5;
EXPLAIN (COSTS OFF) SELECT * FROM r WHERE a < 15;
RESET ROLE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, row_number() OVER (ORDER BY a) FROM t;
EXPLAIN (COSTS OFF) SELECT * FROM inh_p ORDER BY a;
EXPLAIN (COSTS OFF) SELECT * FROM t FOR UPDATE;
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO rt VALUES (1) RETURNING *;
EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT a FROM t) SELECT * FROM c WHERE a = 1;
";

/// An engine panic in the middle of a statement (which the server turns into an ERROR, the
/// session going on) leaves nothing set for the session's next statements: they run as in a
/// session where the statement never ran. For each point where a thread-local flag or scratch
/// is set, a panic is made just after it is.
#[test]
fn panic_leaves_no_state() {
    // (point, statements before, the statement reaching the point)
    let cases: &[(&'static str, &'static str, &'static str)] = &[
        ("optimize::PHV_OUTPUTS", "", "SELECT * FROM t LEFT JOIN (SELECT a, a + 1 AS c FROM t) s ON t.a = s.a WHERE s.c > 3"),
        ("optimize::SECURITY_QUALS", "SET ROLE ru", "UPDATE r SET a = a WHERE a < 5"),
        ("optimize::QUERY_RLS", "SET ROLE ru", "SELECT * FROM r WHERE a < 5"),
        ("sublink::EXISTS_SUBLINKS", "", "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM t t2 WHERE t2.a = t.a)"),
        ("expr::CASE_TEST_DOMAIN", "", "SELECT CASE a WHEN 1 THEN 'one' END FROM t"),
        ("ruleutils::NO_INDENT", "", "EXPLAIN SELECT * FROM t WHERE a = 1"),
        ("ruleutils::RTABLES", "", "SELECT pg_get_viewdef('v')"),
        ("ddl::TOP_LEVEL", "", "CREATE TABLE z2 (a int)"),
        ("event_trigger::STATES", "", "CREATE TABLE z3 (a int)"),
        ("trigger::MOVING_ROW", "", "UPDATE pt SET a = 16 WHERE a = 1"),
        ("trigger::UPDATED_COLS", "", "UPDATE pt SET a = 2 WHERE a = 1"),
        ("scs::QUIET", "", "SELECT sqlv(1)"),
        ("regex::INTERRUPTS", "", "SELECT 'abc' ~ 'b'"),
        ("explain::FILTER_OVER_WINDOW", "", "EXPLAIN SELECT * FROM (SELECT a, row_number() OVER () AS r FROM t) s WHERE r < 3"),
        ("explain::RULE_RETURNING", "", "EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO rt VALUES (1) RETURNING *"),
        ("explain::WINDOW_TLIST", "", "EXPLAIN (VERBOSE, COSTS OFF) SELECT a, row_number() OVER (ORDER BY a) FROM t"),
        ("explain::MERGE_MEMBERS", "SET enable_seqscan = off", "EXPLAIN (COSTS OFF) SELECT * FROM inh_p ORDER BY a"),
        ("explain::INH_SCANS", "", "EXPLAIN SELECT * FROM inh_p"),
        ("explain::UNDER_LOCK_ROWS", "", "EXPLAIN SELECT * FROM t FOR UPDATE"),
        ("rowmarks::OWN_LOCK", "", "SELECT * FROM (SELECT * FROM t OFFSET 0) s FOR UPDATE OF s"),
        ("explain::CTE_ESTIMATES", "", "WITH c AS MATERIALIZED (SELECT a FROM t) SELECT * FROM c WHERE a = 1"),
        ("session::definer", "", "CLUSTER ct USING ct_i"),
        ("session::in_use", "", "UPDATE pt SET a = 2 WHERE a = 1"),
    ];
    let problems = on_thread(move || {
        let mut problems = Vec::new();
        for &(point, before, stmt) in cases {
            // As if the statement never ran.
            let want = {
                let dir = tempfile::tempdir().unwrap();
                let mut s = engine_session(open_store(dir.path()), "b");
                run(&mut s, PANIC_SETUP);
                run(&mut s, before);
                run(&mut s, PANIC_AFTER)
            };
            let dir = tempfile::tempdir().unwrap();
            let mut s = engine_session(open_store(dir.path()), "b");
            run(&mut s, PANIC_SETUP);
            run(&mut s, before);
            pg_engine::scoped::panic_at_for_test(Some(point));
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut s, stmt)));
            pg_engine::scoped::panic_at_for_test(None);
            if r.is_ok() {
                problems.push(format!("{point}: not reached by {stmt}"));
                continue;
            }
            // (What the server does with a panic.)
            s.recover_from_panic();
            s.notices.clear();
            let mut left = pg_engine::scoped::unsettled_for_test();
            left.extend(s.state.running_for_test());
            if !left.is_empty() {
                problems.push(format!("{point}: left set: {left:?}"));
            }
            let got = run(&mut s, PANIC_AFTER);
            if got != want {
                let at = want.lines().zip(got.lines()).position(|(x, y)| x != y).unwrap_or(0);
                let show = |t: &str| t.lines().skip(at.saturating_sub(3)).take(8).collect::<Vec<_>>().join("\n");
                problems.push(format!(
                    "{point}: the next statements differ:\n--- fresh\n{}\n--- after the panic\n{}",
                    show(&want),
                    show(&got)
                ));
            }
        }
        problems
    });
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}
