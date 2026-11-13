//! Block format 2 (`storage::sst`, HANDOFF "Storage format v2"): a store whose files are all of
//! format 1, written by the build before it (`tests/data/storage_format1.tar.gz`, from
//! `storage_format1.sql`), opens and runs with every feature; what this build writes is format
//! 2, the files it doesn't rewrite stay format 1 and are read as before (mixed levels), and a
//! kill -9 while compactions turn format 1 files into format 2 loses nothing.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

fn open(dir: &Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 256 << 10,
        sync: true,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: Vec::new(),
    })
    .unwrap()
}

fn one(s: &mut Session, sql: &str) -> String {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    format!("{:?}", r.last().unwrap().rows)
}

fn unpack(dir: &Path) {
    let tgz = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/storage_format1.tar.gz");
    let st = std::process::Command::new("tar").arg("xzf").arg(tgz).arg("-C").arg(dir).status().unwrap();
    assert!(st.success());
}

/// The block formats of the database's files, by the magic their footers end with: (format
/// 1, format 2).
fn formats(dir: &Path) -> (usize, usize) {
    let (mut one, mut two) = (0, 0);
    for db in std::fs::read_dir(dir.join("base")).unwrap() {
        for f in std::fs::read_dir(db.unwrap().path()).unwrap() {
            let p = f.unwrap().path();
            if p.extension().is_some_and(|x| x == "sst") {
                let b = std::fs::read(&p).unwrap();
                match &b[b.len() - 8..] {
                    m if m == b"1tssllor" => one += 1,
                    m if m == b"2tssllor" => two += 1,
                    m => panic!("{}: {m:?}", p.display()),
                }
            }
        }
    }
    (one, two)
}

/// What `storage_format1.sql` left, as the build that made the fixture answered (the
/// queries' results then, from `make.sh`), with `extra` rows of f added since.
fn check_fixture(s: &mut Session, what: &str) {
    let t = |s: &mut Session, q: &str| one(s, &format!("SELECT concat_ws(',', {q})"));
    assert_eq!(
        t(s, "count(*), sum(id), sum(n), sum(length(v)), sum(length(big))) FROM f WHERE id <= 8000 AND n >= 0"),
        "[[Text(\"6858,27435429,2625429,67630,103500\")]]",
        "{what}"
    );
    assert_eq!(t(s, "count(*), sum(k), round(sum(w)::numeric, 6), sum(length(s))) FROM g"), "[[Text(\"5000,37492500,1785357.142857,48890\")]]", "{what}");
    assert_eq!(t(s, "count(*), sum(a)) FROM h"), "[[Text(\"1,7\")]]", "{what}");
    // Every index agrees with the table, read alone.
    s.query("SET enable_seqscan = off").unwrap();
    s.query("SET enable_bitmapscan = off").unwrap();
    for (q, want) in [
        ("count(*), sum(n)) FROM f WHERE id > 0 AND id <= 8000 AND n >= 0", "6858,2625429"),
        ("count(*), sum(id)) FROM f WHERE n >= 0 AND id <= 8000", "6858,27435429"),
        ("count(*), sum(id)) FROM f WHERE v >= '' AND id <= 8000 AND n >= 0", "6858,27435429"),
        ("count(*), sum(k)) FROM g WHERE k >= 0", "5000,37492500"),
    ] {
        let want = format!("[[Text(\"{want}\")]]");
        let q = &format!("SELECT concat_ws(',', {q}");
        assert_eq!(one(s, q), want, "{what}: {q}");
    }
    s.query("RESET enable_seqscan").unwrap();
    s.query("RESET enable_bitmapscan").unwrap();
    assert_eq!(one(s, "SELECT length(big), substr(big, 1, 6) FROM f WHERE id = 4500"), "[[Int4(7500), Text(\"x4500x\")]]", "{what}");
    assert_eq!(one(s, "SELECT n, v FROM f WHERE id = 3000"), "[[Int4(1000), Text(\"value 3000\")]]", "{what}");
    assert_eq!(one(s, "SELECT count(*) FROM f WHERE id = 7"), "[[Int8(0)]]", "{what}");
}

/// The format 1 store opens and reads as it was written; writes, an index build, updates,
/// deletes, VACUUM and the compactions they make go on in it, format 2 files next to format 1
/// ones; it reads the same after a restart, and after every row is rewritten.
#[test]
fn format_1_store_reads_and_upgrades() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(reads_and_upgrades).unwrap().join().unwrap();
}

fn reads_and_upgrades() {
    let tmp = tempfile::tempdir().unwrap();
    unpack(tmp.path());
    assert_eq!(formats(tmp.path()).1, 0, "the fixture holds format 2 files");
    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    check_fixture(&mut s, "as written");
    // Writes beside the old files: new rows, an index over the old ones, updates and deletes of
    // old rows (their versions and tombstones in new files above the old ones).
    s.query("INSERT INTO f SELECT i, 'new ' || i, -1, NULL FROM generate_series(8001, 12000) i").unwrap();
    s.query("CREATE INDEX g_s ON g (s)").unwrap();
    s.query("UPDATE f SET big = big WHERE id % 500 = 0").unwrap();
    s.query("UPDATE f SET v = v WHERE id <= 8000 AND id % 11 = 0").unwrap();
    s.query("DELETE FROM f WHERE id > 8000 AND id % 2 = 0").unwrap();
    s.query("VACUUM f").unwrap();
    check_fixture(&mut s, "after writes");
    assert_eq!(one(&mut s, "SELECT count(*), sum(id) FROM f WHERE id > 8000"), "[[Int8(2000), Int8(20000000)]]");
    assert_eq!(one(&mut s, "SELECT count(*) FROM g WHERE s >= 'g row 4' AND s < 'g row 5'"), "[[Int8(1111)]]");
    drop(s);
    eng.shutdown();
    // (Mixed: what was written since is format 2, files nothing rewrote are format 1.)
    let (f1, f2) = formats(tmp.path());
    assert!(f1 > 0 && f2 > 0, "format 1: {f1} files, format 2: {f2}");

    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    check_fixture(&mut s, "reopened");
    assert_eq!(one(&mut s, "SELECT count(*), sum(id) FROM f WHERE id > 8000"), "[[Int8(2000), Int8(20000000)]]");
    // Every row of every table rewritten (versions over each file of theirs, compacted).
    for sql in ["UPDATE f SET n = n WHERE true", "UPDATE g SET k = k WHERE true", "UPDATE h SET a = a WHERE true", "VACUUM"] {
        s.query(sql).unwrap();
    }
    check_fixture(&mut s, "rewritten");
    drop(s);
    eng.shutdown();
    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    check_fixture(&mut s, "rewritten, reopened");
    drop(s);
    eng.shutdown();
}

/// The child of `kill_during_upgrade_compactions`: on the fixture's database, batch k (one
/// transaction) COPYs rows of f above the fixture's (n = -k), deletes the even ones of batch
/// k - 1 and updates a few of the fixture's own rows (versions over format 1 files): the compactions that
/// follow merge the format 1 files with the new ones into format 2. After its COMMIT returns,
/// `k` goes into the log (fsynced).
#[test]
fn upgrade_child() {
    let Ok(dir) = std::env::var("CRASH_UPGRADE_DIR") else {
        return;
    };
    let dir = Path::new(&dir);
    let eng = pg_engine::storage::with_txn_spill_bytes(128 << 10, || open(&dir.join("data")));
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("committed")).unwrap();
    let first = match &s.query("SELECT coalesce(max(id), 0) FROM f").unwrap().last().unwrap().rows[0][0] {
        pg_engine::types::Value::Int4(m) => (*m as i64).max(8000) / 10_000 + 1,
        v => panic!("{v:?}"),
    };
    for k in first..first + 1000 {
        let mut rows = String::new();
        for id in k * 10_000..k * 10_000 + 6_000 {
            rows.push_str(&format!("{id}\tbatch {k}\t-{k}\t\\N\n"));
        }
        s.query("BEGIN").unwrap();
        s.query_copy("COPY f FROM STDIN", rows.as_bytes()).unwrap();
        s.query(&format!("DELETE FROM f WHERE n = -{} AND id > 8000 AND id % 2 = 0", k - 1)).unwrap();
        // (The fixture's rows keep their values: updated to themselves, versions over them.)
        s.query(&format!("UPDATE f SET v = v WHERE id <= 8000 AND id % 97 = {}", k % 97)).unwrap();
        s.query("COMMIT").unwrap();
        writeln!(log, "{k}").unwrap();
        log.sync_all().unwrap();
    }
}

/// kill -9 while compactions merge the format 1 store's files with new ones into format 2:
/// the fixture's rows are all there and unchanged, every batch whose COMMIT returned is there
/// with its predecessor's even ids gone, the one cut short all or nothing, and the indexes
/// agree with the rows.
#[test]
fn kill_during_upgrade_compactions() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(upgrade_compactions).unwrap().join().unwrap();
}

fn upgrade_compactions() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("data")).unwrap();
    unpack(&tmp.path().join("data"));
    let exe = std::env::current_exe().unwrap();
    for (round, ms) in [600u64, 1700, 2900].into_iter().enumerate() {
        let mut child = std::process::Command::new(&exe)
            .args(["--exact", "upgrade_child", "--nocapture", "--test-threads=1"])
            .env("CRASH_UPGRADE_DIR", tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        let committed = |p: &Path| std::fs::read_to_string(p.join("committed")).unwrap_or_default();
        while committed(tmp.path()).lines().count() < round + 1 && start.elapsed() < Duration::from_secs(300) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(ms));
        child.kill().unwrap();
        child.wait().unwrap();

        let done: Vec<i64> = committed(tmp.path()).lines().map(|l| l.parse().unwrap()).collect();
        assert!(!done.is_empty(), "round {round}: nothing committed");
        let max = *done.iter().max().unwrap();
        let eng = open(&tmp.path().join("data"));
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        check_fixture(&mut s, &format!("round {round}"));
        let last = match one(&mut s, "SELECT -min(n) FROM f WHERE id > 8000") {
            x if x == format!("[[Int4({max})]]") => max,
            x if x == format!("[[Int4({})]]", max + 1) => max + 1,
            x => panic!("round {round}: the last batch {x}, logged {max}"),
        };
        for k in 1..=last {
            let want = if k == last { 6_000 } else { 3_000 };
            assert_eq!(one(&mut s, &format!("SELECT count(*) FROM f WHERE n = -{k} AND id > 8000")), format!("[[Int8({want})]]"), "round {round} batch {k}");
        }
        let rows = one(&mut s, "SELECT count(*), sum(id) FROM f");
        s.query("SET enable_seqscan = off").unwrap();
        s.query("SET enable_bitmapscan = off").unwrap();
        assert_eq!(one(&mut s, "SELECT count(*), sum(id) FROM f WHERE n >= -1000000"), rows, "round {round}");
        assert_eq!(one(&mut s, "SELECT count(*), sum(id) FROM f WHERE id >= 0"), rows, "round {round}");
        assert_eq!(one(&mut s, "SELECT count(*), sum(id) FROM f WHERE v >= ''"), rows, "round {round}");
        drop(s);
        eng.shutdown();
    }
    let (f1, f2) = formats(&tmp.path().join("data"));
    assert!(f2 > 0, "nothing written in format 2 ({f1} files of format 1)");
}
