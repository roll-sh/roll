//! The same behaviour on both backends. Postgres runs when `ROLL_CODE_TEST_DATABASE` names a
//! server (`scripts/test-code.sh` starts one in Docker); SQLite always runs.

use code_sql::{Backend, Db, Error, Isolation, OptionalExtension, Target, Value, params};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    db: Db,
    _dir: tempfile::TempDir,
}

fn pg_url() -> Option<String> {
    std::env::var(code_sql::TEST_DATABASE_ENV).ok().filter(|u| !u.is_empty())
}

/// SQLite, plus Postgres when it's configured.
fn each(name: &str) -> Vec<Fixture> {
    let mut out = Vec::new();
    let dir = tempfile::tempdir().unwrap();
    out.push(Fixture { db: Db::open(&Target::Sqlite(dir.path().join("t.db"))).unwrap(), _dir: dir });
    if pg_url().is_some() {
        let dir = tempfile::tempdir().unwrap();
        out.push(Fixture { db: Db::open_or_test(&Target::Sqlite(dir.path().join("t.db")), name).unwrap(), _dir: dir });
    } else {
        eprintln!("{}: Postgres not configured; SQLite only", code_sql::TEST_DATABASE_ENV);
    }
    out
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v INTEGER NOT NULL, flag INTEGER NOT NULL DEFAULT 0, data BLOB, usd REAL);
    CREATE TABLE IF NOT EXISTS byte (repo TEXT NOT NULL, id BLOB NOT NULL, n INTEGER NOT NULL, PRIMARY KEY (repo, id)) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS log (seq INTEGER PRIMARY KEY AUTOINCREMENT, what TEXT NOT NULL);
    CREATE INDEX IF NOT EXISTS kv_v ON kv (v);
";

#[test]
fn values_round_trip_with_sqlite_types() {
    for f in each("values") {
        let db = &f.db;
        db.ensure_schema(SCHEMA).unwrap();
        db.execute("INSERT INTO kv (k, v, flag, data, usd) VALUES (?, ?, ?, ?, ?)", params!["a", 1i64, true, &[0u8, 1, 255][..], 0.25f64]).unwrap();
        db.execute("INSERT INTO kv (k, v, flag, data, usd) VALUES (?, ?, ?, ?, ?)", params!["b", u32::MAX, false, None::<Vec<u8>>, None::<f64>]).unwrap();
        let (v, flag, data, usd): (i64, bool, Vec<u8>, f64) =
            db.query_row("SELECT v, flag, data, usd FROM kv WHERE k = ?", ["a"], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
        assert_eq!((v, flag, data, usd), (1, true, vec![0, 1, 255], 0.25));
        let (v, data, usd): (u32, Option<Vec<u8>>, Option<f64>) =
            db.query_row("SELECT v, data, usd FROM kv WHERE k = ?", ["b"], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((v, data, usd), (u32::MAX, None, None));
        // SUM is NUMERIC on Postgres; COUNT is BIGINT; comparisons are booleans.
        let (sum, count, cmp): (i64, i64, bool) =
            db.query_row("SELECT SUM(v), COUNT(*), MAX(v) > 2 FROM kv", (), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((sum, count, cmp), (1 + u32::MAX as i64, 2, true));
        let avg: f64 = db.query_row("SELECT AVG(usd) FROM kv", (), |r| r.get(0)).unwrap();
        assert_eq!(avg, 0.25);
        // A bool binds into an integer column and compares as one.
        let n: i64 = db.query_row("SELECT COUNT(*) FROM kv WHERE flag = ?", [true], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        // ?N placeholders, reused.
        let k: String = db.query_row("SELECT k FROM kv WHERE v >= ?1 AND v <= ?1 + 0", [1i64], |r| r.get(0)).unwrap();
        assert_eq!(k, "a");
        // Wrong types are errors, not silent conversions.
        let e = db.query_row("SELECT k FROM kv WHERE k = ?", ["a"], |r| r.get::<_, i64>(0)).unwrap_err();
        assert!(matches!(e, Error::Conversion(_)), "{e}");
        // No row is NoRows, which `optional` turns into None.
        assert!(db.query_row("SELECT v FROM kv WHERE k = ?", ["zzz"], |r| r.get::<_, i64>(0)).optional().unwrap().is_none());
    }
}

#[test]
fn text_sorts_bytewise_like_sqlite() {
    for f in each("collation") {
        let db = &f.db;
        db.ensure_schema(SCHEMA).unwrap();
        for k in ["b", "B", "a", "_x", "a/b", "a.b", "Z", "é"] {
            db.execute("INSERT INTO kv (k, v) VALUES (?, 0)", [k]).unwrap();
        }
        let ks: Vec<String> = db.query("SELECT k FROM kv ORDER BY k", (), |r| r.get(0)).unwrap();
        assert_eq!(ks, ["B", "Z", "_x", "a", "a.b", "a/b", "b", "é"], "{:?}", db.backend());
    }
}

#[test]
fn insert_or_ignore_rowid_order_and_returning() {
    for f in each("inserts") {
        let db = &f.db;
        db.ensure_schema(SCHEMA).unwrap();
        db.write(|c| {
            assert_eq!(c.execute("INSERT OR IGNORE INTO byte (repo, id, n) VALUES (?, ?, ?)", params!["r", &b"x"[..], 1])?, 1);
            assert_eq!(c.execute("INSERT OR IGNORE INTO byte (repo, id, n) VALUES (?, ?, ?)", params!["r", &b"x"[..], 2])?, 0);
            Ok::<_, Error>(())
        })
        .unwrap();
        let n: i64 = db.query_row("SELECT n FROM byte WHERE repo = ? AND id = ?", params!["r", &b"x"[..]], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        for k in ["z", "y", "x"] {
            db.execute("INSERT INTO kv (k, v) VALUES (?, 0)", [k]).unwrap();
        }
        let ks: Vec<String> = db.query("SELECT k FROM kv ORDER BY rowid", (), |r| r.get(0)).unwrap();
        assert_eq!(ks, ["z", "y", "x"]);
        let seqs: Vec<i64> = db
            .write(|c| (0..3).map(|i| c.query_row("INSERT INTO log (what) VALUES (?) RETURNING seq", [format!("w{i}")], |r| r.get(0))).collect())
            .unwrap();
        assert_eq!(seqs, [1, 2, 3]);
        // Upserts are portable as ON CONFLICT … DO UPDATE with a qualified column.
        db.execute("INSERT INTO kv (k, v) VALUES ('z', 5) ON CONFLICT (k) DO UPDATE SET v = kv.v + excluded.v", ()).unwrap();
        assert_eq!(db.query_row("SELECT v FROM kv WHERE k = 'z'", (), |r| r.get::<_, i64>(0)).unwrap(), 5);
    }
}

#[test]
fn execute_many_pipelines_and_counts() {
    for f in each("many") {
        let db = &f.db;
        db.ensure_schema(SCHEMA).unwrap();
        let rows: Vec<(String, i64)> = (0..2500).map(|i| (format!("k{i:05}"), i)).collect();
        let n = db
            .write(|c| c.execute_many("INSERT INTO kv (k, v) VALUES (?, ?)", rows.iter().map(|(k, v)| [Value::Text(k.clone()), Value::Int(*v)])))
            .unwrap();
        assert_eq!(n, 2500);
        assert_eq!(db.query_row("SELECT SUM(v) FROM kv", (), |r| r.get::<_, i64>(0)).unwrap(), (0..2500).sum::<i64>());
    }
}

/// Read-then-write inside `write` is atomic: 16 threads incrementing one counter through a
/// SELECT and an UPDATE lose no increment (Postgres re-runs the ones that conflict).
#[test]
fn writes_are_serializable_and_retried() {
    for f in each("serial") {
        let db = Arc::new(f.db.clone());
        db.ensure_schema(SCHEMA).unwrap();
        db.execute("INSERT INTO kv (k, v) VALUES ('n', 0)", ()).unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..16 {
                let (db, runs) = (db.clone(), runs.clone());
                s.spawn(move || {
                    for _ in 0..25 {
                        db.write(|c| {
                            runs.fetch_add(1, Ordering::Relaxed);
                            let v: i64 = c.query_row("SELECT v FROM kv WHERE k = 'n'", (), |r| r.get(0))?;
                            c.execute("UPDATE kv SET v = ? WHERE k = 'n'", [v + 1])?;
                            Ok::<_, Error>(())
                        })
                        .unwrap();
                    }
                });
            }
        });
        assert_eq!(db.query_row("SELECT v FROM kv WHERE k = 'n'", (), |r| r.get::<_, i64>(0)).unwrap(), 400);
        let runs = runs.load(Ordering::Relaxed);
        eprintln!("{:?}: 400 increments took {runs} runs", db.backend());
        if db.backend() == Backend::Sqlite {
            assert_eq!(runs, 400);
        }
    }
}

/// An error from the closure rolls everything back; so does a statement that failed even if the
/// closure carried on (Postgres would otherwise silently roll back at COMMIT).
#[test]
fn failures_roll_back_and_are_never_reported_as_success() {
    for f in each("rollback") {
        let db = &f.db;
        db.ensure_schema(SCHEMA).unwrap();
        let r: Result<(), Error> = db.write(|c| {
            c.execute("INSERT INTO kv (k, v) VALUES ('x', 1)", ())?;
            Err(Error::Other("no".into()))
        });
        assert!(r.is_err());
        assert_eq!(db.query_row("SELECT COUNT(*) FROM kv", (), |r| r.get::<_, i64>(0)).unwrap(), 0);
        db.execute("INSERT INTO kv (k, v) VALUES ('dup', 1)", ()).unwrap();
        let r: Result<(), Error> = db.write(|c| {
            c.execute("INSERT INTO kv (k, v) VALUES ('y', 1)", ())?;
            let dup = c.execute("INSERT INTO kv (k, v) VALUES ('dup', 2)", ());
            assert!(dup.as_ref().unwrap_err().is_unique_violation(), "{dup:?}");
            Ok(())
        });
        match db.backend() {
            // SQLite keeps the transaction going after a failed statement.
            Backend::Sqlite => {
                r.unwrap();
                assert_eq!(db.query_row("SELECT COUNT(*) FROM kv WHERE k = 'y'", (), |r| r.get::<_, i64>(0)).unwrap(), 1);
            }
            // Postgres can't: the write fails loudly and nothing is written.
            Backend::Postgres => {
                let e = r.unwrap_err();
                assert!(e.to_string().contains("nothing was written"), "{e}");
                assert_eq!(db.query_row("SELECT COUNT(*) FROM kv WHERE k = 'y'", (), |r| r.get::<_, i64>(0)).unwrap(), 0);
            }
        }
        // A panic inside a write leaves the database usable.
        let db2 = db.clone();
        let _ = std::thread::spawn(move || db2.write(|c| -> Result<(), Error> {
            c.execute("INSERT INTO kv (k, v) VALUES ('p', 1)", ())?;
            panic!("boom")
        }))
        .join();
        db.execute("INSERT INTO kv (k, v) VALUES ('after', 1)", ()).unwrap();
        assert_eq!(db.query_row("SELECT COUNT(*) FROM kv WHERE k = 'p'", (), |r| r.get::<_, i64>(0)).unwrap(), 0);
    }
}

#[test]
fn schema_migrations_and_columns() {
    for f in each("schema") {
        let db = &f.db;
        // Many nodes ensuring the schema at once.
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| db.ensure_schema(SCHEMA).unwrap());
            }
        });
        assert!(db.has_column("kv", "usd").unwrap());
        assert!(!db.has_column("kv", "extra").unwrap());
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| db.add_column("kv", "extra", "TEXT NOT NULL DEFAULT ''").unwrap());
            }
        });
        assert!(db.has_column("kv", "extra").unwrap());
        let runs = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    db.migrate("fill-extra", |c| {
                        runs.fetch_add(1, Ordering::SeqCst);
                        c.execute("UPDATE kv SET extra = 'x'", ())?;
                        Ok(())
                    })
                    .unwrap()
                });
            }
        });
        assert_eq!(runs.load(Ordering::SeqCst), 1, "a migration runs once");
        let now = db.now_ms().unwrap();
        assert!((now - code_sql::now_ms()).abs() < 60_000);
    }
}

/// The API is synchronous but works from inside async tasks of either runtime flavour.
#[test]
fn works_from_async_tasks() {
    for f in each("async") {
        let db = f.db.clone();
        db.ensure_schema(SCHEMA).unwrap();
        let single = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let d = db.clone();
        single.block_on(async move {
            let tasks: Vec<_> = (0..8)
                .map(|i| {
                    let d = d.clone();
                    tokio::spawn(async move { d.execute("INSERT INTO kv (k, v) VALUES (?, ?)", params![format!("s{i}"), i]).unwrap() })
                })
                .collect();
            for t in tasks {
                t.await.unwrap();
            }
        });
        let multi = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let d = db.clone();
        multi.block_on(async move {
            let tasks: Vec<_> = (0..32)
                .map(|i| {
                    let d = d.clone();
                    tokio::spawn(async move { d.execute("INSERT INTO kv (k, v) VALUES (?, ?)", params![format!("m{i}"), i]).unwrap() })
                })
                .collect();
            for t in tasks {
                t.await.unwrap();
            }
        });
        assert_eq!(db.query_row("SELECT COUNT(*) FROM kv", (), |r| r.get::<_, i64>(0)).unwrap(), 40);
    }
}

/// Killed connections are replaced; reads and not-yet-committed writes are re-run.
#[test]
fn survives_connections_being_killed() {
    let Some(url) = pg_url() else { return };
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_or_test(&Target::Sqlite(dir.path().join("t.db")), "kill").unwrap();
    db.ensure_schema(SCHEMA).unwrap();
    db.execute("INSERT INTO kv (k, v) VALUES ('n', 0)", ()).unwrap();
    let admin = Db::connect(&url, "public", Default::default()).unwrap();
    let schema = db.pg_schema().unwrap().to_string();
    let kill = || {
        admin
            .query_row(
                "SELECT COUNT(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE application_name = ?",
                [format!("roll-code {schema}")],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
    };
    for round in 0..5 {
        let killed = kill();
        // The pool finds out on next use and re-runs.
        let v: i64 = db.query_row("SELECT v FROM kv WHERE k = 'n'", (), |r| r.get(0)).unwrap();
        assert_eq!(v, round);
        db.write(|c| c.execute("UPDATE kv SET v = v + 1 WHERE k = 'n'", ())).unwrap();
        eprintln!("round {round}: killed {killed} connections");
    }
    assert_eq!(db.query_row("SELECT v FROM kv WHERE k = 'n'", (), |r| r.get::<_, i64>(0)).unwrap(), 5);
}

#[test]
fn listen_wakes_on_notify() {
    let Some(_) = pg_url() else { return };
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_or_test(&Target::Sqlite(dir.path().join("t.db")), "listen").unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    rt.block_on(async {
        let mut rx = db.listen("code_test").unwrap();
        // The first wake-up comes once it's listening.
        tokio::time::timeout(std::time::Duration::from_secs(10), rx.changed()).await.unwrap().unwrap();
        let before = *rx.borrow_and_update();
        let d = db.clone();
        tokio::task::spawn_blocking(move || d.write(|c| c.query_row("SELECT pg_notify('code_test', 'x')", (), |_| Ok(()))).unwrap()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), rx.changed()).await.unwrap().unwrap();
        assert!(*rx.borrow() > before);
    });
}

#[test]
fn read_committed_writes_still_compare_and_swap() {
    for f in each("cas") {
        let db = Arc::new(f.db.clone());
        db.ensure_schema(SCHEMA).unwrap();
        db.execute("INSERT INTO kv (k, v) VALUES ('ref', 0)", ()).unwrap();
        let wins = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let n = db
                        .write_with(Isolation::ReadCommitted, |c| c.execute("UPDATE kv SET v = 1 WHERE k = 'ref' AND v = 0", ()))
                        .unwrap();
                    wins.fetch_add(n, Ordering::SeqCst);
                });
            }
        });
        assert_eq!(wins.load(Ordering::SeqCst), 1, "exactly one compare-and-swap wins");
    }
}
