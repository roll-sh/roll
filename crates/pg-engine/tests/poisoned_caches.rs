//! Process-wide caches behind a lock (a system catalog's static rows, text search
//! dictionaries) keep working after a thread panicked while holding the lock: their critical
//! sections are one lookup or one insert, so a poisoned lock's map is still whole.

mod common;

use common::{engine_query, engine_session, open_store, render, statements};

fn run(sql: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "p");
    let mut out = String::new();
    for stmt in statements(sql) {
        out.push_str(&render(&stmt, &engine_query(&mut s, &stmt)));
    }
    out
}

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

const SQL: &str = "SELECT count(*) > 100 FROM pg_proc;
SELECT amname FROM pg_am WHERE amname = 'btree';
SELECT to_tsvector('english', 'The cats sat');
SELECT ts_lexize('english_stem', 'running');";

#[test]
fn poisoned_cache_locks_recover() {
    let before = on_thread(|| run(SQL));
    pg_engine::catalog::static_rows::poison_cache_for_test();
    pg_engine::types::tsearch::config::poison_dict_cache_for_test();
    let after = on_thread(|| run(SQL));
    assert_eq!(before, after);
    assert!(after.contains("'cat':2"), "{after}");
}

fn engine(dir: &std::path::Path) -> std::sync::Arc<pg_engine::engine::Engine> {
    pg_engine::engine::Engine::open(pg_engine::engine::EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
        idle_close: std::time::Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
    })
    .unwrap()
}

/// A Parse / Describe (an embedder's extended protocol) on a thread another session ran on,
/// before this session executed anything: `pg_temp` is this session's temporary schema.
#[test]
fn parse_describe_use_their_own_temp_schema() {
    on_thread(|| {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(dir.path());
        let mut sa = e.connect(2, "postgres", "postgres", "t").unwrap();
        for stmt in statements("CREATE TEMP TABLE a11_tt (x int); SELECT * FROM pg_temp.a11_tt") {
            let _ = engine_query(&mut sa, &stmt);
        }
        let mut sb = e.connect(3, "postgres", "postgres", "t").unwrap();
        let r = sb
            .parse_message("", "SELECT * FROM pg_temp.a11_tt", &[])
            .and_then(|p| sb.describe(&p));
        assert!(r.is_err(), "another session's temporary table was found");
        drop((sa, sb));
        e.shutdown();
    });
}
