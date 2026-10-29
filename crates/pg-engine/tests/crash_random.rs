//! A seeded crash harness. A child process (this test binary run again) runs random steps
//! against one database: transactions of inserts, updates (in place, of TOASTed values, of
//! the key), deletes, foreign-key cascades, rolled-back savepoints, DDL (indexes, columns,
//! tables, TRUNCATE), two-phase commits, VACUUM, VACUUM FULL. A thread in the child kills it
//! with SIGKILL at a seeded moment. The database is reopened and checked against a model of
//! the committed steps:
//! - the committed data is there and the uncommitted data isn't, read through every index;
//! - every live row has a ctid of its own, and that ctid fetches it;
//! - constraints hold;
//! - the sequence never goes back past a value a committed transaction drew;
//! - no prepared transaction is lost.
//!
//! Then more steps run in process and are checked again, and the next round crashes again.
//! Every step's outcome goes to `steps` (fsynced), so a round can continue the model.
//! `CRASH_SEEDS=a,b,..` and `CRASH_ROUNDS=n` choose the runs (the defaults are small). To find
//! the step that breaks an invariant, `CRASH_CHECK_STEPS=1` checks ctids after every step and
//! `CRASH_TRACE=<file>` appends every step's statements to the file.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use pg_engine::types::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

fn open(dir: &Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 8 << 20,
        memtable_bytes: 256 << 10,
        sync: true,
        idle_close: Duration::from_secs(600),
        superuser: "postgres".into(),
        max_prepared_transactions: 4,
        ..Default::default()
    })
    .unwrap()
}

fn texts(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let show = |v: &Value| match v {
        Value::Text(t) => t.to_string(),
        v => format!("{v:?}"),
    };
    r.last()
        .unwrap()
        .rows
        .iter()
        .map(|r| r.iter().map(show).collect::<Vec<_>>().join("|"))
        .collect()
}

fn one(s: &mut Session, sql: &str) -> String {
    texts(s, sql).join("\n")
}

struct Rng(u64);
impl Rng {
    fn new(seed: u64, i: u64) -> Rng {
        let mut r = Rng(seed ^ i.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x2545_f491_4f6c_dd1d);
        for _ in 0..4 {
            r.next();
        }
        r
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<T>(&mut self, m: &BTreeMap<i32, T>) -> Option<i32> {
        let n = m.len() as u64;
        (n > 0).then(|| *m.keys().nth(self.below(n) as usize).unwrap())
    }
}

/// What the committed steps left.
#[derive(Clone, Default)]
struct Model {
    /// a: id -> (v, w, u)
    a: BTreeMap<i32, (i32, String, i32)>,
    /// b: id -> (a_id, x)
    b: BTreeMap<i32, (i32, i32)>,
    /// Columns added to a, with their defaults.
    cols: Vec<(String, i32)>,
    /// Whether a(w) has an index.
    widx: bool,
    /// Tables e<i>: their row counts.
    tables: BTreeMap<String, i32>,
}

#[derive(Clone, Debug)]
enum Op {
    InsA(i32, i32, String),
    BulkA(i32, i32),
    UpdV(i32, i32),
    UpdW(i32, String),
    Move(i32, i32),
    DelA(i32),
    InsB(i32, i32, i32),
    UpdB(i32, i32),
    DelB(i32, i32),
    AddCol(String, i32),
    Index(bool),
    MkTable(String, i32),
    DropTable(String),
    TruncB,
    /// Ops in a savepoint rolled back.
    Undone(Vec<Op>),
}

/// A long value (TOASTed) or a short one, as SQL and as text.
fn wval(r: &mut Rng, tag: i32) -> String {
    if r.below(8) == 0 {
        format!("{}{tag}", "w".repeat(2000 + r.below(3000) as usize))
    } else {
        format!("w{tag}-{}", r.below(1000))
    }
}

fn wsql(w: &str) -> String {
    let n = w.chars().take_while(|&c| c == 'w').count();
    if n > 100 { format!("repeat('w', {n}) || '{}'", &w[n..]) } else { format!("'{w}'") }
}

impl Model {
    fn apply(&mut self, op: &Op) {
        match op {
            Op::InsA(id, v, w) => {
                self.a.insert(*id, (*v, w.clone(), id * 7));
            }
            Op::BulkA(from, n) => {
                for id in *from..from + n {
                    self.a.insert(id, (id % 97, format!("b{id}"), id * 7));
                }
            }
            Op::UpdV(id, v) => self.a.get_mut(id).unwrap().0 = *v,
            Op::UpdW(id, w) => self.a.get_mut(id).unwrap().1 = w.clone(),
            Op::Move(id, to) => {
                let r = self.a.remove(id).unwrap();
                self.a.insert(*to, r);
                for b in self.b.values_mut().filter(|b| b.0 == *id) {
                    b.0 = *to;
                }
            }
            Op::DelA(id) => {
                self.a.remove(id);
                self.b.retain(|_, b| b.0 != *id);
            }
            Op::InsB(id, a, x) => {
                self.b.insert(*id, (*a, *x));
            }
            Op::UpdB(id, x) => self.b.get_mut(id).unwrap().1 = *x,
            Op::DelB(lo, hi) => self.b.retain(|k, _| k < lo || k >= hi),
            Op::AddCol(c, d) => self.cols.push((c.clone(), *d)),
            Op::Index(on) => self.widx = *on,
            Op::MkTable(t, n) => {
                self.tables.insert(t.clone(), *n);
            }
            Op::DropTable(t) => {
                self.tables.remove(t);
            }
            Op::TruncB => self.b.clear(),
            Op::Undone(_) => {}
        }
    }

    /// A random op valid in this state; `id` is a key no other op uses.
    fn gen_op(&self, r: &mut Rng, id: i32, ddl: bool) -> Op {
        loop {
            let op = match r.below(if ddl { 16 } else { 11 }) {
                0 | 1 => Op::InsA(id, r.below(500) as i32, wval(r, id)),
                2 if r.below(4) == 0 => Op::BulkA(id, 1 + r.below(150) as i32),
                3 => match r.pick(&self.a) {
                    Some(k) => Op::UpdV(k, r.below(500) as i32),
                    None => continue,
                },
                4 => match r.pick(&self.a) {
                    Some(k) => Op::UpdW(k, wval(r, k)),
                    None => continue,
                },
                5 => match r.pick(&self.a) {
                    Some(k) => Op::Move(k, id),
                    None => continue,
                },
                6 => match r.pick(&self.a) {
                    Some(k) => Op::DelA(k),
                    None => continue,
                },
                7 | 8 => match r.pick(&self.a) {
                    Some(k) => Op::InsB(id, k, r.below(100) as i32),
                    None => continue,
                },
                9 => match r.pick(&self.b) {
                    Some(k) => Op::UpdB(k, r.below(100) as i32),
                    None => continue,
                },
                10 => match r.pick(&self.b) {
                    Some(k) => Op::DelB(k, k + r.below(20_000) as i32),
                    None => continue,
                },
                11 if self.cols.len() < 6 => Op::AddCol(format!("c{id}"), r.below(9) as i32),
                12 => Op::Index(!self.widx),
                13 => Op::MkTable(format!("e{id}"), r.below(300) as i32),
                14 => match self.tables.keys().nth(r.below(self.tables.len() as u64) as usize) {
                    Some(t) => Op::DropTable(t.clone()),
                    None => continue,
                },
                15 if r.below(3) == 0 => Op::TruncB,
                _ => continue,
            };
            return op;
        }
    }
}

fn op_sql(op: &Op, out: &mut Vec<String>) {
    let sql = match op {
        Op::InsA(id, v, w) => format!("INSERT INTO a (id, v, w, u) VALUES ({id}, {v}, {}, {})", wsql(w), id * 7),
        Op::BulkA(f, n) => format!(
            "INSERT INTO a (id, v, w, u) SELECT g, g % 97, 'b' || g, g * 7 FROM generate_series({f}, {}) g",
            f + n - 1
        ),
        Op::UpdV(id, v) => format!("UPDATE a SET v = {v} WHERE id = {id}"),
        Op::UpdW(id, w) => format!("UPDATE a SET w = {} WHERE id = {id}", wsql(w)),
        Op::Move(id, to) => format!("UPDATE a SET id = {to} WHERE id = {id}"),
        Op::DelA(id) => format!("DELETE FROM a WHERE id = {id}"),
        Op::InsB(id, a, x) => format!("INSERT INTO b VALUES ({id}, {a}, {x})"),
        Op::UpdB(id, x) => format!("UPDATE b SET x = {x} WHERE id = {id}"),
        Op::DelB(lo, hi) => format!("DELETE FROM b WHERE id >= {lo} AND id < {hi}"),
        Op::AddCol(c, d) => format!("ALTER TABLE a ADD COLUMN {c} int DEFAULT {d}"),
        Op::Index(true) => "CREATE INDEX a_w ON a (w)".into(),
        Op::Index(false) => "DROP INDEX a_w".into(),
        Op::MkTable(t, n) => format!(
            "CREATE TABLE {t} AS SELECT g AS k, repeat('e', g % 40) AS z FROM generate_series(1, {n}) g"
        ),
        Op::DropTable(t) => format!("DROP TABLE {t}"),
        Op::TruncB => "TRUNCATE b".into(),
        Op::Undone(ops) => {
            out.push("SAVEPOINT s1".into());
            for o in ops {
                op_sql(o, out);
            }
            "ROLLBACK TO SAVEPOINT s1".into()
        }
    };
    out.push(sql);
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum End {
    Commit,
    Rollback,
    /// PREPARE TRANSACTION, then COMMIT PREPARED (true) or ROLLBACK PREPARED.
    Prepare(bool),
    /// No transaction: VACUUM and such.
    Maint,
}

/// Step `i`: its statements, how it ends, and its ops.
fn step(seed: u64, i: u64, m: &Model) -> (Vec<String>, End, Vec<Op>) {
    let mut r = Rng::new(seed, i);
    let base = i as i32 * 10_000;
    let k = r.below(100);
    if k < 6 {
        let sql = ["VACUUM a", "VACUUM b", "VACUUM FULL a", "VACUUM FULL b", "VACUUM (FREEZE) a", "CHECKPOINT"][k as usize];
        return (vec![sql.into(), format!("INSERT INTO txlog (i) VALUES ({i})")], End::Maint, vec![]);
    }
    let end = match k {
        6..=10 => End::Rollback,
        11..=14 => End::Prepare(k != 14),
        _ => End::Commit,
    };
    let ddl = r.below(6) == 0;
    let mut cur = m.clone();
    let mut ops = Vec::new();
    for j in 0..1 + r.below(6) as i32 {
        let op = if r.below(10) == 0 {
            let mut sp = cur.clone();
            let mut undone = Vec::new();
            for jj in 0..1 + r.below(3) as i32 {
                let o = sp.gen_op(&mut r, base + 2000 + j * 600 + jj * 160, false);
                sp.apply(&o);
                undone.push(o);
            }
            Op::Undone(undone)
        } else {
            cur.gen_op(&mut r, base + j * 160, ddl)
        };
        cur.apply(&op);
        ops.push(op);
    }
    let mut sql = vec!["BEGIN".to_string()];
    for o in &ops {
        op_sql(o, &mut sql);
    }
    sql.push(format!("INSERT INTO txlog (i) VALUES ({i})"));
    sql.push(match end {
        End::Commit => "COMMIT".into(),
        End::Rollback => "ROLLBACK".into(),
        End::Prepare(_) => format!("PREPARE TRANSACTION 'p{i}'"),
        End::Maint => unreachable!(),
    });
    if let End::Prepare(c) = end {
        sql.push(format!("{} PREPARED 'p{i}'", if c { "COMMIT" } else { "ROLLBACK" }));
    }
    (sql, end, ops)
}

/// The steps log: one line per finished step, `<i> c` (its effects are in) or `<i> a`.
fn read_log(dir: &Path) -> Vec<bool> {
    std::fs::read_to_string(dir.join("steps"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.ends_with(" c"))
        .collect()
}

fn append_log(dir: &Path, i: usize, committed: bool) {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("steps")).unwrap();
    writeln!(f, "{i} {}", if committed { "c" } else { "a" }).unwrap();
    f.sync_all().unwrap();
}

/// The model after the logged steps.
fn replay(seed: u64, log: &[bool]) -> Model {
    let mut m = Model::default();
    for (i, &c) in log.iter().enumerate() {
        let (_, end, ops) = step(seed, i as u64, &m);
        if c && end != End::Maint {
            for o in &ops {
                m.apply(o);
            }
        }
    }
    m
}

const SCHEMA: &[&str] = &[
    "CREATE SEQUENCE s",
    "CREATE TABLE a (id int PRIMARY KEY, v int NOT NULL CHECK (v >= 0), w text, u int UNIQUE)",
    "CREATE INDEX a_v ON a (v)",
    "CREATE TABLE b (id int PRIMARY KEY, a_id int REFERENCES a ON UPDATE CASCADE ON DELETE CASCADE, x int)",
    "CREATE INDEX b_a ON b (a_id)",
    "CREATE TABLE txlog (i int PRIMARY KEY, sv bigint DEFAULT nextval('s'))",
];

/// Run steps `from..to`, logging each one's outcome; with `kill`, a thread kills the process
/// `kill.1` microseconds after step `kill.0` starts.
fn run_steps(s: &mut Session, dir: &Path, seed: u64, m: &mut Model, from: usize, to: usize, kill: Option<(usize, u64)>) {
    for i in from..to {
        if let Some((at, us)) = kill
            && at == i
        {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_micros(us));
                let me = std::process::id().to_string();
                std::process::Command::new("kill").args(["-9", &me]).status().unwrap();
            });
        }
        let (sql, end, ops) = step(seed, i as u64, m);
        if let Ok(f) = std::env::var("CRASH_TRACE") {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(f).unwrap();
            writeln!(f, "{i}: {}", sql.join("; ")).unwrap();
        }
        for q in &sql {
            s.query(q).unwrap_or_else(|e| panic!("seed {seed} step {i}: {q}: {e:?}"));
        }
        let committed = matches!(end, End::Commit | End::Prepare(true) | End::Maint);
        if committed && end != End::Maint {
            for o in &ops {
                m.apply(o);
            }
        }
        append_log(dir, i, committed);
        if std::env::var("CRASH_CHECK_STEPS").is_ok() {
            for t in ["a", "b"] {
                let got = one(s, &format!("SELECT count(*) = count(DISTINCT ctid) FROM {t}"));
                if got != "Bool(true)" {
                    let rows = texts(s, &format!("SELECT ctid, id, xmin FROM {t} ORDER BY ctid")).join(" ");
                    panic!("seed {seed} step {i} (from {from}): {t}'s ctids after {}: {rows}", sql.join("; "));
                }
            }
        }
    }
}

/// After a crash: settle the step that was running (a prepared transaction it left is
/// finished as the step would have), log it, and return the model of every logged step.
fn settle(s: &mut Session, dir: &Path, seed: u64) -> Model {
    let mut log = read_log(dir);
    let m = replay(seed, &log);
    let i = log.len();
    let (_, end, ops) = step(seed, i as u64, &m);
    // (DDL holds locks that reads of its tables wait for.)
    let ddl = ops.iter().any(|o| matches!(o, Op::AddCol(..) | Op::Index(_) | Op::MkTable(..) | Op::DropTable(_) | Op::TruncB));
    let prepared = texts(s, "SELECT gid FROM pg_prepared_xacts");
    let in_log = one(s, &format!("SELECT count(*) FROM txlog WHERE i = {i}")) == "Int8(1)";
    let committed = if prepared.contains(&format!("p{i}")) {
        // (Prepared, not committed yet: none of it shows.)
        assert!(!in_log, "seed {seed}: step {i} is prepared, but its rows show");
        if !ddl {
            check(s, seed, &m, "a prepared transaction's rows don't show");
        }
        let End::Prepare(c) = end else { panic!("seed {seed}: step {i} prepared as {end:?}") };
        s.query(&format!("{} PREPARED 'p{i}'", if c { "COMMIT" } else { "ROLLBACK" })).unwrap();
        c
    } else {
        assert!(prepared.is_empty(), "seed {seed}: prepared {prepared:?} at step {i}");
        assert!(!in_log || end != End::Rollback, "seed {seed}: step {i} rolled back, but its rows show");
        in_log
    };
    append_log(dir, i, committed);
    log.push(committed);
    replay(seed, &log)
}

/// The invariants, with `what` in the failure messages.
fn check(s: &mut Session, seed: u64, m: &Model, what: &str) {
    let what = format!("seed {seed}: {what}");
    let want_a: Vec<String> = m.a.iter().map(|(k, (v, w, u))| format!("Int4({k})|Int4({v})|{w}|Int4({u})")).collect();
    let want_b: Vec<String> = m.b.iter().map(|(k, (a, x))| format!("Int4({k})|Int4({a})|Int4({x})")).collect();
    let (na, nb) = (m.a.len(), m.b.len());
    let sum_a = m.a.values().map(|r| r.0 as i64).sum::<i64>();
    for (how, set) in [
        ("seqscan", "SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_indexonlyscan = off"),
        ("indexscan", "SET enable_seqscan = off; SET enable_bitmapscan = off; SET enable_indexscan = on; SET enable_indexonlyscan = on"),
        ("bitmapscan", "SET enable_seqscan = off; SET enable_indexscan = off; SET enable_indexonlyscan = off; SET enable_bitmapscan = on"),
    ] {
        s.query(set).unwrap();
        let got = texts(s, "SELECT id, v, w, u FROM a WHERE id > -1 ORDER BY id");
        assert!(got == want_a, "{what}: a by {how}: {} rows, {na} expected", got.len());
        let got = texts(s, "SELECT id, a_id, x FROM b WHERE id > -1 ORDER BY id");
        assert!(got == want_b, "{what}: b by {how}: {} rows, {nb} expected", got.len());
        let q = |s: &mut Session, sql: &str| one(s, sql);
        assert_eq!(q(s, "SELECT count(*), coalesce(sum(v), 0) FROM a WHERE v >= 0"), format!("Int8({na})|Int8({sum_a})"), "{what}: a_v by {how}");
        assert_eq!(q(s, "SELECT count(*) FROM a WHERE u > -1"), format!("Int8({na})"), "{what}: a_u_key by {how}");
        assert_eq!(q(s, "SELECT count(*) FROM b WHERE a_id > -1"), format!("Int8({nb})"), "{what}: b_a by {how}");
        if m.widx {
            assert_eq!(q(s, "SELECT count(*) FROM a WHERE w >= ''"), format!("Int8({na})"), "{what}: a_w by {how}");
        }
    }
    s.query("RESET ALL").unwrap();
    for (t, n) in [("a", na), ("b", nb)] {
        let got = one(s, &format!("SELECT count(*), count(DISTINCT ctid) FROM {t}"));
        assert_eq!(got, format!("Int8({n})|Int8({n})"), "{what}: {t}'s ctids");
        let bad = one(s, &format!("SELECT count(*) FROM {t} x WHERE (SELECT count(*) FROM {t} y WHERE y.ctid = x.ctid AND y.id = x.id) <> 1 AND x.id % 3 = 0"));
        assert_eq!(bad, "Int8(0)", "{what}: {t}: a ctid that doesn't fetch its own row");
    }
    assert_eq!(one(s, "SELECT count(*) FROM b WHERE a_id NOT IN (SELECT id FROM a)"), "Int8(0)", "{what}: foreign key");
    for (c, d) in &m.cols {
        assert_eq!(one(s, &format!("SELECT count(*) FROM a WHERE {c} IS DISTINCT FROM {d}")), "Int8(0)", "{what}: column {c}");
    }
    let tables = texts(s, "SELECT relname FROM pg_class WHERE relname ~ '^e[0-9]+$' AND relkind = 'r' ORDER BY relname");
    let want: Vec<String> = m.tables.keys().cloned().collect();
    assert_eq!(tables, want, "{what}: tables");
    for (t, n) in &m.tables {
        let got = one(s, &format!("SELECT count(*), count(DISTINCT ctid) FROM {t}"));
        assert_eq!(got, format!("Int8({n})|Int8({n})"), "{what}: {t}");
    }
    let seq = one(s, "SELECT nextval('s') > coalesce(max(sv), 0) FROM txlog");
    assert_eq!(seq, "Bool(true)", "{what}: the sequence went back past a committed value");
}

/// The child: on from the logged steps, killed `CRASH_KILL_US` microseconds into step
/// `CRASH_KILL_AT`.
#[test]
fn crash_random_child() {
    let Ok(dir) = std::env::var("CRASH_RANDOM_DIR") else {
        return;
    };
    let env = |k: &str| std::env::var(k).unwrap().parse::<u64>().unwrap();
    let (seed, at, us) = (env("CRASH_SEED"), env("CRASH_KILL_AT") as usize, env("CRASH_KILL_US"));
    let dir = Path::new(&dir);
    let eng = open(&dir.join("data"));
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    let log = read_log(dir);
    if log.is_empty() {
        for q in SCHEMA {
            s.query(q).unwrap();
        }
    }
    let mut m = replay(seed, &log);
    run_steps(&mut s, dir, seed, &mut m, log.len(), at + 100_000, Some((at, us)));
    unreachable!("seed {seed}: never killed");
}

/// One round: the child runs until killed, then the database is reopened, settled and
/// checked, runs 100 steps and is checked again.
fn crash_round(dir: &Path, seed: u64, round: u64) {
    let mut r = Rng::new(seed, 1_000_000 + round);
    let at = read_log(dir).len() + 20 + r.below(150) as usize;
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_random_child", "--nocapture", "--test-threads=1"])
        .env("CRASH_RANDOM_DIR", dir)
        .env("CRASH_SEED", seed.to_string())
        .env("CRASH_KILL_AT", at.to_string())
        .env("CRASH_KILL_US", r.below(20_000).to_string())
        .output()
        .unwrap();
    let why = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("panicked") || l.contains("seed"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(out.status.code().is_none(), "seed {seed} round {round}: the child failed: {why}");
    let eng = open(&dir.join("data"));
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    let mut m = settle(&mut s, dir, seed);
    let n = read_log(dir).len();
    check(&mut s, seed, &m, &format!("round {round}, recovered after step {n}"));
    run_steps(&mut s, dir, seed, &mut m, n, n + 100, None);
    check(&mut s, seed, &m, &format!("round {round}, 100 steps after recovering"));
    drop(s);
    eng.shutdown();
}

#[test]
fn random_crashes() {
    let seeds: Vec<u64> = std::env::var("CRASH_SEEDS")
        .map(|v| v.split(',').map(|x| x.trim().parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![1]);
    let rounds: u64 = std::env::var("CRASH_ROUNDS").map_or(3, |v| v.parse().unwrap());
    for seed in seeds {
        let tmp = tempfile::tempdir().unwrap();
        for round in 0..rounds {
            crash_round(tmp.path(), seed, round);
        }
    }
}
