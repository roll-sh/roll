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
