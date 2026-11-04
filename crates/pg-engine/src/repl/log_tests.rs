use super::*;
use crate::engine::{Engine, EngineOptions};
use std::time::Duration;

fn options(dir: &std::path::Path) -> EngineOptions {
    EngineOptions {
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
        max_prepared_transactions: 4,
        auth_files: Default::default(),
        settings: vec![("wal_level".into(), "logical".into()), ("max_replication_slots".into(), "4".into())],
    }
}

/// The change stream's records in the store (change records, commit records), after a flush
/// and a compaction when `settle` (a row of unlogged table `u` written first, so the flush
/// leaves a file and the compaction rewrites the ones its keys fall in).
fn left(s: &mut crate::session::Session, settle: bool) -> (usize, usize) {
    let store = s.state.db.store.clone();
    if settle {
        s.query("INSERT INTO u VALUES (1) ON CONFLICT DO NOTHING; UPDATE u SET i = i").unwrap();
        store.flush_memtables().unwrap();
        store.compact().unwrap();
    }
    let ts = store.visible_ts();
    let (a, _) = changes_range(0);
    let (_, b) = changes_range(u64::MAX);
    let changes = store.scan(&a, &b, ts).count();
    let commits = store.scan(&commit_key(1), &commit_key(u64::MAX), ts).count();
    (changes, commits)
}

/// What every slot confirmed leaves the store with its next flush (records confirmed while
/// in the memtable never reach a file) or the next compaction of its file, without being
/// read back. A transaction still running (here a prepared one) holds the change records
/// from its own on, as it holds PostgreSQL's restart_lsn; and every record no slot
/// confirmed survives, across a restart too.
#[test]
fn confirmed_records_are_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let get = "SELECT count(*) FROM pg_logical_slot_get_changes('s', NULL, NULL, 'skip-empty-xacts', '1')";
    let count = |s: &mut crate::session::Session, sql: &str| format!("{:?}", s.query(sql).unwrap().pop().unwrap().rows);
    {
        let e = Engine::open(options(dir.path())).unwrap();
        let mut s = e.connect(1, "postgres", "postgres", "discard-test").unwrap();
        // (u first: its keys lie under t's, inside the files compaction rewrites.)
        s.query("CREATE UNLOGGED TABLE u (i int PRIMARY KEY); CREATE TABLE t (id int PRIMARY KEY, v text)").unwrap();
        s.query("SELECT pg_create_logical_replication_slot('s', 'test_decoding')").unwrap();
        s.query("BEGIN; INSERT INTO t VALUES (0, 'prepared'); PREPARE TRANSACTION 'g'").unwrap();
        for i in 1..=50 {
            s.query(&format!("INSERT INTO t VALUES ({i}, 'x'), ({}, 'y')", i + 1000)).unwrap();
        }
        let (changes, commits) = left(&mut s, false);
        assert!(changes >= 101 && commits == 51, "nothing confirmed yet: {changes} / {commits}");
        // BEGIN, two inserts and COMMIT each.
        assert_eq!(count(&mut s, get), "[[Int8(200)]]");
        let (held, commits) = left(&mut s, true);
        assert!(
            held == changes && commits == 0,
            "the prepared one holds the change records: {held} / {commits}; discarded, L0, L1: {:?}",
            s.state.db.store.discarded()
        );
        s.query("COMMIT PREPARED 'g'").unwrap();
        assert_eq!(count(&mut s, get), "[[Int8(3)]]");
        s.query("INSERT INTO t VALUES (2000, 'z')").unwrap();
        assert_eq!(count(&mut s, get), "[[Int8(3)]]");
        assert_eq!(left(&mut s, true), (0, 0), "the file holding them compacted");
        // Left unconfirmed over the restart.
        s.query("BEGIN; INSERT INTO t VALUES (3000, 'prepared'); PREPARE TRANSACTION 'g2'").unwrap();
        s.query("INSERT INTO t VALUES (3001, 'w')").unwrap();
        drop(s);
        e.shutdown();
    }
    let e = Engine::open(options(dir.path())).unwrap();
    let mut s = e.connect(1, "postgres", "postgres", "discard-test").unwrap();
    s.query("INSERT INTO t VALUES (3002, 'v')").unwrap();
    s.query("SELECT pg_logical_slot_get_changes('s', NULL, NULL)").unwrap();
    s.query("COMMIT PREPARED 'g2'").unwrap();
    let decoded = s.query("SELECT data FROM pg_logical_slot_peek_changes('s', NULL, NULL, 'skip-empty-xacts', '1')").unwrap().pop().unwrap().rows;
    let decoded = format!("{decoded:?}");
    assert!(decoded.contains("'prepared'") && !decoded.contains("'w'"), "{decoded}");
    assert_eq!(count(&mut s, get), "[[Int8(3)]]");
    s.query("INSERT INTO t VALUES (4000, 'u')").unwrap();
    assert_eq!(count(&mut s, get), "[[Int8(3)]]");
    assert_eq!(left(&mut s, true), (0, 0));
    // Without a slot nothing is logged, and what's left goes.
    s.query("INSERT INTO t VALUES (5000, 'w')").unwrap();
    s.query("SELECT pg_drop_replication_slot('s')").unwrap();
    s.query("INSERT INTO t VALUES (5001, 'w')").unwrap();
    assert_eq!(left(&mut s, true), (0, 0));
    drop(s);
    e.shutdown();
}
