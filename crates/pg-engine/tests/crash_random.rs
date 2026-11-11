//! A seeded crash harness. A child process (this test binary run again) runs random steps
//! against one database: transactions of inserts, updates (in place, of TOASTed values, of
//! the key), deletes, foreign-key cascades, rolled-back savepoints, DDL (indexes, columns,
//! tables, TRUNCATE), two-phase commits, VACUUM, VACUUM FULL. A thread in the child kills it
//! with SIGKILL at a seeded moment: some microseconds into a seeded step, or (a third of the
//! rounds) just after a flush, compaction or checkpoint starts writing a file. The database
//! is reopened and checked against a model of the committed steps:
//! - durability: txlog holds exactly the steps logged as committed (and maybe the one in
//!   flight, whose COMMIT may have been durable without its client hearing back);
//! - atomicity: the step in flight left all of its writes or none, as its txlog row says;
//! - the committed data is there and the uncommitted data isn't, read through every index;
//! - every live row has a ctid of its own, and that ctid fetches it;
//! - constraints hold;
//! - the sequence never goes back past a value a committed transaction drew;
//! - no prepared transaction is lost.
//!
//! Then more steps run in process and are checked again, and the next round crashes again.
//! Every step's outcome goes to `steps` (fsynced, one write a line), so a round can continue
//! the model. `CRASH_SEEDS=a,b,..` and `CRASH_ROUNDS=n` choose the runs (the defaults are
//! small); `CRASH_ASYNC=1` runs the child with synchronous_commit off (a crash may then lose a
//! suffix of its commits, never part of one). To find the step that breaks an invariant,
//! `CRASH_CHECK_STEPS=1` checks ctids after every step and `CRASH_TRACE=<file>` appends every
//! step's statements to the file; `CRASH_SAVE=<dir>` keeps each crashed state, which
//! `CRASH_INSPECT=<dir>/s<seed>-r<round>` (with `CRASH_SEED`) recovers and checks again.
//! `CRASH_POWER=1` (with `--features powerloss`) makes every crash a power cut: what no fsync
//! covered is lost too.

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
    /// Rows inserted into a table e<i>: after a rolled-back DROP they must not take the row
    /// ids of the rows already there.
    InsE(String, i32),
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
            Op::InsE(t, n) => *self.tables.get_mut(t).unwrap() += n,
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
            let op = match r.below(if ddl { 17 } else { 11 }) {
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
                16 => match self.tables.keys().nth(r.below(self.tables.len() as u64) as usize) {
                    Some(t) => Op::InsE(t.clone(), 1 + r.below(50) as i32),
                    None => continue,
                },
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
        Op::InsE(t, n) => format!("INSERT INTO {t} SELECT g, repeat('e', g % 40) FROM generate_series(1, {n}) g"),
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

/// The steps log: one line per finished step, `<i> c` (its effects are in) or `<i> a`. A
/// crash can cut the last line short (the step's commit returned, its line wasn't all
/// written): such a step counts as in flight, as a commit whose client never heard back,
/// and `settle` decides it from the database. Each line must be the step it numbers.
fn read_log(dir: &Path) -> Vec<bool> {
    let text = std::fs::read_to_string(dir.join("steps")).unwrap_or_default();
    let whole = &text[..text.rfind('\n').map_or(0, |n| n + 1)];
    whole
        .lines()
        .enumerate()
        .map(|(i, l)| match l.strip_prefix(&format!("{i} ")) {
            Some("c") => true,
            Some("a") => false,
            _ => panic!("the steps log's line {i} is {l:?}"),
        })
        .collect()
}

/// Log step `i`'s outcome: one write of the whole line, after any cut short line is gone.
fn append_log(dir: &Path, i: usize, committed: bool) {
    let path = dir.join("steps");
    let len = std::fs::read_to_string(&path).map_or(0, |t| t.rfind('\n').map_or(0, |n| n + 1)) as u64;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
    if f.metadata().unwrap().len() != len {
        f.set_len(len).unwrap();
    }
    f.write_all(format!("{i} {}\n", if committed { "c" } else { "a" }).as_bytes()).unwrap();
    f.sync_all().unwrap();
}

fn write_log(dir: &Path, log: &[bool]) {
    let text: String = log.iter().enumerate().map(|(i, c)| format!("{i} {}\n", if *c { "c" } else { "a" })).collect();
    let f = dir.join("steps");
    std::fs::write(&f, text).unwrap();
    std::fs::File::open(&f).unwrap().sync_all().unwrap();
}

#[test]
fn torn_acknowledgement_is_still_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    for tail in ["1", "1 ", "1 c", "1 a"] {
        std::fs::write(dir.path().join("steps"), format!("0 c\n{tail}")).unwrap();
        assert_eq!(read_log(dir.path()), vec![true]);
        append_log(dir.path(), 1, true);
        assert_eq!(read_log(dir.path()), vec![true, true]);
        assert_eq!(std::fs::read_to_string(dir.path().join("steps")).unwrap(), "0 c\n1 c\n");
    }
}

/// `CRASH_ASYNC=1`: the child commits with synchronous_commit off.
fn async_commit() -> bool {
    std::env::var("CRASH_ASYNC").is_ok_and(|v| v == "1")
}

/// `CRASH_POWER=1`: each crash is a power cut, not only a process kill. The child records
/// its file changes (the `powerloss` feature, `storage::powerloss`); then every write and
/// directory entry no fsync covered is dropped (half the rounds: a seeded part of them is
/// kept, as a disk may have written them anyway).
fn power_loss() -> bool {
    std::env::var("CRASH_POWER").is_ok_and(|v| v == "1")
}

#[cfg(feature = "powerloss")]
fn cut_power(dir: &Path, shadow: &Path, keep: Option<u64>, seed: u64, round: u64) {
    let cut = pg_engine::storage::powerloss::power_cut(&dir.join("data"), shadow, keep).unwrap();
    eprintln!("seed {seed} round {round}: the power cut took {} writes and {} directory entries (kept some: {})", cut.writes, cut.entries, keep.is_some());
    // (Files changed outside `storage::durable` and taken as fsynced: none of the databases'.)
    let adopted = std::fs::read_to_string(shadow.join("adopted")).unwrap_or_default();
    let ours: Vec<&str> = adopted.lines().filter(|l| l.contains("/base/") || l.ends_with("databases.json")).collect();
    assert!(ours.is_empty(), "seed {seed} round {round}: written outside storage::durable: {ours:?}");
}

#[cfg(not(feature = "powerloss"))]
fn cut_power(_: &Path, _: &Path, _: Option<u64>, _: u64, _: u64) {
    panic!("CRASH_POWER=1 needs the powerloss feature (cargo test --features powerloss)");
}

/// The steps txlog holds (by sequential scan).
fn logged_steps(s: &mut Session) -> std::collections::BTreeSet<usize> {
    s.query("SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_indexonlyscan = off").unwrap();
    let r = texts(s, "SELECT i FROM txlog");
    s.query("RESET ALL").unwrap();
    r.iter().map(|v| v.trim_start_matches("Int4(").trim_end_matches(')').parse().unwrap()).collect()
}

/// Whether a and b hold exactly the model's rows (by sequential scan).
fn rows_are(s: &mut Session, m: &Model) -> bool {
    s.query("SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_indexonlyscan = off").unwrap();
    let a = texts(s, "SELECT id, v, w, u FROM a ORDER BY id");
    let b = texts(s, "SELECT id, a_id, x FROM b ORDER BY id");
    s.query("RESET ALL").unwrap();
    a == m.a.iter().map(|(k, (v, w, u))| format!("Int4({k})|Int4({v})|{w}|Int4({u})")).collect::<Vec<_>>()
        && b == m.b.iter().map(|(k, (a, x))| format!("Int4({k})|Int4({a})|Int4({x})")).collect::<Vec<_>>()
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

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// SIGKILL this process (as a crash: nothing flushed, nothing run).
fn sigkill() -> ! {
    // SAFETY: kill(2) with this process's id and SIGKILL.
    unsafe { kill(std::process::id() as i32, 9) };
    unreachable!("killed")
}

/// Return once the `n`th file a flush, compaction or checkpoint starts writing in the
/// databases' directories under `base` (an SST, a WAL or a manifest) has appeared.
fn wait_for_files(base: &Path, n: usize) {
    let scan = |seen: &mut std::collections::HashSet<std::path::PathBuf>| {
        let mut new = 0;
        for d in std::fs::read_dir(base).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let p = f.path();
                if matches!(p.extension().and_then(|e| e.to_str()), Some("sst" | "wal" | "tmp")) && seen.insert(p) {
                    new += 1;
                }
            }
        }
        new
    };
    let mut seen = std::collections::HashSet::new();
    scan(&mut seen);
    let mut events = 0;
    while events <= n {
        std::thread::sleep(Duration::from_micros(20));
        events += scan(&mut seen);
    }
}

/// Run steps `from..to`, logging each one's outcome; with `kill`, a thread kills the process
/// `kill.1` microseconds after step `kill.0` starts.
fn run_steps(s: &mut Session, dir: &Path, seed: u64, m: &mut Model, from: usize, to: usize, kill: Option<(usize, u64)>) {
    for i in from..to {
        if let Some((at, us)) = kill
            && at == i
        {
            let data = dir.join("data").join("base");
            let on = std::env::var("CRASH_KILL_ON").ok().map(|v| v.parse::<usize>().unwrap());
            std::thread::spawn(move || {
                if let Some(n) = on {
                    wait_for_files(&data, n);
                }
                std::thread::sleep(Duration::from_micros(us));
                sigkill();
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
    let txlog = logged_steps(s);
    if async_commit() {
        // (Asynchronous commits a crash lost: a suffix of the commits, every one after the
        // first lost one. The steps from there are forgotten and run again.)
        if let Some(f) = (0..log.len()).find(|&i| log[i] && !txlog.contains(&i)) {
            let after: Vec<usize> = txlog.iter().copied().filter(|&i| i > f).collect();
            assert!(after.is_empty(), "seed {seed}: step {f}'s commit was lost, but later ones weren't: {after:?}");
            log.truncate(f);
            write_log(dir, &log);
        }
    }
    // Durability: exactly the steps logged as committed are in txlog (and the one in flight
    // may be).
    let want: Vec<usize> = (0..log.len()).filter(|&i| log[i]).collect();
    let got: Vec<usize> = txlog.iter().copied().filter(|&i| i < log.len()).collect();
    assert_eq!(got, want.clone(), "seed {seed}: txlog against the steps logged as committed");
    let m = replay(seed, &log);
    let i = log.len();
    let (_, end, ops) = step(seed, i as u64, &m);
    let late: Vec<usize> = txlog.iter().copied().filter(|&j| j > i).collect();
    assert!(late.is_empty(), "seed {seed}: txlog has steps {late:?} past the one in flight ({i})");
    // (DDL holds locks that reads of its tables wait for.)
    let ddl = ops.iter().any(|o| matches!(o, Op::AddCol(..) | Op::Index(_) | Op::MkTable(..) | Op::DropTable(_) | Op::TruncB));
    let prepared = texts(s, "SELECT gid FROM pg_prepared_xacts");
    let in_log = txlog.contains(&i);
    if !ddl && !prepared.contains(&format!("p{i}")) && end != End::Maint {
        // Atomicity: the step in flight left all of its writes or none, and txlog agrees.
        let mut with = m.clone();
        if matches!(end, End::Commit | End::Prepare(true)) {
            ops.iter().for_each(|o| with.apply(o));
        }
        let (none, all) = (rows_are(s, &m), rows_are(s, &with));
        assert!(
            if in_log { all } else { none },
            "seed {seed}: step {i} ({end:?}) in flight left a partial transaction: its txlog row is {}, and a and b hold {} of its writes",
            if in_log { "there" } else { "missing" },
            if all { "all" } else if none { "none" } else { "some" }
        );
    }
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
        assert!(!in_log || !matches!(end, End::Rollback | End::Prepare(false)), "seed {seed}: step {i} rolled back, but its rows show");
        in_log
    };
    append_log(dir, i, committed);
    log.push(committed);
    replay(seed, &log)
}

/// What differs between the rows read and those expected (both sorted): the first few of
/// each side's own, and what the table holds by each scan, for the failure message.
fn row_diff(s: &mut Session, table: &str, got: &[String], want: &[String]) -> String {
    let (g, w): (std::collections::BTreeSet<&String>, std::collections::BTreeSet<&String>) = (got.iter().collect(), want.iter().collect());
    let missing: Vec<&&String> = w.difference(&g).take(5).collect();
    let extra: Vec<&&String> = g.difference(&w).take(5).collect();
    let mut out = format!("missing {missing:?}, unexpected {extra:?}");
    for how in ["enable_seqscan = off; SET enable_bitmapscan = off", "enable_indexscan = off; SET enable_indexonlyscan = off; SET enable_bitmapscan = off"] {
        let _ = s.query(&format!("SET {how}"));
        out += &format!("; with {how}: {}", texts(s, &format!("SELECT count(*), sum(id) FROM {table} WHERE id > -1")).join(","));
        let _ = s.query("RESET enable_seqscan; RESET enable_indexscan; RESET enable_indexonlyscan; RESET enable_bitmapscan");
    }
    out + &format!("; ctids of {table}: {}", texts(s, &format!("SELECT count(*), count(DISTINCT ctid) FROM {table}")).join(","))
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
        if got != want_a {
            let d = row_diff(s, "a", &got, &want_a);
            panic!("{what}: a by {how}: {} rows, {na} expected: {d}", got.len());
        }
        let got = texts(s, "SELECT id, a_id, x FROM b WHERE id > -1 ORDER BY id");
        if got != want_b {
            let d = row_diff(s, "b", &got, &want_b);
            panic!("{what}: b by {how}: {} rows, {nb} expected: {d}", got.len());
        }
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
    if async_commit() {
        s.query("SET synchronous_commit = off").unwrap();
    }
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
    let us = r.below(20_000);
    // A third of the rounds die mid-write of a file: the `n`th one written from step `at` on
    // (a flush, compaction or checkpoint), `us` / 50 microseconds after it appeared.
    let on = (r.below(3) == 0).then(|| r.below(4));
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", "crash_random_child", "--nocapture", "--test-threads=1"])
        .env("CRASH_RANDOM_DIR", dir)
        .env("CRASH_SEED", seed.to_string())
        .env("CRASH_KILL_AT", at.to_string())
        .env("CRASH_KILL_US", if on.is_some() { us / 50 } else { us }.to_string());
    if let Some(n) = on {
        child.env("CRASH_KILL_ON", n.to_string());
    }
    let shadow = dir.join("shadow");
    if power_loss() {
        child.env("ROLL_POWERLOSS", format!("{}:{}", dir.join("data").display(), shadow.display()));
    }
    let out = child.output().unwrap();
    let why = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("panicked") || l.contains("seed"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(out.status.code().is_none(), "seed {seed} round {round}: the child failed: {why}");
    if power_loss() {
        cut_power(dir, &shadow, (r.below(2) == 0).then(|| r.below(1 << 30)), seed, round);
    }
    if let Ok(save) = std::env::var("CRASH_SAVE") {
        // (The crashed state, for `crash_inspect`.)
        let to = Path::new(&save).join(format!("s{seed}-r{round}"));
        let _ = std::fs::remove_dir_all(&to);
        std::fs::create_dir_all(&to).unwrap();
        let ok = std::process::Command::new("cp").arg("-a").arg(dir.join("data")).arg(dir.join("steps")).arg(&to).status().unwrap();
        assert!(ok.success());
    }
    let eng = open(&dir.join("data"));
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    let mut m = settle(&mut s, dir, seed);
    let n = read_log(dir).len();
    check(&mut s, seed, &m, &format!("round {round}, recovered after step {n}"));
    run_steps(&mut s, dir, seed, &mut m, n, n + 100, None);
    check(&mut s, seed, &m, &format!("round {round}, 100 steps after recovering"));
    drop(s);
    eng.shutdown();
    drop(eng);
    // A clean restart: the models its close kept (`sizing::kept`), then 50 more steps.
    let eng = open(&dir.join("data"));
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    check(&mut s, seed, &m, &format!("round {round}, after a clean restart"));
    run_steps(&mut s, dir, seed, &mut m, n + 100, n + 150, None);
    check(&mut s, seed, &m, &format!("round {round}, 50 steps after a clean restart"));
    drop(s);
    eng.shutdown();
}

/// Recover a copy of a state `CRASH_SAVE` kept (`CRASH_INSPECT=<dir>`, with `CRASH_SEED`),
/// settle it and check it, printing what the in-flight step left.
#[test]
fn crash_inspect() {
    let Ok(src) = std::env::var("CRASH_INSPECT") else {
        return;
    };
    let seed: u64 = std::env::var("CRASH_SEED").unwrap().parse().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ok = std::process::Command::new("cp").arg("-a").arg(format!("{src}/data")).arg(format!("{src}/steps")).arg(tmp.path()).status().unwrap();
    assert!(ok.success());
    let dir = tmp.path();
    let eng = open(&dir.join("data"));
    let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
    let i = read_log(dir).len();
    eprintln!("in flight: step {i}; txlog max {}", one(&mut s, "SELECT max(i), count(*) FROM txlog"));
    eprintln!("txlog row by seq scan: {}", one(&mut s, &format!("SET enable_indexscan = off; SET enable_bitmapscan = off; SELECT count(*) FROM txlog WHERE i = {i}")));
    s.query("RESET ALL").unwrap();
    let m = settle(&mut s, dir, seed);
    check(&mut s, seed, &m, &format!("inspected after step {}", read_log(dir).len()));
    eprintln!("inspect: ok");
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

/// `settle` fails on a recovered state with part of the in-flight step's writes (its txlog
/// row but not all of its rows), and on one that lost an acknowledged commit. (Built by SQL;
/// a build whose commits split or drop their WAL records fails `random_crashes` the same
/// ways, HANDOFF "Crash recovery".)
#[test]
fn recovery_oracle_rejects_partial_and_lost_acknowledged_transactions() {
    let empty = Model::default();
    let plain = |o: &Op| !matches!(o, Op::AddCol(..) | Op::Index(_) | Op::MkTable(..) | Op::DropTable(_) | Op::TruncB);
    let seed = (1..1000)
        .find(|&seed| {
            let (_, end, ops) = step(seed, 0, &empty);
            let mut with = empty.clone();
            ops.iter().for_each(|o| with.apply(o));
            end == End::Commit && with.a.len() >= 2 && ops.iter().all(plain)
        })
        .unwrap();
    for partial in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let eng = open(&dir.path().join("data"));
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        for sql in SCHEMA {
            s.query(sql).unwrap();
        }
        let mut model = Model::default();
        run_steps(&mut s, dir.path(), seed, &mut model, 0, 1, None);
        if partial {
            // Step 0 in flight, its txlog row there but one of its rows gone.
            let first = model.a.keys().next().unwrap();
            s.query(&format!("DELETE FROM a WHERE id = {first}")).unwrap();
            std::fs::write(dir.path().join("steps"), "").unwrap();
        } else {
            // Step 0 acknowledged, every effect of its commit gone.
            s.query("TRUNCATE b, a, txlog CASCADE").unwrap();
        }
        drop(s);
        eng.shutdown();
        let eng = open(&dir.path().join("data"));
        let mut s = eng.connect(1, "postgres", "postgres", "t").unwrap();
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            settle(&mut s, dir.path(), seed);
        }))
        .expect_err("settle accepted a broken recovered state");
        let message = (failure.downcast_ref::<String>().map(String::as_str))
            .or_else(|| failure.downcast_ref::<&str>().copied())
            .unwrap();
        let want = if partial { "partial transaction" } else { "txlog against the steps logged as committed" };
        assert!(message.contains(want), "{message}");
    }
}

/// The power-loss layer itself (`storage::powerloss`): a child writes through
/// `storage::durable`, then the power is cut. What an fsync covered stays (a file's writes, a
/// directory's entries); what none did goes: unsynced writes, a create or rename whose
/// directory wasn't fsynced.
#[cfg(feature = "powerloss")]
#[test]
fn power_cut_keeps_only_what_was_fsynced() {
    use pg_engine::storage::durable::{self, DFile};
    if let Ok(dir) = std::env::var("POWER_CUT_CHILD") {
        let d = Path::new(&dir);
        let a = DFile::create(&d.join("a")).unwrap();
        a.write_all_at(b"synced", 0).unwrap();
        durable::sync_dir(d).unwrap();
        a.sync_data().unwrap();
        durable::replace(&d.join("e"), b"durable", true).unwrap();
        a.write_all_at(b"-lost", 6).unwrap();
        // (Created and fsynced, but never in a fsynced directory.)
        DFile::create(&d.join("b")).unwrap().sync_all().unwrap();
        // (Renamed over an old file without a directory fsync: the old one comes back.)
        durable::replace(&d.join("c"), b"new", false).unwrap();
        sigkill();
    }
    let tmp = tempfile::tempdir().unwrap();
    let (data, shadow) = (tmp.path().join("data"), tmp.path().join("shadow"));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("c"), "old").unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "power_cut_keeps_only_what_was_fsynced", "--nocapture"])
        .env("POWER_CUT_CHILD", &data)
        .env("ROLL_POWERLOSS", format!("{}:{}", data.display(), shadow.display()))
        .output()
        .unwrap();
    assert!(out.status.code().is_none(), "{}", String::from_utf8_lossy(&out.stderr));
    pg_engine::storage::powerloss::power_cut(&data, &shadow, None).unwrap();
    let read = |n: &str| std::fs::read_to_string(data.join(n)).ok();
    assert_eq!(read("a").as_deref(), Some("synced"));
    assert_eq!(read("b"), None);
    assert_eq!(read("c").as_deref(), Some("old"));
    assert_eq!(read("e").as_deref(), Some("durable"));
}

/// Commits acknowledged into a WAL file the store just made (at open, and after a memtable
/// filled and the WAL rotated) survive a power cut: the file's directory entry is durable
/// before they are. The child commits key `k<i>` (fixed width: the store's user keys are
/// prefix-free) until `ROTATIONS` rotations have happened
/// and then 5 more, printing each acknowledged `i`, and dies.
#[cfg(feature = "powerloss")]
#[test]
fn power_cut_keeps_commits_in_a_new_wal() {
    use pg_engine::storage::{Store, StoreOptions};
    let opts = || StoreOptions { memtable_bytes: 64 << 10, background: false, ..Default::default() };
    let wals = |d: &Path| std::fs::read_dir(d).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "wal")).count();
    if let Ok(dir) = std::env::var("POWER_WAL_CHILD") {
        let (d, rotations) = (Path::new(&dir), std::env::var("ROTATIONS").unwrap().parse::<usize>().unwrap());
        let s = Store::open(&d.join("db"), opts()).unwrap();
        let (mut i, mut after) = (0u64, 0);
        while after < 5 {
            s.commit(vec![(format!("k{i:06}").into_bytes(), Some(vec![b'x'; 1000].into()))], |_| Ok(())).unwrap();
            println!("acked {i}");
            i += 1;
            if wals(&d.join("db")) > rotations {
                after += 1;
            }
        }
        sigkill();
    }
    for rotations in [0, 1, 3] {
        let tmp = tempfile::tempdir().unwrap();
        let (data, shadow) = (tmp.path().join("data"), tmp.path().join("shadow"));
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "power_cut_keeps_commits_in_a_new_wal", "--nocapture"])
            .env("POWER_WAL_CHILD", &data)
            .env("ROTATIONS", rotations.to_string())
            .env("ROLL_POWERLOSS", format!("{}:{}", data.display(), shadow.display()))
            .output()
            .unwrap();
        assert!(out.status.code().is_none(), "{}", String::from_utf8_lossy(&out.stderr));
        let acked = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.strip_prefix("acked ")?.parse::<u64>().ok()).max().unwrap();
        pg_engine::storage::powerloss::power_cut(&data, &shadow, None).unwrap();
        let s = Store::open(&data.join("db"), opts()).unwrap();
        let ts = s.visible_ts();
        let lost: Vec<u64> = (0..=acked).filter(|i| s.get(format!("k{i:06}").as_bytes(), ts).unwrap().is_none()).collect();
        assert!(lost.is_empty(), "{} WAL file(s), acknowledged commits lost to a power cut: {} of {}, the first {:?}", rotations, lost.len(), acked + 1, &lost[..lost.len().min(5)]);
    }
}

/// The power cut at every point of a fixed store workload: before each write, fsync, create,
/// rename and removal the store makes (`ROLL_POWERLOSS_AT`), through commits in rotating WAL
/// files, flushes, a compaction, a commit big enough to go in as an SST of its own and a
/// clean close. Each cut drops what no fsync covered (and, a second time, a seeded part of
/// it is kept). The store must open, every acknowledged commit must be there whole, and
/// nothing after the one in flight.
#[cfg(feature = "powerloss")]
#[test]
fn power_cut_at_every_point() {
    use pg_engine::storage::{Store, StoreOptions};
    let opts = || StoreOptions { memtable_bytes: 16 << 10, background: false, ..Default::default() };
    // Commit `i`'s keys: two small ones, or ten (an SST of its own) when i % 25 == 24.
    let keys = |i: u64| -> Vec<Vec<u8>> { (0..if i % 25 == 24 { 10 } else { 2 }).map(|k| format!("{i:04}-{k}").into_bytes()).collect() };
    if let Ok(dir) = std::env::var("POWER_EVERY_CHILD") {
        let s = Store::open(&Path::new(&dir).join("db"), opts()).unwrap();
        for i in 0..60u64 {
            let w = keys(i).into_iter().map(|k| (k, Some(vec![i as u8; 1000].into()))).collect();
            s.commit(w, |_| Ok(())).unwrap();
            println!("acked {i}");
            match i {
                20 | 45 => s.flush_memtables().unwrap(),
                30 => s.compact().unwrap(),
                _ => {}
            }
        }
        s.close().unwrap();
        println!("ops {}", pg_engine::storage::powerloss::ops());
        return;
    }
    let run = |at: Option<u64>, keep: Option<u64>| -> (String, Option<i32>) {
        let tmp = tempfile::tempdir().unwrap();
        let (data, shadow) = (tmp.path().join("data"), tmp.path().join("shadow"));
        let mut c = std::process::Command::new(std::env::current_exe().unwrap());
        c.args(["--exact", "power_cut_at_every_point", "--nocapture"])
            .env("POWER_EVERY_CHILD", &data)
            .env("ROLL_POWERLOSS", format!("{}:{}", data.display(), shadow.display()));
        if let Some(n) = at {
            c.env("ROLL_POWERLOSS_AT", n.to_string());
        }
        let out = c.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if at.is_none() {
            return (stdout, out.status.code());
        }
        assert!(out.status.code().is_none(), "cut at {at:?}: {}", String::from_utf8_lossy(&out.stderr));
        let acked = stdout.lines().filter_map(|l| l.strip_prefix("acked ")?.parse::<u64>().ok()).max();
        pg_engine::storage::powerloss::power_cut(&data, &shadow, keep).unwrap();
        let what = format!("cut before operation {} (keeping {keep:?}), {acked:?} acknowledged", at.unwrap());
        let s = Store::open(&data.join("db"), opts()).unwrap_or_else(|e| panic!("{what}: the store doesn't open: {e:?}"));
        let ts = s.visible_ts();
        let flight = acked.map_or(0, |a| a + 1);
        for i in 0..60u64 {
            let have: Vec<bool> = keys(i).iter().map(|k| s.get(k, ts).unwrap().is_some()).collect();
            let all = have.iter().all(|&h| h);
            let none = have.iter().all(|&h| !h);
            assert!(all || none, "{what}: commit {i} is partly there: {have:?}");
            if i < flight {
                assert!(all, "{what}: acknowledged commit {i} is lost");
            } else if i > flight {
                assert!(none, "{what}: commit {i}, after the one in flight, is there");
            }
        }
        (stdout, None)
    };
    let (out, code) = run(None, None);
    assert_eq!(code, Some(0), "{out}");
    let ops: u64 = out.lines().find_map(|l| l.strip_prefix("ops ")?.parse().ok()).unwrap();
    assert!(ops > 100, "only {ops} operations recorded");
    for at in 1..=ops {
        run(Some(at), None);
        run(Some(at), Some(at));
    }
}

/// Group commit keeps a prefix through a power cut: 8 threads commit at once, each commit
/// taking the next number of a sequence under the commit lock (so in commit order), with rotations
/// and flushes on the way; the power goes at a seeded moment. The numbers that survive must be
/// exactly 0..k, with no gap (a later commit never durable without an earlier one), and k past
/// every acknowledged one. Releasing locks before the fsync (pipelined commits) relies on this.
#[cfg(feature = "powerloss")]
#[test]
fn power_cut_keeps_a_prefix_of_concurrent_commits() {
    use pg_engine::storage::{Store, StoreOptions};
    use std::sync::atomic::{AtomicU64, Ordering};
    let opts = || StoreOptions { memtable_bytes: 64 << 10, ..Default::default() };
    if let Ok(dir) = std::env::var("POWER_PREFIX_CHILD") {
        let us: u64 = std::env::var("POWER_PREFIX_US").unwrap().parse().unwrap();
        let s = Store::open(&Path::new(&dir).join("db"), opts()).unwrap();
        let seq = std::sync::Arc::new(AtomicU64::new(0));
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_micros(us));
            sigkill();
        });
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (s, seq) = (s.clone(), seq.clone());
                std::thread::spawn(move || {
                    loop {
                        let mut n = 0;
                        s.commit_grouped(Vec::new(), |_, w| {
                            n = seq.fetch_add(1, Ordering::Relaxed);
                            w.push((format!("seq{n:08}").into_bytes(), Some(vec![7u8; 300].into())));
                            Ok(())
                        }, |_| {}, |_| {}).unwrap();
                        println!("acked {n}");
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
    }
    for (i, us) in [20_000u64, 60_000, 150_000, 400_000, 900_000].into_iter().enumerate() {
        for keep in [None, Some(i as u64 + 1)] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, shadow) = (tmp.path().join("data"), tmp.path().join("shadow"));
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "power_cut_keeps_a_prefix_of_concurrent_commits", "--nocapture"])
                .env("POWER_PREFIX_CHILD", &data)
                .env("POWER_PREFIX_US", us.to_string())
                .env("ROLL_POWERLOSS", format!("{}:{}", data.display(), shadow.display()))
                .output()
                .unwrap();
            assert!(out.status.code().is_none(), "{}", String::from_utf8_lossy(&out.stderr));
            // (A line cut by the kill isn't an acknowledgement.)
            let text = String::from_utf8_lossy(&out.stdout);
            let whole = &text[..text.rfind('\n').map_or(0, |n| n + 1)];
            let acked = whole.lines().filter_map(|l| l.strip_prefix("acked ")?.parse::<u64>().ok()).max();
            pg_engine::storage::powerloss::power_cut(&data, &shadow, keep).unwrap();
            let s = Store::open(&data.join("db"), opts()).unwrap();
            let have: Vec<u64> = s
                .scan(b"seq", b"seq~", s.visible_ts())
                .map(|r| String::from_utf8_lossy(&r.unwrap().0)[3..].parse::<u64>().unwrap())
                .collect();
            let what = format!("cut after {us} us (keeping {keep:?}): {} commits there, {acked:?} the last acknowledged", have.len());
            assert!(have.iter().enumerate().all(|(i, &n)| i as u64 == n), "{what}: not a prefix: {:?}", have.iter().enumerate().find(|(i, n)| *i as u64 != **n));
            assert!(acked.is_none_or(|a| (have.len() as u64) > a), "{what}: an acknowledged commit is lost");
        }
    }
}
