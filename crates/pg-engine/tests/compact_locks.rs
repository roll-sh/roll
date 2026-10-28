//! Compact row and unique-key locks (`txn::bulklocks`): a transaction past a few thousand
//! locks keeps its next ones in compact sets, and other transactions see them exactly as
//! the lock manager's own entries: NOWAIT, SKIP LOCKED, lock_timeout, lock strengths, waits
//! for an in-progress unique key, savepoints, commit and rollback. (The isolation suite runs
//! with every lock compact as well: `ROLL_COMPACT_LOCKS_AFTER=0`.)

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::sync::Arc;
use std::time::Duration;

fn open(dir: &std::path::Path) -> Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 8 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 2,
        auth_files: Default::default(),
        settings: Vec::new(),
        server_programs: false,
    })
    .unwrap()
}

/// Run `sql`; Ok(the last statement's rows, rendered) or Err(its SQLSTATE).
fn q(s: &mut Session, sql: &str) -> Result<Vec<String>, String> {
    match s.query(sql) {
        Ok(r) => Ok(r
            .last()
            .map(|r| r.rows.iter().map(|row| format!("{row:?}")).collect())
            .unwrap_or_default()),
        Err(e) => Err(e.code.to_string()),
    }
}

fn ok(s: &mut Session, sql: &str) -> Vec<String> {
    q(s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn on_thread(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(64 << 20).spawn(f).unwrap().join().unwrap();
}

/// Locks taken compact so far, in this process.
fn compact_holds() -> u64 {
    pg_engine::txn::locks::COMPACT_HOLDS.load(std::sync::atomic::Ordering::Relaxed)
}

/// 6,000 rows; A locks or writes 5,000 of them, past the compact threshold.
fn setup(e: &Engine) -> (Session, Session) {
    let mut a = e.connect(1, "postgres", "postgres", "a").unwrap();
    let b = e.connect(2, "postgres", "postgres", "b").unwrap();
    ok(&mut a, "CREATE TABLE t (id int PRIMARY KEY, k int UNIQUE, v int)");
    ok(&mut a, "INSERT INTO t SELECT g, g, 0 FROM generate_series(1, 6000) g");
    (a, b)
}

#[test]
fn compact_row_locks_conflict_as_any() {
    on_thread(|| {
        let dir = tempfile::tempdir().unwrap();
        let e = open(dir.path());
        let (mut a, mut b) = setup(&e);
        ok(&mut a, "BEGIN");
        let before = compact_holds();
        ok(&mut a, "UPDATE t SET v = v + 1 WHERE id <= 5000");
        assert!(compact_holds() >= before + 2900, "the late row locks are compact");
        // Early rows (the usual entries) and late ones (compact) alike.
        for id in [10, 4500, 4999] {
            assert_eq!(q(&mut b, &format!("SELECT id FROM t WHERE id = {id} FOR UPDATE NOWAIT")), Err("55P03".into()), "{id}");
            assert_eq!(q(&mut b, &format!("SELECT id FROM t WHERE id = {id} FOR KEY SHARE NOWAIT")), Ok(vec![format!("[Int4({id})]")]), "{id}");
        }
        assert_eq!(ok(&mut b, "SELECT count(*) FROM (SELECT id FROM t WHERE id BETWEEN 4990 AND 5010 FOR UPDATE SKIP LOCKED) s"), ["[Int8(10)]"]);
        ok(&mut b, "SELECT id FROM t WHERE id = 5500 FOR UPDATE NOWAIT");
        ok(&mut b, "SET lock_timeout = '150ms'");
        assert_eq!(q(&mut b, "UPDATE t SET v = 7 WHERE id = 4321"), Err("55P03".into()));
        // A unique check of a key whose row the UPDATE changed waits for it too.
        assert_eq!(q(&mut b, "DELETE FROM t WHERE id = 4600"), Err("55P03".into()));
        ok(&mut a, "ROLLBACK");
        ok(&mut b, "UPDATE t SET v = 7 WHERE id = 4321");
        assert_eq!(ok(&mut b, "SELECT v FROM t WHERE id = 4321"), ["[Int4(7)]"]);
    });
}

#[test]
fn compact_key_share_and_savepoints() {
    on_thread(|| {
        let dir = tempfile::tempdir().unwrap();
        let e = open(dir.path());
        let (mut a, mut b) = setup(&e);
        ok(&mut b, "SET lock_timeout = '150ms'");
        ok(&mut a, "BEGIN");
        ok(&mut a, "SELECT count(*) FROM (SELECT id FROM t WHERE id <= 5000 FOR KEY SHARE) s");
        // FOR KEY SHARE lets a non-key update through, not a delete.
        ok(&mut b, "UPDATE t SET v = 1 WHERE id = 4500");
        assert_eq!(q(&mut b, "DELETE FROM t WHERE id = 4501"), Err("55P03".into()));
        // A stronger lock of the same rows, in a savepoint rolled back: the key share stays.
        ok(&mut a, "SAVEPOINT s");
        ok(&mut a, "SELECT count(*) FROM (SELECT id FROM t WHERE id <= 5000 FOR UPDATE) s");
        assert_eq!(q(&mut b, "UPDATE t SET v = 2 WHERE id = 4502"), Err("55P03".into()));
        ok(&mut a, "ROLLBACK TO s");
        ok(&mut b, "UPDATE t SET v = 2 WHERE id = 4502");
        assert_eq!(q(&mut b, "DELETE FROM t WHERE id = 4503"), Err("55P03".into()));
        // Released savepoint: its locks stay with the transaction until it ends.
        ok(&mut a, "SAVEPOINT r");
        ok(&mut a, "UPDATE t SET v = 9 WHERE id BETWEEN 5001 AND 6000");
        ok(&mut a, "RELEASE r");
        assert_eq!(q(&mut b, "SELECT id FROM t WHERE id = 5999 FOR SHARE NOWAIT"), Err("55P03".into()));
        ok(&mut a, "COMMIT");
        ok(&mut b, "DELETE FROM t WHERE id = 4503");
        ok(&mut b, "SELECT id FROM t WHERE id = 5999 FOR UPDATE NOWAIT");
    });
}

#[test]
fn compact_unique_keys_wait_for_their_writer() {
    on_thread(|| {
        let dir = tempfile::tempdir().unwrap();
        let e = open(dir.path());
        let (mut a, mut b) = setup(&e);
        ok(&mut b, "SET lock_timeout = '150ms'");
        ok(&mut a, "BEGIN");
        let before = compact_holds();
        ok(&mut a, "INSERT INTO t SELECT g, g, 0 FROM generate_series(10001, 15000) g");
        assert!(compact_holds() >= before + 2 * 2900, "the late keys are compact");
        // In-progress keys, early and compact: an insert of one waits for A.
        for id in [10002, 14000] {
            assert_eq!(q(&mut b, &format!("INSERT INTO t VALUES ({id}, -{id}, 1)")), Err("55P03".into()), "{id}");
            assert_eq!(q(&mut b, &format!("INSERT INTO t VALUES (-{id}, {id}, 1)")), Err("55P03".into()), "k {id}");
            assert_eq!(q(&mut b, &format!("INSERT INTO t VALUES ({id}, -{id}, 1) ON CONFLICT DO NOTHING")), Err("55P03".into()), "{id}");
        }
        ok(&mut b, "INSERT INTO t VALUES (20000, 20000, 1)");
        // An ON CONFLICT pre-check's wait names the other writer's row, compact or not
        // (XactLockTableWait's CONTEXT): where the heap puts it.
        let mut waits = Vec::new();
        for id in [10002, 14000] {
            let e = b.query(&format!("INSERT INTO t VALUES ({id}, -{id}, 1) ON CONFLICT (id) DO NOTHING")).unwrap_err();
            waits.push((id, e.context.unwrap_or_default().to_string()));
        }
        // A waiting writer gets the unique violation once A commits.
        let e2 = e.clone();
        let waiter = std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || {
                let mut c = e2.connect(3, "postgres", "postgres", "c").unwrap();
                q(&mut c, "INSERT INTO t VALUES (14500, -14500, 1)")
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        ok(&mut a, "COMMIT");
        assert_eq!(waiter.join().unwrap(), Err("23505".into()));
        // (Where PostgreSQL 18.6 puts the two rows: their ctids after the same inserts.)
        for ((id, context), tid) in waits.into_iter().zip(["(32,82)", "(54,10)"]) {
            assert_eq!(context, format!("while inserting index tuple {tid} in relation \"t\""), "{id}");
        }
        assert_eq!(q(&mut b, "INSERT INTO t VALUES (14000, -1, 1)"), Err("23505".into()));
        // Rolled back: the keys are free.
        ok(&mut a, "BEGIN");
        ok(&mut a, "INSERT INTO t SELECT g, g, 0 FROM generate_series(30001, 35000) g");
        ok(&mut a, "ROLLBACK");
        ok(&mut b, "INSERT INTO t VALUES (34000, 34000, 1)");
        // The same transaction inserting a key again after deleting it.
        ok(&mut a, "BEGIN");
        ok(&mut a, "INSERT INTO t SELECT g, g, 0 FROM generate_series(40001, 45000) g");
        ok(&mut a, "DELETE FROM t WHERE id = 44000");
        ok(&mut a, "INSERT INTO t VALUES (44000, 44000, 5)");
        assert_eq!(q(&mut a, "INSERT INTO t VALUES (44001, 1, 5)"), Err("23505".into()));
        ok(&mut a, "ROLLBACK");
        assert_eq!(ok(&mut b, "SELECT count(*) FROM t WHERE id > 40000"), ["[Int8(0)]"]);
    });
}

#[test]
fn compact_locks_prepare_and_finish() {
    on_thread(|| {
        let dir = tempfile::tempdir().unwrap();
        let e = open(dir.path());
        let (mut a, mut b) = setup(&e);
        ok(&mut b, "SET lock_timeout = '150ms'");
        ok(&mut a, "BEGIN");
        ok(&mut a, "UPDATE t SET v = 3 WHERE id <= 5000");
        let before = compact_holds();
        ok(&mut a, "INSERT INTO t SELECT g, g, 0 FROM generate_series(10001, 15000) g");
        assert!(compact_holds() >= before + 2 * 2900, "the late keys are compact");
        ok(&mut a, "PREPARE TRANSACTION 'p1'");
        // The prepared transaction keeps them all.
        assert_eq!(q(&mut b, "SELECT id FROM t WHERE id = 4999 FOR UPDATE NOWAIT"), Err("55P03".into()));
        assert_eq!(q(&mut b, "INSERT INTO t VALUES (14999, -1, 1)"), Err("55P03".into()));
        ok(&mut a, "COMMIT PREPARED 'p1'");
        ok(&mut b, "SELECT id FROM t WHERE id = 4999 FOR UPDATE NOWAIT");
        assert_eq!(q(&mut b, "INSERT INTO t VALUES (14999, -1, 1)"), Err("23505".into()));
    });
}
