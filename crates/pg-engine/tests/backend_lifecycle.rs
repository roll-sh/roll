mod common;
use common::{engine_query, render, statements};
use pg_engine::{
    engine::{Engine, EngineOptions},
    session::Session,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let mut out = String::new();
    for stmt in statements(sql) {
        let part = render(&stmt, &engine_query(s, &stmt));
        assert!(!part.contains("ERROR"), "{part}");
        out.push_str(&part);
    }
    out.lines()
        .filter(|l| {
            !l.starts_with("> ") && !l.starts_with("-- ") && !l.is_empty() && !l.contains(':')
        })
        .map(str::to_owned)
        .collect()
}
fn open(dir: &std::path::Path) -> Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
        idle_close: Duration::from_secs(60),
        password: None,
        ..EngineOptions::default()
    })
    .unwrap()
}
fn on_thread(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn fast_own_transaction_waiters() {
    on_thread(|| {
        for snapshot in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let e = open(dir.path());
            let mut a = e.connect(1, "postgres", "postgres", "holder").unwrap();
            let mut observer = e.connect(3, "postgres", "postgres", "observer").unwrap();
            rows(
                &mut a,
                "CREATE TABLE cic1 (x int); CREATE TABLE cic2 (y int)",
            );
            rows(
                &mut a,
                if snapshot {
                    "BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT 1"
                } else {
                    "BEGIN; LOCK TABLE cic1 IN ROW EXCLUSIVE MODE"
                },
            );
            assert_eq!(
                rows(&mut a, "SELECT txid_current_if_assigned() IS NULL"),
                ["t"]
            );
            let waiter_engine = e.clone();
            let waiter = std::thread::Builder::new().stack_size(64 << 20).spawn(move || {
                let mut b = waiter_engine.connect(2, "postgres", "postgres", "waiter").unwrap();
                rows(&mut b, "SET statement_timeout = '8s'; CREATE INDEX CONCURRENTLY cic1_x ON cic1 ((x + 1))");
            }).unwrap();
            let start = Instant::now();
            let blocked = loop {
                let pids = rows(&mut observer, "SELECT pg_blocking_pids(2)::text");
                if pids == ["{1}"] {
                    break true;
                }
                if waiter.is_finished() || start.elapsed() >= Duration::from_secs(5) {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let locks = rows(
                &mut observer,
                "SELECT locktype, mode, granted FROM pg_locks WHERE pid IN (1,2) ORDER BY pid,locktype,mode,granted",
            );
            println!("snapshot={snapshot} blocked={blocked} locks={locks:?}");
            let waiting = rows(
                &mut observer,
                "SELECT count(*) > 0 FROM pg_locks WHERE pid=2 AND NOT granted",
            ) == ["t"];
            rows(&mut a, "COMMIT");
            waiter.join().unwrap();
            assert!(
                blocked,
                "CIC must wait for xid0 holder; snapshot={snapshot}"
            );
            assert!(
                waiting,
                "pg_locks must expose the waiter; snapshot={snapshot}"
            );
            assert_eq!(
                rows(&mut observer, "SELECT pg_blocking_pids(2)::text"),
                ["{}"]
            );
            drop((a, observer));
            e.shutdown();
        }
    });
}

#[test]
fn same_pid_old_session_end_preserves_new_idle_statistics() {
    // Embedded callers supply pids: ending an older session must not remove its replacement's slot.
    on_thread(|| {
        let dir = tempfile::tempdir().unwrap();
        let e = open(dir.path());
        let mut old = e.connect(1, "postgres", "postgres", "old").unwrap();
        rows(
            &mut old,
            "CREATE TABLE stslot (x int); SELECT pg_stat_force_next_flush()",
        );
        let mut new = e.connect(1, "postgres", "postgres", "new").unwrap();
        let mut observer = e.connect(3, "postgres", "postgres", "observer").unwrap();
        rows(&mut new, "SELECT pg_stat_force_next_flush()");
        rows(&mut new, "INSERT INTO stslot VALUES (1)");
        let q = "SELECT n_tup_ins FROM pg_stat_user_tables WHERE relname='stslot'";
        assert_eq!(rows(&mut observer, q), ["0"]);
        drop(old);
        std::thread::sleep(Duration::from_millis(10_500));
        let seen = rows(&mut observer, q);
        println!("same_pid_idle_seen={seen:?}");
        drop(new);
        assert_eq!(
            rows(&mut observer, q),
            ["1"],
            "new session end flushes its own slot"
        );
        assert_eq!(
            seen,
            ["1"],
            "old session end must retain the new session's idle-stat registration"
        );
        drop(observer);
        e.shutdown();
    });
}
