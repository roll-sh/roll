//! Backend-status snapshots are transaction-local and separate from cumulative statistics.
mod common;

use common::{Event, engine_query};
use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Option<String>>> {
    engine_query(s, sql)
        .into_iter()
        .flat_map(|event| match event {
            Event::Result { rows, .. } => rows,
            Event::Notice(_) => Vec::new(),
            Event::Error(e) => panic!("{sql}: {e:?}"),
        })
        .collect()
}

fn status(s: &mut Session) -> Vec<Vec<Option<String>>> {
    rows(
        s,
        "SELECT s.* FROM pg_stat_get_backend_idset() bid, \
        LATERAL pg_stat_get_backend_subxact(bid) s WHERE pg_stat_get_backend_pid(bid) = pg_backend_pid()",
    )
}

fn expect(s: &mut Session, count: usize, overflowed: bool) {
    assert_eq!(
        status(s),
        vec![vec![
            Some(count.to_string()),
            Some(if overflowed { "t" } else { "f" }.into())
        ]]
    );
}

#[test]
fn backend_snapshot_lifecycle() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(|| {
        pg_engine::stack::set_limit(60 << 20);
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(EngineOptions {
            data_dir: dir.path().into(), cache_bytes: 4 << 20, memtable_bytes: 1 << 20,
            sync: false, ..Default::default()
        }).unwrap();
        let (pid, _, _) = engine.register_backend(None, None);
        let mut s = engine.connect(pid, "postgres", "postgres", "snapshot").unwrap();
        rows(&mut s, "CREATE TABLE sx (i int)");
        let bid = rows(&mut s, "SELECT bid FROM pg_stat_get_backend_idset() bid WHERE pg_stat_get_backend_pid(bid) = pg_backend_pid()")[0][0].clone().unwrap();
        for mode in ["none", "cache", "snapshot"] {
            rows(&mut s, &format!("SET stats_fetch_consistency = {mode}"));
            rows(&mut s, "BEGIN");
            rows(&mut s, &format!("SELECT pg_stat_get_backend_pid({bid})"));
            rows(&mut s, "SAVEPOINT root");
            rows(&mut s, "INSERT INTO sx VALUES (1)");
            expect(&mut s, 0, false);
            rows(&mut s, "SELECT pg_stat_clear_snapshot()");
            expect(&mut s, 1, false);
            rows(&mut s, "ROLLBACK TO root");
            expect(&mut s, 1, false);
            rows(&mut s, "SELECT pg_stat_clear_snapshot()");
            expect(&mut s, 0, false);
            rows(&mut s, "ROLLBACK");
            expect(&mut s, 0, false);

            rows(&mut s, "BEGIN");
            rows(&mut s, "SAVEPOINT root");
            for _ in 0..65 {
                rows(&mut s, "SAVEPOINT child");
                rows(&mut s, "INSERT INTO sx VALUES (2)");
                rows(&mut s, "RELEASE child");
            }
            expect(&mut s, 64, true);
            rows(&mut s, "ROLLBACK TO root");
            expect(&mut s, 64, true);
            rows(&mut s, "SELECT pg_stat_clear_snapshot()");
            expect(&mut s, 0, true);
            rows(&mut s, "INSERT INTO sx VALUES (3)");
            rows(&mut s, "SELECT pg_stat_clear_snapshot()");
            expect(&mut s, 1, true);
            rows(&mut s, "RELEASE root");
            rows(&mut s, "SELECT pg_stat_clear_snapshot()");
            expect(&mut s, 1, true);
            rows(&mut s, "COMMIT");
            expect(&mut s, 0, false);
        }
        rows(&mut s, "BEGIN");
        for _ in 0..64 {
            rows(&mut s, "SAVEPOINT child");
            rows(&mut s, "INSERT INTO sx VALUES (7)");
            rows(&mut s, "RELEASE child");
        }
        expect(&mut s, 64, false);
        rows(&mut s, "SAVEPOINT child");
        rows(&mut s, "INSERT INTO sx VALUES (8)");
        rows(&mut s, "RELEASE child");
        expect(&mut s, 64, false);
        rows(&mut s, "SELECT pg_stat_clear_snapshot()");
        expect(&mut s, 64, true);
        rows(&mut s, "ROLLBACK");
        expect(&mut s, 0, false);
        rows(&mut s, "SET stats_fetch_consistency = cache");
        rows(&mut s, "BEGIN");
        expect(&mut s, 0, false);
        rows(&mut s, "SAVEPOINT root");
        rows(&mut s, "INSERT INTO sx VALUES (4)");
        rows(&mut s, "SET stats_fetch_consistency = none");
        expect(&mut s, 0, false);
        rows(&mut s, "SET stats_fetch_consistency = cache");
        expect(&mut s, 0, false);
        rows(&mut s, "SELECT pg_stat_get_snapshot_timestamp()");
        expect(&mut s, 1, false);
        rows(&mut s, "ROLLBACK");
        // An autocommit SET must consume its deferred reset at transaction end.
        rows(&mut s, "SET stats_fetch_consistency = none");
        rows(&mut s, "SET stats_fetch_consistency = cache");
        rows(&mut s, "BEGIN");
        expect(&mut s, 0, false);
        rows(&mut s, "SAVEPOINT root");
        rows(&mut s, "INSERT INTO sx VALUES (5)");
        rows(&mut s, "SET stats_fetch_consistency = cache");
        rows(&mut s, "SELECT pg_stat_get_snapshot_timestamp()");
        expect(&mut s, 0, false);
        rows(&mut s, "ROLLBACK");

        // A library writer publishes at allocation/abort, without the wire server's
        // statement_done_activity report. Its transaction remains active throughout.
        let (writer_pid, _, _) = engine.register_backend(None, None);
        let mut writer = engine.connect(writer_pid, "postgres", "postgres", "writer").unwrap();
        let writer_bid = rows(&mut writer, "SELECT bid FROM pg_stat_get_backend_idset() bid WHERE pg_stat_get_backend_pid(bid) = pg_backend_pid()")[0][0].clone().unwrap();
        let other = format!("SELECT * FROM pg_stat_get_backend_subxact({writer_bid})");
        rows(&mut s, "BEGIN");
        assert_eq!(rows(&mut s, &other), vec![vec![Some("0".into()), Some("f".into())]]);
        rows(&mut writer, "BEGIN");
        rows(&mut writer, "SAVEPOINT child");
        rows(&mut writer, "INSERT INTO sx VALUES (6)");
        assert_eq!(rows(&mut s, &other), vec![vec![Some("0".into()), Some("f".into())]]);
        rows(&mut s, "SELECT pg_stat_clear_snapshot()");
        assert_eq!(rows(&mut s, &other), vec![vec![Some("1".into()), Some("f".into())]]);
        let failed = engine_query(&mut writer, "SELECT 1 / 0");
        assert!(failed.iter().any(|e| matches!(e, Event::Error(e) if e.code == "22012")));
        assert_eq!(rows(&mut s, &other), vec![vec![Some("1".into()), Some("f".into())]]);
        rows(&mut s, "SELECT pg_stat_clear_snapshot()");
        assert_eq!(rows(&mut s, &other), vec![vec![Some("0".into()), Some("f".into())]]);
        rows(&mut writer, "ROLLBACK TO child");
        assert_eq!(rows(&mut s, &other), vec![vec![Some("0".into()), Some("f".into())]]);
        rows(&mut s, "SELECT pg_stat_clear_snapshot()");
        assert_eq!(rows(&mut s, &other), vec![vec![Some("0".into()), Some("f".into())]]);
        rows(&mut writer, "ROLLBACK");
        rows(&mut s, "COMMIT");
        rows(&mut s, "BEGIN");
        assert_eq!(rows(&mut s, &other), vec![vec![Some("0".into()), Some("f".into())]]);
        drop(writer);
        engine.unregister_backend(writer_pid);
        assert_eq!(rows(&mut s, &format!("SELECT pg_stat_get_backend_pid({writer_bid})")), vec![vec![Some(writer_pid.to_string())]]);
        rows(&mut s, "SELECT pg_stat_clear_snapshot()");
        assert_eq!(rows(&mut s, &format!("SELECT pg_stat_get_backend_pid({writer_bid})")), vec![vec![None]]);
        rows(&mut s, "COMMIT");
        drop(s);
        engine.unregister_backend(pid);
        engine.shutdown();
    }).unwrap().join().unwrap();
}
