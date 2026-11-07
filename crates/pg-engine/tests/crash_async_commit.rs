//! kill -9 during asynchronous commits (`synchronous_commit = off`, `Store::commit_async`): a
//! child process (this test binary run again) commits small transactions without waiting
//! for their WAL fsync, with a synchronous one now and then, and is killed at a random
//! moment. The database reopened must hold a prefix of the commits (none after one that's
//! missing, each transaction whole), every synchronous commit whose COMMIT returned and
//! everything before it; and, once the child has gone quiet for three times
//! wal_writer_delay, every commit (PostgreSQL's bound on what an asynchronous commit loses).

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

fn open(dir: &Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 8 << 20,
        memtable_bytes: 1 << 20,
        sync: true,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        outbound: Vec::new(),
        locale: Default::default(),
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: Vec::new(),
    })
    .unwrap()
}

fn int(s: &mut Session, sql: &str) -> i64 {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    match &r.last().unwrap().rows[0][0] {
        pg_engine::types::Value::Int4(v) => *v as i64,
        pg_engine::types::Value::Int8(v) => *v,
        v => panic!("{sql}: {v:?}"),
    }
}

/// The child: transaction k inserts row k into `c` and sets `ctr.n = k`; every 37th is
/// synchronous, and once its COMMIT returns "s k" goes into the log (fsynced). With
/// CRASH_ASYNC_PAUSE=i it logs "p k" after its i-th transaction and stops committing.
#[test]
fn crash_async_child() {
    let Ok(dir) = std::env::var("CRASH_ASYNC_DIR") else {
        return;
    };
    let pause: Option<i64> = std::env::var("CRASH_ASYNC_PAUSE").ok().map(|v| v.parse().unwrap());
    let dir = Path::new(&dir);
    let eng = open(&dir.join("data"));
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    s.query("CREATE TABLE IF NOT EXISTS c (id int PRIMARY KEY, k int)").unwrap();
    s.query("CREATE TABLE IF NOT EXISTS ctr (id int PRIMARY KEY, n int)").unwrap();
    s.query("INSERT INTO ctr VALUES (1, 0) ON CONFLICT DO NOTHING").unwrap();
    s.query("SET synchronous_commit = off").unwrap();
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("log")).unwrap();
    let first = int(&mut s, "SELECT n FROM ctr") + 1;
    for k in first..first + 1_000_000 {
        let sync = k % 37 == 0;
        s.query("BEGIN").unwrap();
        if sync {
            s.query("SET LOCAL synchronous_commit = on").unwrap();
        }
        s.query(&format!("INSERT INTO c VALUES ({k}, {k})")).unwrap();
        s.query(&format!("UPDATE ctr SET n = {k}")).unwrap();
        s.query("COMMIT").unwrap();
        if sync {
            writeln!(log, "s {k}").unwrap();
            log.sync_all().unwrap();
        }
        if pause == Some(k - first) {
            writeln!(log, "p {k}").unwrap();
            log.sync_all().unwrap();
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}

#[test]
fn kill_during_async_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let log = |p: &Path| std::fs::read_to_string(p.join("log")).unwrap_or_default();
    let last = |p: &Path, tag: &str| {
        log(p)
            .lines()
            .filter_map(|l| l.strip_prefix(tag).map(|k| k.parse::<i64>().unwrap()))
            .max()
    };
    // Killed at a random moment of a stream of commits (after some synchronous ones), and
    // once while quiet after the commit it names.
    for (round, ms) in [(0, 150u64), (1, 420), (2, 0), (3, 90)] {
        let paused = round == 2;
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["--exact", "crash_async_child", "--nocapture", "--test-threads=1"])
            .env("CRASH_ASYNC_DIR", tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let before = last(tmp.path(), "s ").unwrap_or(0);
        if paused {
            cmd.env("CRASH_ASYNC_PAUSE", "200");
        }
        let mut child = cmd.spawn().unwrap();
        let start = std::time::Instant::now();
        let ready = || {
            if paused {
                last(tmp.path(), "p ").is_some()
            } else {
                last(tmp.path(), "s ").unwrap_or(0) >= before + 74
            }
        };
        while !ready() && start.elapsed() < Duration::from_secs(300) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ready(), "round {round}: the child didn't get going");
        // (Quiet: three times wal_writer_delay, and some.)
        std::thread::sleep(Duration::from_millis(if paused { 800 } else { ms }));
        child.kill().unwrap();
        child.wait().unwrap();

        let eng = open(&tmp.path().join("data"));
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        let n = int(&mut s, "SELECT n FROM ctr");
        // A prefix of the commits, each whole.
        assert_eq!(int(&mut s, "SELECT count(*) FROM c"), n, "round {round}");
        assert_eq!(int(&mut s, "SELECT coalesce(max(id), 0) FROM c"), n, "round {round}");
        assert_eq!(int(&mut s, "SELECT count(*) FROM c WHERE id <> k"), 0, "round {round}");
        // Every synchronous commit that returned, and what came before it.
        let synced = last(tmp.path(), "s ").unwrap();
        assert!(n >= synced, "round {round}: {n} < synced {synced}");
        if paused {
            let p = last(tmp.path(), "p ").unwrap();
            assert_eq!(n, p, "round {round}: quiet for 3 x wal_writer_delay, yet commits lost");
            // (The next rounds pause no more.)
            std::fs::write(
                tmp.path().join("log"),
                log(tmp.path()).lines().filter(|l| !l.starts_with("p ")).map(|l| format!("{l}\n")).collect::<String>(),
            )
            .unwrap();
        }
        drop(s);
        eng.shutdown();
    }
}
