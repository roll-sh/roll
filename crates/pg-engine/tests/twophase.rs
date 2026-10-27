//! Two-phase commit across a restart: prepared transactions keep their writes, catalog
//! changes, locks and gids when the engine stops and opens again (their state records are in
//! their databases' stores), are recovered before any session connects, and finish there.
//! The SQL behaviour itself is golden `a71_twophase` (recorded from PostgreSQL).

mod common;

use common::{engine_query, render, statements};
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

fn engine(dir: &std::path::Path, max_prepared: usize) -> std::sync::Arc<Engine> {
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
        max_prepared_transactions: max_prepared,
        auth_files: Default::default(),
        settings: Vec::new(),
    })
    .unwrap()
}

/// The rows of each statement of `sql` (rendered `a | b`; header lines carry `name:type`).
fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    run(s, sql)
        .lines()
        .filter(|l| !l.starts_with("> ") && !l.starts_with("-- ") && !l.is_empty() && !l.contains(':'))
        .map(str::to_string)
        .collect()
}

fn on_thread(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(64 << 20).spawn(f).unwrap().join().unwrap();
}

#[test]
fn prepared_transactions_survive_a_restart() {
    on_thread(|| {
        let dir = tempfile::tempdir().unwrap();
        {
            let e = engine(dir.path(), 5);
            let mut a = e.connect(1, "postgres", "postgres", "t").unwrap();
            run(&mut a, "CREATE DATABASE d2");
            let mut s = e.connect(2, "d2", "postgres", "t").unwrap();
            let out = run(
                &mut s,
                "CREATE TABLE t (id int PRIMARY KEY, v text);
                 INSERT INTO t VALUES (1, 'a'), (2, 'b');
                 BEGIN; UPDATE t SET v = 'x' WHERE id = 1; INSERT INTO t VALUES (3, 'c');
                 CREATE TABLE t2 (a int); INSERT INTO t2 VALUES (7); PREPARE TRANSACTION 'g1';
                 BEGIN; SELECT * FROM t WHERE id = 2 FOR SHARE; PREPARE TRANSACTION 'g2';",
            );
            assert!(!out.contains("ERROR"), "{out}");
            drop((a, s));
            e.shutdown();
        }
        let e = engine(dir.path(), 5);
        // Recovered at startup: listed from another database before d2 is connected to.
        let mut a = e.connect(1, "postgres", "postgres", "t").unwrap();
        assert_eq!(
            rows(&mut a, "SELECT gid, database FROM pg_prepared_xacts ORDER BY gid"),
            ["g1 | d2", "g2 | d2"]
        );
        let mut s = e.connect(2, "d2", "postgres", "t").unwrap();
        // Their writes are still not seen, and their locks still block.
        assert_eq!(rows(&mut s, "SELECT * FROM t ORDER BY id"), ["1 | a", "2 | b"]);
        let blocked = run(
            &mut s,
            "SELECT * FROM t WHERE id = 1 FOR UPDATE NOWAIT;
             SELECT * FROM t WHERE id = 2 FOR UPDATE NOWAIT;
             SET lock_timeout = '50ms'; INSERT INTO t VALUES (3, 'dup');
             BEGIN; LOCK TABLE t IN ACCESS EXCLUSIVE MODE NOWAIT; ROLLBACK;
             SELECT * FROM t WHERE id = 2 FOR KEY SHARE NOWAIT",
        );
        assert_eq!(blocked.matches("could not obtain lock on row").count(), 2, "{blocked}");
        assert!(blocked.contains("canceling statement due to lock timeout"), "{blocked}");
        assert!(blocked.contains("could not obtain lock on relation \"t\""), "{blocked}");
        assert!(blocked.contains("2 | b"), "{blocked}");
        // A gid stays taken.
        let dup = run(&mut s, "RESET lock_timeout; BEGIN; PREPARE TRANSACTION 'g1'");
        assert!(dup.contains("transaction identifier \"g1\" is already in use"), "{dup}");
        let out = run(&mut s, "COMMIT PREPARED 'g1'; ROLLBACK PREPARED 'g2'");
        assert!(!out.contains("ERROR"), "{out}");
        assert_eq!(rows(&mut s, "SELECT * FROM t ORDER BY id"), ["1 | x", "2 | b", "3 | c"]);
        assert_eq!(rows(&mut s, "SELECT * FROM t2"), ["7"]);
        assert_eq!(rows(&mut s, "SELECT * FROM t WHERE id = 2 FOR UPDATE NOWAIT"), ["2 | b"]);
        assert_eq!(rows(&mut a, "SELECT count(*) FROM pg_prepared_xacts"), ["0"]);
        // A database with a prepared transaction can't be dropped.
        run(&mut s, "BEGIN; INSERT INTO t VALUES (4, 'd'); PREPARE TRANSACTION 'g3'");
        drop(s);
        let busy = run(&mut a, "DROP DATABASE d2");
        assert!(busy.contains("There is 1 prepared transaction using the database."), "{busy}");
        drop(a);
        e.shutdown();
        // Gone for good once finished: nothing left after another restart.
        let e = engine(dir.path(), 5);
        let mut s = e.connect(3, "d2", "postgres", "t").unwrap();
        assert_eq!(rows(&mut s, "SELECT gid FROM pg_prepared_xacts"), ["g3"]);
        run(&mut s, "COMMIT PREPARED 'g3'");
        drop(s);
        e.shutdown();
        let e = engine(dir.path(), 5);
        let mut s = e.connect(4, "d2", "postgres", "t").unwrap();
        assert_eq!(rows(&mut s, "SELECT count(*) FROM pg_prepared_xacts"), ["0"]);
        assert_eq!(rows(&mut s, "SELECT count(*) FROM t"), ["4"]);
        drop(s);
        e.shutdown();
    });
}

/// A prepared transaction that changed the cluster-wide catalog (roles) finishes after a
/// restart, also when other transactions changed roles and databases meanwhile: committed,
/// its roles are there with what it gave them; rolled back, they aren't.
#[test]
fn prepared_role_changes_finish_after_a_restart() {
    on_thread(|| {
        let dir = tempfile::tempdir().unwrap();
        {
            let e = engine(dir.path(), 5);
            let mut a = e.connect(1, "postgres", "postgres", "t").unwrap();
            let out = run(
                &mut a,
                "CREATE ROLE regress_tp_w;
                 BEGIN; CREATE ROLE regress_tp_q LOGIN; ALTER ROLE regress_tp_w CONNECTION LIMIT 3;
                 CREATE TABLE tp (i int); PREPARE TRANSACTION 'r1';
                 BEGIN; CREATE ROLE regress_tp_gone; PREPARE TRANSACTION 'r2';
                 CREATE ROLE regress_tp_z; CREATE DATABASE regress_tp_db; DROP DATABASE regress_tp_db;",
            );
            assert!(!out.contains("ERROR"), "{out}");
            drop(a);
            e.shutdown();
        }
        let e = engine(dir.path(), 5);
        let mut a = e.connect(1, "postgres", "postgres", "t").unwrap();
        let out = run(&mut a, "COMMIT PREPARED 'r1'; ROLLBACK PREPARED 'r2'");
        assert!(!out.contains("ERROR"), "{out}");
        assert_eq!(
            rows(&mut a, "SELECT rolname, rolcanlogin, rolconnlimit FROM pg_roles WHERE rolname LIKE 'regress_tp%' ORDER BY 1"),
            ["regress_tp_q | t | -1", "regress_tp_w | f | 3", "regress_tp_z | f | -1"]
        );
        assert_eq!(rows(&mut a, "SELECT count(*) FROM tp"), ["0"]);
        run(&mut a, "DROP TABLE tp; DROP ROLE regress_tp_q; DROP ROLE regress_tp_w; DROP ROLE regress_tp_z");
        drop(a);
        e.shutdown();
    });
}
