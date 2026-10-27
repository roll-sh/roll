//! kill -9 during big COPYs and UPDATEs: a child process (this test binary run again) loads
//! data in big transactions, each spilling its writes to disk (`txn::spill`) and committed as
//! an ingested SST, and is killed at a random moment; the database reopened must hold every
//! transaction whose COMMIT returned, none of the one cut short (all or nothing), indexes
//! agreeing with the rows, and no spilled files left.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

const ROWS: i64 = 20_000;

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
    })
    .unwrap()
}

fn one(s: &mut Session, sql: &str) -> String {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    format!("{:?}", r.last().unwrap().rows)
}

/// The child: batch k COPYs ids [k * ROWS, (k + 1) * ROWS) with n = 0 and then sets n = k
/// on every one of them by UPDATE, in one transaction; after its COMMIT returns, `k` goes
/// into the log (fsynced).
#[test]
fn crash_child() {
    let Ok(dir) = std::env::var("CRASH_BULK_DIR") else {
        return;
    };
    pg_engine::storage::set_default_txn_spill_bytes(256 << 10);
    let dir = Path::new(&dir);
    let eng = open(&dir.join("data"));
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    s.query("CREATE TABLE IF NOT EXISTS t (id int PRIMARY KEY, v text, n int)").unwrap();
    s.query("CREATE INDEX IF NOT EXISTS t_n ON t (n)").unwrap();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("committed"))
        .unwrap();
    // (After a kill, on from the last batch in the table.)
    let r = s.query("SELECT coalesce(max(id), 0) FROM t").unwrap();
    let first = match &r.last().unwrap().rows[0][0] {
        pg_engine::types::Value::Int4(m) => *m as i64 / ROWS + 1,
        v => panic!("{v:?}"),
    };
    // (Killed long before the last.)
    for k in first..first + 1000 {
        let mut data = String::new();
        for id in k * ROWS..(k + 1) * ROWS {
            data.push_str(&format!("{id}\tvalue {id} of batch {k}\t0\n"));
        }
        s.query("BEGIN").unwrap();
        s.query_copy("COPY t FROM STDIN", data.as_bytes()).unwrap();
        s.query(&format!("UPDATE t SET n = {k} WHERE id >= {} AND id < {}", k * ROWS, (k + 1) * ROWS))
            .unwrap();
        s.query("COMMIT").unwrap();
        writeln!(log, "{k}").unwrap();
        log.sync_all().unwrap();
    }
}

#[test]
fn kill_during_big_copy_and_update() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    for (round, ms) in [700u64, 1900, 3100].into_iter().enumerate() {
        let mut child = std::process::Command::new(&exe)
            .args(["--exact", "crash_child", "--nocapture", "--test-threads=1"])
            .env("CRASH_BULK_DIR", tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Wait for some batches to commit, then cut one short.
        let start = std::time::Instant::now();
        let committed = |p: &Path| std::fs::read_to_string(p.join("committed")).unwrap_or_default();
        while committed(tmp.path()).lines().count() < round + 1
            && start.elapsed() < Duration::from_secs(300)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(ms));
        child.kill().unwrap();
        child.wait().unwrap();

        let done: Vec<i64> = committed(tmp.path()).lines().map(|l| l.parse().unwrap()).collect();
        assert!(!done.is_empty(), "round {round}: nothing committed");
        let eng = open(&tmp.path().join("data"));
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        let max = *done.iter().max().unwrap();
        for k in 1..=max + 1 {
            let (lo, hi) = (k * ROWS, (k + 1) * ROWS);
            let got = one(&mut s, &format!("SELECT count(*), count(*) FILTER (WHERE n = {k}) FROM t WHERE id >= {lo} AND id < {hi}"));
            if done.contains(&k) {
                assert_eq!(got, format!("[[Int8({ROWS}), Int8({ROWS})]]"), "round {round} batch {k}");
            } else {
                // The batch cut short: all of it (its COMMIT returned, the log write didn't
                // happen) or none.
                assert!(
                    got == "[[Int8(0), Int8(0)]]" || got == format!("[[Int8({ROWS}), Int8({ROWS})]]"),
                    "round {round} batch {k}: {got}"
                );
            }
        }
        // The indexes agree with the rows.
        let rows = one(&mut s, "SELECT count(*), sum(n) FROM t");
        s.query("SET enable_seqscan = off").unwrap();
        s.query("SET enable_bitmapscan = off").unwrap();
        let by_n = one(&mut s, "SELECT count(*), sum(n) FROM t WHERE n >= 0");
        let by_id = one(&mut s, "SELECT count(*), sum(n) FROM t WHERE id >= 0");
        assert_eq!(rows, by_n, "round {round}");
        assert_eq!(rows, by_id, "round {round}");
        drop(s);
        eng.shutdown();
        // Nothing is left of the spilled writes.
        for d in std::fs::read_dir(tmp.path().join("data").join("base")).unwrap() {
            let spill = d.unwrap().path().join("spill");
            let left = std::fs::read_dir(&spill).map(|d| d.count()).unwrap_or(0);
            assert_eq!(left, 0, "round {round}: {}", spill.display());
        }
    }
}
