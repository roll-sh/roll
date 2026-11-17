//! Row positions (ctid) across a restart and a crash. PostgreSQL never moves a committed row
//! when it restarts or recovers, and never gives a live row's ctid to another one. A child
//! process (this test binary run again) runs small update transactions, records every row's
//! ctid after the last COMMIT it makes, and then kills itself with SIGKILL partway through the
//! next transaction. After the database reopens, every committed row must still have the ctid
//! it had, the uncommitted transaction must be gone, and later updates must not reuse a live
//! row's ctid.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::path::Path;
use std::time::Duration;

const ROWS: i64 = 5_000;

fn open(dir: &Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 8 << 20,
        memtable_bytes: 1 << 20,
        sync: true,
        idle_close: Duration::from_secs(600),
        superuser: "postgres".into(),
        ..Default::default()
    })
    .unwrap()
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last()
        .unwrap()
        .rows
        .iter()
        .map(|r| r.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join(" "))
        .collect()
}

fn one(s: &mut Session, sql: &str) -> String {
    rows(s, sql).join("\n")
}

/// A tiny deterministic generator (xorshift).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn setup(s: &mut Session) {
    s.query("CREATE TABLE t (id int PRIMARY KEY, v text, n int)").unwrap();
    s.query("CREATE TABLE log (k int, at int)").unwrap();
    s.query(&format!(
        "INSERT INTO t SELECT g, repeat('x', 20 + g % 50), 0 FROM generate_series(1, {ROWS}) g"
    ))
    .unwrap();
}

/// Transaction `i` of the workload: a log row, and an update of one row of `t` that moves it
/// (a new key: not HOT) or rewrites it in place (HOT when the page has room).
fn step(s: &mut Session, rng: &mut Rng, i: u64, commit: bool) {
    let k = 1 + rng.below(ROWS as u64) as i64;
    let len = 10 + rng.below(80);
    s.query("BEGIN").unwrap();
    s.query(&format!("INSERT INTO log VALUES ({k}, {i})")).unwrap();
    let sql = if rng.below(3) == 0 {
        format!("UPDATE t SET id = id, v = repeat('y', {len}), n = n + 1 WHERE id = {k}")
    } else {
        format!("UPDATE t SET id = {k}, v = repeat('z', {len}), n = n + 1 WHERE id = {k}")
    };
    s.query(&sql).unwrap();
    if commit {
        s.query("COMMIT").unwrap();
    }
}

/// Every row's (id, ctid), in id order.
fn ctids(s: &mut Session) -> Vec<String> {
    rows(s, "SELECT id, ctid FROM t ORDER BY id")
}

/// The invariants: one row per key, every live row at its own ctid, and each ctid fetches
/// exactly its row.
fn check_unique(s: &mut Session, what: &str) {
    assert_eq!(
        one(s, "SELECT count(*), count(DISTINCT ctid) FROM t"),
        format!("Int8({ROWS}) Int8({ROWS})"),
        "{what}: duplicate ctids"
    );
    let bad = one(
        s,
        "SELECT count(*) FROM t a WHERE a.id % 20 = 0 AND (SELECT count(*) FROM t b WHERE b.ctid = a.ctid AND b.id = a.id) <> 1",
    );
    assert_eq!(bad, "Int8(0)", "{what}: a ctid that doesn't fetch its own row");
}

/// The child: `CRASH_CTIDS_DIR/data`, `steps` committed transactions, every row's ctid in
/// `before`, then one more transaction killed before its COMMIT.
#[test]
fn crash_ctids_child() {
    let Ok(dir) = std::env::var("CRASH_CTIDS_DIR") else {
        return;
    };
    let steps: u64 = std::env::var("CRASH_CTIDS_STEPS").unwrap().parse().unwrap();
    let dir = Path::new(&dir);
    let eng = open(&dir.join("data"));
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    setup(&mut s);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for i in 0..steps {
        step(&mut s, &mut rng, i, true);
    }
    check_unique(&mut s, "before the crash");
    std::fs::write(dir.join("before"), ctids(&mut s).join("\n")).unwrap();
    step(&mut s, &mut rng, steps, false);
    s.query("UPDATE t SET n = n + 1000 WHERE id <= 100").unwrap();
    let me = std::process::id().to_string();
    std::process::Command::new("kill").args(["-9", &me]).status().unwrap();
    unreachable!("still alive after SIGKILL");
}

/// Reopened after `kill -9`: committed rows where they were, the cut transaction gone, and
/// 600 more transactions never handing out a live row's ctid.
#[test]
fn ctids_survive_a_crash() {
    for steps in [300u64, 1_200] {
        let tmp = tempfile::tempdir().unwrap();
        let st = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_ctids_child", "--nocapture", "--test-threads=1"])
            .env("CRASH_CTIDS_DIR", tmp.path())
            .env("CRASH_CTIDS_STEPS", steps.to_string())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!st.success(), "steps {steps}: the child wasn't killed");
        let before = std::fs::read_to_string(tmp.path().join("before")).expect("the child's ctids");
        let eng = open(&tmp.path().join("data"));
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        let what = format!("{steps} steps, after the crash");
        assert_eq!(one(&mut s, "SELECT count(*), max(n) < 1000 FROM t"), format!("Int8({ROWS}) Bool(true)"), "{what}");
        assert_eq!(one(&mut s, "SELECT count(*) FROM log"), format!("Int8({steps})"), "{what}");
        assert!(ctids(&mut s).join("\n") == before, "{what}: committed rows moved");
        check_unique(&mut s, &what);
        let mut rng = Rng(0x2545_f491_4f6c_dd1d ^ steps);
        for i in 0..600 {
            step(&mut s, &mut rng, steps + i, true);
        }
        check_unique(&mut s, &format!("{steps} steps, 600 more after the crash"));
        drop(s);
        eng.shutdown();
    }
}

/// A transaction prepared before a restart keeps its rows' places: once COMMIT PREPARED makes
/// them visible, no other row holds their ctids, before or after more updates.
#[test]
fn prepared_ctids_survive_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let opts = |dir: &Path| EngineOptions {
        data_dir: dir.to_path_buf(),
        superuser: "postgres".into(),
        max_prepared_transactions: 2,
        ..Default::default()
    };
    let eng = Engine::open(opts(tmp.path())).unwrap();
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    setup(&mut s);
    s.query("BEGIN").unwrap();
    s.query("UPDATE t SET v = v || 'p' WHERE id % 10 = 0").unwrap();
    s.query("INSERT INTO t SELECT g, 'new', 0 FROM generate_series(5001, 5300) g").unwrap();
    s.query("DELETE FROM t WHERE id % 10 = 5").unwrap();
    s.query("PREPARE TRANSACTION 'p1'").unwrap();
    drop(s);
    eng.shutdown();
    drop(eng);
    let eng = Engine::open(opts(tmp.path())).unwrap();
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    // (The model is built while the transaction is still prepared; rows it locks are left
    // alone.)
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    for _ in 0..200 {
        let k = 1 + rng.below(ROWS as u64) as i64;
        if k % 5 != 0 {
            s.query(&format!("UPDATE t SET id = {k}, v = repeat('q', {}) WHERE id = {k}", 10 + k % 70)).unwrap();
        }
    }
    s.query("COMMIT PREPARED 'p1'").unwrap();
    // (The DELETE also took the new rows 5005, 5015, ...)
    let n = ROWS + 300 - 530;
    let check = |s: &mut Session, what: &str| {
        assert_eq!(one(s, "SELECT count(*), count(DISTINCT ctid) FROM t"), format!("Int8({n}) Int8({n})"), "{what}");
    };
    check(&mut s, "after COMMIT PREPARED");
    for i in 0..400 {
        step(&mut s, &mut rng, 200 + i, true);
    }
    check(&mut s, "400 transactions after COMMIT PREPARED");
    drop(s);
    eng.shutdown();
}

/// VACUUM FULL after updates that rolled back (a savepoint's, a whole transaction's, one that
/// moved its row to a new key): every row is copied, and gets a ctid of its own.
#[test]
fn vacuum_full_after_rolled_back_updates() {
    let tmp = tempfile::tempdir().unwrap();
    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    s.query("CREATE TABLE r (id int PRIMARY KEY, x int)").unwrap();
    s.query("INSERT INTO r SELECT g, 0 FROM generate_series(1, 40) g").unwrap();
    for sql in [
        "BEGIN; SAVEPOINT a; UPDATE r SET x = 1 WHERE id = 5; ROLLBACK TO SAVEPOINT a; COMMIT",
        "BEGIN; UPDATE r SET x = 2 WHERE id = 9; ROLLBACK",
        "BEGIN; UPDATE r SET id = 1000 WHERE id = 13; ROLLBACK",
        "VACUUM FULL r",
        "INSERT INTO r SELECT g, 3 FROM generate_series(41, 80) g",
    ] {
        s.query(sql).unwrap();
    }
    assert_eq!(one(&mut s, "SELECT count(*), count(DISTINCT ctid) FROM r"), "Int8(80) Int8(80)");
    assert_eq!(one(&mut s, "SELECT ctid FROM r WHERE id IN (5, 9, 13) ORDER BY id"), "Tid(0, 5)\nTid(0, 9)\nTid(0, 13)");
    drop(s);
    eng.shutdown();
}

/// The same across a clean restart (and a database reopened after it closed idle): a model
/// laid out afresh must keep every row where its ctid says it is.
#[test]
fn ctids_survive_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    setup(&mut s);
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for i in 0..800 {
        step(&mut s, &mut rng, i, true);
    }
    let before = ctids(&mut s);
    drop(s);
    eng.shutdown();
    drop(eng);
    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    assert!(ctids(&mut s) == before, "rows moved across a restart");
    for i in 0..600 {
        step(&mut s, &mut rng, 800 + i, true);
    }
    check_unique(&mut s, "600 transactions after a restart");
    drop(s);
    eng.shutdown();
}

/// A database a build before 9a6faba8 wrote can hold rows stored with another row's TID.
/// The first session after the database opens gives each its own, once; the ctids are then
/// unique, fetch their rows and stay across a restart.
#[test]
fn old_duplicate_ctids_repaired_once() {
    let tmp = tempfile::tempdir().unwrap();
    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    setup(&mut s);
    let mut rng = Rng(0x0dd_c71d);
    for i in 0..300 {
        step(&mut s, &mut rng, i, true);
    }
    drop(s);
    assert_eq!(eng.duplicate_tids_for_test("postgres", "t", 0).unwrap(), 0);
    eng.duplicate_tids_for_test("postgres", "t", 40).unwrap();
    assert_eq!(eng.duplicate_tids_for_test("postgres", "t", 0).unwrap(), 40);
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    assert_eq!(eng.duplicate_tids_for_test("postgres", "t", 0).unwrap(), 0);
    check_unique(&mut s, "after the repair");
    for i in 0..200 {
        step(&mut s, &mut rng, 300 + i, true);
    }
    check_unique(&mut s, "200 transactions after the repair");
    let after = ctids(&mut s);
    drop(s);
    eng.shutdown();
    drop(eng);
    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    assert!(ctids(&mut s) == after, "rows moved across a restart");
    drop(s);
    eng.shutdown();
}

// ── MultiXact state across a restart and a crash ──
//
// Recorded from PostgreSQL 18.6 (HANDOFF "MultiXact"): two KEY SHARE lockers of row 1 (a
// multi), a lone FOR SHARE locker of row 2, a FOR UPDATE locker of row 3 and an updater of
// row 4, all committed; then a FOR NO KEY UPDATE locker of row 4 still running. After a clean
// restart (that locker rolled back first) every xmax is as it was, the rolled-back locker's
// too; after a crash the running locker's is gone (its WAL record was never written). The
// multi's members resolve, and the next multi made is the one after it.

/// The xids of the transactions' locks are relative to the first locker's (`xa`).
fn mx_xid(s: &mut Session) -> i64 {
    one(s, "SELECT txid_current()").trim_start_matches("Int8(").trim_end_matches(')').parse().unwrap()
}

/// Runs the lockers; returns `xa` and the session still holding row 4's lock.
fn mx_scenario(eng: &Engine) -> (i64, Session) {
    let mut c = eng.connect(4, "postgres", "postgres", "t").unwrap();
    c.query("CREATE TABLE d (id int PRIMARY KEY, v int)").unwrap();
    c.query("INSERT INTO d SELECT g, g FROM generate_series(1, 4) g").unwrap();
    let mut a = eng.connect(2, "postgres", "postgres", "t").unwrap();
    let mut b = eng.connect(3, "postgres", "postgres", "t").unwrap();
    a.query("BEGIN").unwrap();
    let xa = mx_xid(&mut a);
    b.query("BEGIN").unwrap();
    mx_xid(&mut b);
    a.query("SELECT id FROM d WHERE id = 1 FOR KEY SHARE").unwrap();
    b.query("SELECT id FROM d WHERE id = 1 FOR KEY SHARE").unwrap();
    a.query("SELECT id FROM d WHERE id = 2 FOR SHARE").unwrap();
    a.query("COMMIT").unwrap();
    b.query("COMMIT").unwrap();
    c.query("BEGIN").unwrap();
    mx_xid(&mut c);
    c.query("SELECT id FROM d WHERE id = 3 FOR UPDATE").unwrap();
    c.query("UPDATE d SET v = 40 WHERE id = 4").unwrap();
    c.query("COMMIT").unwrap();
    a.query("BEGIN").unwrap();
    mx_xid(&mut a);
    a.query("SELECT id FROM d WHERE id = 4 FOR NO KEY UPDATE").unwrap();
    (xa, a)
}

/// Each row's xmax (relative to `xa`, or "multi"), the multi's members, its mxid_age, the
/// row locks pgrowlocks finds, and how far after it the next multi made is.
fn mx_state(eng: &Engine, s: &mut Session, xa: i64) -> String {
    let rel = |v: &str| -> String {
        let n: i64 = v.trim_start_matches("Oid(").trim_end_matches(')').parse().unwrap();
        if n == 0 {
            "0".into()
        } else if n >= xa - 5 {
            format!("xa+{}", n - xa)
        } else {
            "multi".into()
        }
    };
    let mut out: Vec<String> = rows(s, "SELECT id, v, xmax FROM d ORDER BY id")
        .iter()
        .map(|r| {
            let f: Vec<&str> = r.split(' ').collect();
            format!("{} {} {}", f[0], f[1], rel(f[2]))
        })
        .collect();
    out.push(one(s, &format!("SELECT xid::text::int8 - {xa}, mode FROM pg_get_multixact_members((SELECT xmax FROM d WHERE id = 1))")));
    out.push(one(s, "SELECT mxid_age(xmax), (SELECT count(*) FROM pgrowlocks('d')) FROM d WHERE id = 1"));
    let mut t = eng.connect(9, "postgres", "postgres", "t").unwrap();
    s.query("BEGIN; SELECT id FROM d WHERE id = 2 FOR KEY SHARE").unwrap();
    t.query("BEGIN; SELECT id FROM d WHERE id = 2 FOR KEY SHARE").unwrap();
    out.push(one(s, "SELECT (SELECT xmax::text::int8 FROM d WHERE id = 2) - (SELECT xmax::text::int8 FROM d WHERE id = 1)"));
    s.query("COMMIT").unwrap();
    t.query("COMMIT").unwrap();
    out.join(" | ")
}

const MX_KEPT: &str = "Int4(1) Int4(1) multi | Int4(2) Int4(2) xa+0 | Int4(3) Int4(3) xa+2 | Int4(4) Int4(40) xa+3 | Int8(0) Text(\"keysh\")\nInt8(1) Text(\"keysh\") | Int4(1) Int8(0) | Int8(1)";

#[test]
fn multixact_survives_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    s.query("CREATE EXTENSION pgrowlocks").unwrap();
    let (xa, mut a) = mx_scenario(&eng);
    a.query("ROLLBACK").unwrap();
    drop((s, a));
    eng.shutdown();
    drop(eng);
    let eng = open(tmp.path());
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    assert_eq!(mx_state(&eng, &mut s, xa), MX_KEPT);
    drop(s);
    eng.shutdown();
}

/// The child: the lockers, `xa` in `xa`, then SIGKILL while row 4's locker runs.
#[test]
fn multixact_crash_child() {
    let Ok(dir) = std::env::var("CRASH_MX_DIR") else {
        return;
    };
    let dir = Path::new(&dir);
    let eng = open(&dir.join("data"));
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    s.query("CREATE EXTENSION pgrowlocks").unwrap();
    let (xa, _a) = mx_scenario(&eng);
    std::fs::write(dir.join("xa"), xa.to_string()).unwrap();
    let me = std::process::id().to_string();
    std::process::Command::new("kill").args(["-9", &me]).status().unwrap();
    unreachable!("still alive after SIGKILL");
}

#[test]
fn multixact_survives_a_crash() {
    let tmp = tempfile::tempdir().unwrap();
    let st = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "multixact_crash_child", "--nocapture", "--test-threads=1"])
        .env("CRASH_MX_DIR", tmp.path())
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!st.success(), "the child wasn't killed");
    let xa: i64 = std::fs::read_to_string(tmp.path().join("xa")).unwrap().parse().unwrap();
    let eng = open(&tmp.path().join("data"));
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    assert_eq!(mx_state(&eng, &mut s, xa), MX_KEPT.replace("Int4(40) xa+3", "Int4(40) 0"));
    drop(s);
    eng.shutdown();
}
