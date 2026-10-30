//! Differential testing against the reference PostgreSQL 18.6 with random schemas, data and
//! statements (`difftest/gen.rs`), plus engine-only oracles (TLP, NoREC, plan settings).
//! See `tests/difftest/STATUS.md` for how to run it and the current score.
//!
//! Every entry point is `#[ignore]`d: they need the reference server.
//!
//! ```sh
//! # a fixed-seed smoke run (a few minutes), failing on any mismatch
//! cargo test -p pg-engine --test difftest -- --ignored --nocapture smoke
//! # many seeds: cases land in DIFFTEST_OUT (one file per mismatch)
//! DIFFTEST_SEEDS=1-200 DIFFTEST_N=150 DIFFTEST_OUT=~/.cache/x/dt \
//!   cargo test -p pg-engine --test difftest -- --ignored --nocapture run
//! # reduce a case to a minimal reproducer (<case>.min.sql)
//! DIFFTEST_CASE=<file> cargo test -p pg-engine --test difftest -- --ignored --nocapture reduce
//! # run a case (or any .sql file) on both sides and show the outputs
//! DIFFTEST_CASE=<file> cargo test -p pg-engine --test difftest -- --ignored --nocapture replay
//! ```
//!
//! Environment: `DIFFTEST_REF` (127.0.0.1:55499), `DIFFTEST_PASSWORD` (pw), `DIFFTEST_DB`
//! (database name prefix on the reference, default `difftest`; use your own), `DIFFTEST_PLAN`
//! (share of queries re-run under random planner settings, 0.25), `DIFFTEST_ORACLES` (share of
//! TLP / NoREC checks per statement, 0.2), `DIFFTEST_HANG_SECS` (60: a statement running longer
//! is recorded as a hang and the process exits).

#![allow(clippy::type_complexity, clippy::wrong_self_convention)]

mod common;
#[path = "difftest/generate.rs"]
mod generate;

use common::pgclient::PgClient;
use common::{Event, Field, engine_query, engine_session, open_store};
use generate::{Stmt, schema, statements};
use pg_engine::session::Session;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------- outcomes

/// What one side produced for a statement.
#[derive(Clone, Debug, Default)]
struct Out {
    notices: Vec<Field>,
    error: Option<Field>,
    cols: Option<Vec<(String, u32)>>,
    rows: Vec<String>,
    tag: String,
    /// The engine panicked (message).
    panic: Option<String>,
}

fn out_of(events: Vec<Event>) -> Out {
    let mut o = Out::default();
    for e in events {
        match e {
            Event::Notice(f) => o.notices.push(f),
            Event::Error(f) => o.error = Some(f),
            Event::Result { cols, rows, tag } => {
                // (User type oids differ between the servers.)
                o.cols = cols.map(|c| c.into_iter().map(|(n, t)| (n, t.min(16384))).collect());
                o.rows = rows
                    .into_iter()
                    .map(|r| {
                        r.into_iter()
                            .map(|v| v.unwrap_or_else(|| "NULL".into()))
                            .collect::<Vec<_>>()
                            .join(" | ")
                    })
                    .collect();
                o.tag = tag;
            }
        }
    }
    o
}

impl Out {
    fn summary(&self) -> String {
        let mut s = String::new();
        if let Some(p) = &self.panic {
            let _ = writeln!(s, "PANIC: {p}");
        }
        for n in &self.notices {
            let _ = writeln!(s, "{} {}: {}", n.severity, n.code, n.message);
        }
        if let Some(e) = &self.error {
            let _ = writeln!(s, "ERROR {}: {}", e.code, e.message);
            if let Some(d) = &e.detail {
                let _ = writeln!(s, "DETAIL: {d}");
            }
            if let Some(h) = &e.hint {
                let _ = writeln!(s, "HINT: {h}");
            }
            if let Some(p) = e.position {
                let _ = writeln!(s, "POSITION: {p}");
            }
        } else {
            if let Some(c) = &self.cols {
                let h: Vec<String> = c.iter().map(|(n, t)| format!("{n}:{t}")).collect();
                let _ = writeln!(s, "{}", h.join(" | "));
            }
            for r in self.rows.iter().take(40) {
                let _ = writeln!(s, "{r}");
            }
            if self.rows.len() > 40 {
                let _ = writeln!(s, "... ({} rows)", self.rows.len());
            }
            let _ = writeln!(s, "-- {}", self.tag);
        }
        s
    }
    fn sorted(&self) -> Vec<String> {
        let mut v = self.rows.clone();
        v.sort();
        v
    }
}

/// Mismatch classes, most serious first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    Crash,
    Hang,
    WrongRows,
    WrongOrder,
    WrongState,
    Tlp,
    Norec,
    PlanVariance,
    WrongColumns,
    EngineError,
    MissingError,
    WrongError,
    EvalError,
    ErrorMessage,
    Notice,
    Tag,
    ErrorDetail,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::Crash => "crash",
            Class::Hang => "hang",
            Class::WrongRows => "wrong_rows",
            Class::WrongOrder => "wrong_order",
            Class::WrongState => "wrong_state",
            Class::Tlp => "tlp",
            Class::Norec => "norec",
            Class::PlanVariance => "plan_variance",
            Class::WrongColumns => "wrong_columns",
            Class::EngineError => "engine_error",
            Class::MissingError => "missing_error",
            Class::WrongError => "wrong_error",
            Class::EvalError => "eval_error",
            Class::ErrorMessage => "error_message",
            Class::Notice => "notice",
            Class::Tag => "tag",
            Class::ErrorDetail => "error_detail",
        }
    }
    fn from_name(s: &str) -> Option<Class> {
        ALL_CLASSES.iter().copied().find(|c| c.name() == s)
    }
}

const ALL_CLASSES: &[Class] = &[
    Class::Crash,
    Class::Hang,
    Class::WrongRows,
    Class::WrongOrder,
    Class::WrongState,
    Class::Tlp,
    Class::Norec,
    Class::PlanVariance,
    Class::WrongColumns,
    Class::EngineError,
    Class::MissingError,
    Class::WrongError,
    Class::EvalError,
    Class::ErrorMessage,
    Class::Notice,
    Class::Tag,
    Class::ErrorDetail,
];

/// Errors that say nothing about correctness: timeouts, resource limits.
fn noise(f: &Field) -> bool {
    matches!(f.code.as_str(), "57014" | "53200" | "54001" | "53400")
}

/// Compare the engine's outcome with the reference's.
fn compare(e: &Out, r: &Out, ordered: bool) -> Option<(Class, String)> {
    if let Some(p) = &e.panic {
        return Some((Class::Crash, p.clone()));
    }
    // A data error (22xxx, 21000) on one side only, or two different ones: which rows and
    // expressions get evaluated (and folded while planning) differs. Separate from errors
    // the parser and planner raise, which don't depend on that.
    let eval = |f: &Option<Field>| f.as_ref().is_none_or(|f| f.code.starts_with("22") || f.code == "21000" || f.code == "2202E");
    match (&e.error, &r.error) {
        (Some(x), _) if noise(x) => return None,
        (_, Some(y)) if noise(y) => return None,
        (x, y) if eval(x) && eval(y) && (x.is_some() || y.is_some()) && x.as_ref().map(|f| &f.code) != y.as_ref().map(|f| &f.code) => {
            let c = |f: &Option<Field>| f.as_ref().map(|f| format!("{}: {}", f.code, f.message)).unwrap_or_else(|| "ok".into());
            return Some((Class::EvalError, format!("{} vs {}", c(x), c(y))));
        }
        (None, Some(y)) => return Some((Class::MissingError, format!("{}: {}", y.code, y.message))),
        (Some(x), None) => return Some((Class::EngineError, format!("{}: {}", x.code, x.message))),
        (Some(x), Some(y)) => {
            if x.code != y.code {
                return Some((Class::WrongError, format!("{} vs {}", x.code, y.code)));
            }
            if x.message != y.message {
                return Some((Class::ErrorMessage, format!("{} | {}", x.message, y.message)));
            }
            if x.detail != y.detail || x.hint != y.hint || x.position != y.position {
                return Some((Class::ErrorDetail, x.code.clone()));
            }
        }
        (None, None) => {
            if e.cols != r.cols {
                return Some((Class::WrongColumns, format!("{:?} vs {:?}", e.cols, r.cols)));
            }
            if e.sorted() != r.sorted() {
                return Some((
                    Class::WrongRows,
                    format!("{} vs {} rows", e.rows.len(), r.rows.len()),
                ));
            }
            if ordered && e.rows != r.rows {
                return Some((Class::WrongOrder, String::new()));
            }
            if e.tag != r.tag {
                return Some((Class::Tag, format!("{} vs {}", e.tag, r.tag)));
            }
        }
    }
    let en: Vec<(&str, &str)> = e.notices.iter().map(|n| (n.code.as_str(), n.message.as_str())).collect();
    let rn: Vec<(&str, &str)> = r.notices.iter().map(|n| (n.code.as_str(), n.message.as_str())).collect();
    if en != rn {
        return Some((Class::Notice, format!("{en:?} vs {rn:?}")));
    }
    None
}

// ---------------------------------------------------------------------- the two sides

struct Watch {
    started: Option<Instant>,
    case: String,
}

static WATCH: Mutex<Watch> = Mutex::new(Watch { started: None, case: String::new() });
static PANIC_MSG: Mutex<Option<String>> = Mutex::new(None);

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn out_dir() -> PathBuf {
    let d = PathBuf::from(env(
        "DIFFTEST_OUT",
        &format!("{}/../../target/difftest", env!("CARGO_MANIFEST_DIR")),
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Start the hang watchdog (once) and the quiet panic hook that keeps panic messages.
fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "panic".into()
            };
            let loc = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
            *PANIC_MSG.lock().unwrap() = Some(format!("{msg}{loc}"));
            if std::env::var("DIFFTEST_PANIC_TRACE").is_ok() {
                prev(info);
            }
        }));
        let secs: u64 = env("DIFFTEST_HANG_SECS", "60").parse().unwrap();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let w = WATCH.lock().unwrap();
                if let Some(t) = w.started
                    && t.elapsed() > Duration::from_secs(secs)
                {
                    let p = out_dir().join(format!("hang-{}.sql", std::process::id()));
                    let _ = std::fs::write(&p, &w.case);
                    eprintln!("difftest: statement ran over {secs}s, recorded {}", p.display());
                    std::process::exit(3);
                }
            }
        });
    });
}

struct Pair {
    eng: Session,
    _dir: tempfile::TempDir,
    refc: PgClient,
    db: String,
}

const SESSION_SETUP: &[&str] = &[
    "SET statement_timeout = '10s'",
    "SET TimeZone = 'UTC'",
    "SET DateStyle = 'ISO, MDY'",
    "SET IntervalStyle = 'postgres'",
    "SET extra_float_digits = 1",
];

fn ref_addr() -> String {
    env("DIFFTEST_REF", "127.0.0.1:55499")
}

fn admin() -> PgClient {
    PgClient::connect(&ref_addr(), "postgres", &env("DIFFTEST_PASSWORD", "pw"), "postgres")
        .expect("connect to the reference PostgreSQL (DIFFTEST_REF)")
}

impl Pair {
    fn open(db: &str) -> Pair {
        let mut a = admin();
        a.simple_query(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).unwrap();
        let ev = a
            .simple_query(&format!(
                "CREATE DATABASE {db} TEMPLATE template0 LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LOCALE 'C' ENCODING 'UTF8'"
            ))
            .unwrap();
        assert!(!ev.iter().any(|e| matches!(e, Event::Error(_))), "{ev:?}");
        let refc = PgClient::connect(&ref_addr(), "postgres", &env("DIFFTEST_PASSWORD", "pw"), db).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let eng = engine_session(open_store(dir.path()), db);
        let mut p = Pair { eng, _dir: dir, refc, db: db.to_string() };
        for s in SESSION_SETUP {
            p.run(s);
        }
        p
    }

    fn engine(&mut self, sql: &str) -> Out {
        *PANIC_MSG.lock().unwrap() = None;
        WATCH.lock().unwrap().started = Some(Instant::now());
        let r = std::panic::catch_unwind(AssertUnwindSafe(|| engine_query(&mut self.eng, sql)));
        WATCH.lock().unwrap().started = None;
        match r {
            Ok(ev) => out_of(ev),
            Err(_) => {
                self.eng.recover_from_panic();
                let _ = self.eng.notices.drain(..);
                Out {
                    panic: Some(PANIC_MSG.lock().unwrap().take().unwrap_or_else(|| "panic".into())),
                    ..Out::default()
                }
            }
        }
    }

    fn reference(&mut self, sql: &str) -> Out {
        out_of(self.refc.simple_query(sql).expect("reference connection"))
    }

    fn run(&mut self, sql: &str) -> (Out, Out) {
        let r = self.reference(sql);
        let e = self.engine(sql);
        (e, r)
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        let mut a = admin();
        let _ = a.simple_query(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.db));
    }
}

// ---------------------------------------------------------------------- a case

/// A self-contained reproducer: setup statements, one statement to compare, settings.
#[derive(Clone, Debug)]
struct Case {
    class: Class,
    ordered: bool,
    setup: Vec<String>,
    stmt: String,
    /// For TLP / NoREC / plan-variance cases: the statement the first is compared with.
    other: Option<String>,
    note: String,
}

impl Case {
    fn text(&self, e: Option<&Out>, r: Option<&Out>) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "-- class: {}", self.class.name());
        let _ = writeln!(s, "-- ordered: {}", self.ordered);
        if !self.note.is_empty() {
            let _ = writeln!(s, "-- note: {}", self.note.replace('\n', " "));
        }
        for (k, o) in [("engine", e), ("reference", r)] {
            if let Some(o) = o {
                for l in o.summary().lines().take(30) {
                    let _ = writeln!(s, "-- {k}: {l}");
                }
            }
        }
        s.push('\n');
        for st in &self.setup {
            let _ = writeln!(s, "{}\n", copy_text(st));
        }
        if let Some(o) = &self.other {
            let _ = writeln!(s, "-- other\n{o};\n");
        }
        let _ = writeln!(s, "-- statement\n{}", copy_text(&self.stmt));
        s
    }

    fn parse(src: &str) -> Case {
        let mut class = Class::WrongRows;
        let mut ordered = false;
        let mut note = String::new();
        for l in src.lines() {
            if let Some(v) = l.strip_prefix("-- class: ") {
                class = Class::from_name(v.trim()).unwrap_or(Class::WrongRows);
            } else if let Some(v) = l.strip_prefix("-- ordered: ") {
                ordered = v.trim() == "true";
            } else if let Some(v) = l.strip_prefix("-- note: ") {
                note = v.to_string();
            }
        }
        let (body, stmt) = match src.rsplit_once("-- statement\n") {
            Some((b, s)) => (b, s),
            None => {
                // A plain .sql file: the last statement is the one compared.
                let mut st = common::statements(src);
                let last = st.pop().unwrap_or_default();
                return Case { class, ordered, setup: st.iter().map(|s| strip_comments(s)).collect(), stmt: strip_comments(&last), other: None, note };
            }
        };
        let (body, other) = match body.rsplit_once("-- other\n") {
            Some((b, o)) => (b, Some(strip_comments(o).trim_end_matches(';').to_string())),
            None => (body, None),
        };
        let setup = common::statements(body)
            .iter()
            .map(|s| strip_comments(s))
            .filter(|s| !s.is_empty())
            .collect();
        Case { class, ordered, setup, stmt: strip_comments(stmt).trim_end_matches(';').to_string(), other, note }
    }
}

/// A statement as a script has it, with its `;`: COPY FROM STDIN's data after it, up to `\.`.
fn copy_text(st: &str) -> String {
    match common::split_copy(st) {
        (sql, Some(data)) => format!("{sql};\n{data}\\."),
        (sql, None) => format!("{sql};"),
    }
}

/// A statement without its leading `--` comment lines and the trailing `;`.
fn strip_comments(s: &str) -> String {
    let t: Vec<&str> = s.lines().skip_while(|l| l.trim_start().starts_with("--") || l.trim().is_empty()).collect();
    t.join("\n").trim().trim_end_matches(';').trim().to_string()
}

// ---------------------------------------------------------------------- a run

#[derive(Default)]
struct Score {
    seeds: usize,
    statements: usize,
    queries: usize,
    both_error: usize,
    /// The errors both sides gave (the generator's waste), by code and message start.
    both_codes: BTreeMap<String, usize>,
    /// Every mismatch: `<class> s<seed> #<statement>`.
    found: Vec<String>,
    plan_runs: usize,
    oracle_runs: usize,
    by_class: BTreeMap<Class, usize>,
}

struct Runner {
    out: PathBuf,
    plan_share: f64,
    oracle_share: f64,
    score: Score,
    /// Mismatch signatures already recorded (class + a short key), to keep one case per kind.
    seen: std::collections::HashSet<String>,
}

fn signature(class: Class, detail: &str) -> String {
    let d: String = detail.chars().take(80).collect();
    format!("{}:{d}", class.name())
}

impl Runner {
    fn record(&mut self, seed: u64, idx: usize, case: Case, e: Option<&Out>, r: Option<&Out>, detail: &str) {
        *self.score.by_class.entry(case.class).or_default() += 1;
        self.score.found.push(format!("{} s{seed} #{idx}", case.class.name()));
        let sig = signature(case.class, detail);
        let fresh = self.seen.insert(sig.clone());
        let name = format!("{}-s{seed}-{idx}.sql", case.class.name());
        if fresh || matches!(case.class, Class::WrongRows | Class::Crash | Class::Tlp | Class::Norec | Class::PlanVariance | Class::WrongState | Class::WrongOrder) {
            let _ = std::fs::write(self.out.join(&name), case.text(e, r));
        }
        eprintln!("  [{}] seed {seed} #{idx}: {}", case.class.name(), detail.chars().take(160).collect::<String>());
    }

    fn run_seed(&mut self, seed: u64, n: usize, db_prefix: &str) {
        let (mut sch, setup) = schema(seed);
        let mut pair = Pair::open(&format!("{db_prefix}_{seed}"));
        let mut history: Vec<String> = Vec::new();
        for st in &setup {
            let (e, r) = pair.run(st);
            history.push(st.clone());
            if r.error.is_some() {
                // Objects the reference refused don't exist for the statements after.
                let words: Vec<&str> = st.split_whitespace().collect();
                if words.len() > 2 && (words[1] == "VIEW" || words[1] == "FUNCTION") {
                    let name = words[2].split('(').next().unwrap().to_string();
                    sch.tables.retain(|t| t.name != name);
                    sch.funcs.retain(|f| f.name != name);
                    sch.srfs.retain(|f| f.0 != name);
                }
            }
            if let Some((class, detail)) = compare(&e, &r, false) {
                let case = Case { class, ordered: false, setup: history[..history.len() - 1].to_vec(), stmt: st.clone(), other: None, note: detail.clone() };
                self.record(seed, 0, case, Some(&e), Some(&r), &detail);
            }
        }
        self.score.seeds += 1;
        let stmts = statements(seed, &sch, n);
        let mut rng = generate::Rng::new(seed ^ 0xABCD);
        let mut diverged = false;
        let last = stmts.len();
        for (i, st) in stmts.into_iter().enumerate() {
            let idx = i + 1;
            self.score.statements += 1;
            let (sql, ordered, table) = match &st {
                Stmt::Query(q) => (q.sql.clone(), q.ordered, None),
                Stmt::Dml { sql, table, ordered } => (sql.clone(), *ordered, Some(table.clone())),
                Stmt::Other(s) => (s.clone(), false, None),
            };
            {
                let mut w = WATCH.lock().unwrap();
                w.case = Case { class: Class::Hang, ordered, setup: history.clone(), stmt: sql.clone(), other: None, note: format!("seed {seed} #{idx}") }.text(None, None);
                // (DIFFTEST_ECHO: the statement about to run, as a case file, for one that
                // takes the process down, a stack overflow, which no hook sees.)
                if std::env::var("DIFFTEST_ECHO").is_ok() {
                    let _ = std::fs::write(out_dir().join(format!("current-s{seed}.sql")), &w.case);
                }
            }
            let (e, r) = pair.run(&sql);
            if matches!(st, Stmt::Query(_)) {
                self.score.queries += 1;
            }
            if e.error.is_some()
                && let Some(f) = &r.error
            {
                self.score.both_error += 1;
                let key: String = format!("{} {}", f.code, f.message).chars().take(60).collect();
                *self.score.both_codes.entry(key).or_default() += 1;
            }
            let mismatch = compare(&e, &r, ordered);
            if let Some((class, detail)) = &mismatch {
                let case = Case { class: *class, ordered, setup: history.clone(), stmt: sql.clone(), other: None, note: detail.clone() };
                self.record(seed, idx, case, Some(&e), Some(&r), detail);
            }
            if !matches!(st, Stmt::Query(_)) {
                history.push(sql.clone());
            }
            if let Some(t) = table {
                // The table's contents after the DML.
                let q = format!("SELECT * FROM {t} x ORDER BY x::text COLLATE \"C\"");
                let (e2, r2) = pair.run(&q);
                if let Some((class, detail)) = compare(&e2, &r2, true) {
                    let class = if matches!(class, Class::WrongRows | Class::WrongOrder) { Class::WrongState } else { class };
                    let case = Case { class, ordered: true, setup: history.clone(), stmt: q, other: None, note: detail.clone() };
                    self.record(seed, idx, case, Some(&e2), Some(&r2), &detail);
                    if class == Class::WrongState {
                        // The two sides' data diverged: the rest of the seed would only repeat it.
                        eprintln!("  seed {seed}: data diverged after #{idx}, next seed");
                        diverged = true;
                        break;
                    }
                }
                continue;
            }
            let Stmt::Query(q) = &st else { continue };
            let ok = e.error.is_none() && r.error.is_none() && mismatch.is_none();
            // Planner settings: both sides again under the same settings, and the engine's
            // rows must not depend on the plan.
            if rng.chance(self.plan_share) && e.error.is_none() && e.panic.is_none() {
                self.score.plan_runs += 1;
                let mut g = generate::Gen::new(rng.next(), &sch);
                let sets = g.plan_settings();
                for s in &sets {
                    pair.run(s);
                }
                let (e2, r2) = pair.run(&q.sql);
                for s in &sets {
                    let g = s.trim_start_matches("SET ").split(' ').next().unwrap();
                    pair.run(&format!("RESET {g}"));
                }
                let mut setup = history.clone();
                setup.extend(sets.iter().cloned());
                if let Some((class, detail)) = compare(&e2, &r2, q.ordered) {
                    let case = Case { class, ordered: q.ordered, setup: setup.clone(), stmt: q.sql.clone(), other: None, note: format!("{} [{}]", detail, sets.join("; ")) };
                    self.record(seed, idx, case, Some(&e2), Some(&r2), &detail);
                } else if ok && e2.error.is_none() && e2.panic.is_none() && e.sorted() != e2.sorted() {
                    let case = Case { class: Class::PlanVariance, ordered: false, setup, stmt: q.sql.clone(), other: None, note: sets.join("; ") };
                    self.record(seed, idx, case, Some(&e2), Some(&e), "rows depend on planner settings");
                }
            }
            if rng.chance(self.oracle_share) {
                self.oracles(&mut pair, &sch, &history, seed, idx, rng.next());
            }
        }
        if !diverged {
            self.final_state(&mut pair, &history, seed, last + 1);
        }
        drop(pair);
    }

    /// The database's final state: which tables there are, and every table's contents.
    fn final_state(&mut self, pair: &mut Pair, history: &[String], seed: u64, idx: usize) {
        pair.run("ROLLBACK");
        let list = "SELECT relname, relkind FROM pg_class WHERE relnamespace = 'public'::regnamespace AND relkind IN ('r', 'p', 'v', 'S') ORDER BY 1 COLLATE \"C\"";
        let mut qs = vec![list.to_string()];
        let (_, r) = pair.run(list);
        qs.extend(
            r.rows
                .iter()
                .filter_map(|row| row.strip_suffix(" | r").or(row.strip_suffix(" | p")))
                .map(|t| format!("SELECT * FROM \"{t}\" x ORDER BY x::text COLLATE \"C\"")),
        );
        for q in qs {
            let (e, r) = pair.run(&q);
            if let Some((class, detail)) = compare(&e, &r, true) {
                let class = if matches!(class, Class::WrongRows | Class::WrongOrder) { Class::WrongState } else { class };
                let case = Case { class, ordered: true, setup: history.to_vec(), stmt: q, other: None, note: format!("final state: {detail}") };
                self.record(seed, idx, case, Some(&e), Some(&r), &detail);
                return;
            }
        }
    }

    /// TLP and NoREC on the engine: a query must return the same rows as its predicate
    /// partitions, and a filtered count the same as a count of the predicate's values.
    /// A difference the reference shows too is the generator's fault and is dropped.
    fn oracles(&mut self, pair: &mut Pair, sch: &generate::Schema, history: &[String], seed: u64, idx: usize, s: u64) {
        let mut g = generate::Gen::new(s, sch);
        let (from, cols, pred) = g.oracle_parts();
        self.score.oracle_runs += 1;
        let list = if cols.is_empty() { "1".to_string() } else { cols.join(", ") };
        let row = if cols.is_empty() { "1".to_string() } else { format!("ROW({list})") };
        let (kind, a, b) = match g.rng.below(4) {
            0 => (
                Class::Tlp,
                format!("SELECT {list} FROM {from}"),
                format!(
                    "SELECT {list} FROM {from} WHERE {pred} UNION ALL SELECT {list} FROM {from} WHERE NOT ({pred}) UNION ALL SELECT {list} FROM {from} WHERE ({pred}) IS NULL"
                ),
            ),
            1 => (
                Class::Tlp,
                format!("SELECT DISTINCT {list} FROM {from}"),
                format!(
                    "SELECT {list} FROM {from} WHERE {pred} UNION SELECT {list} FROM {from} WHERE NOT ({pred}) UNION SELECT {list} FROM {from} WHERE ({pred}) IS NULL"
                ),
            ),
            2 => (
                Class::Tlp,
                format!("SELECT count(*), max(({row})::text) FROM {from}"),
                format!(
                    "SELECT sum(c), max(m) FROM (SELECT count(*) c, max(({row})::text) m FROM {from} WHERE {pred} UNION ALL SELECT count(*), max(({row})::text) FROM {from} WHERE NOT ({pred}) UNION ALL SELECT count(*), max(({row})::text) FROM {from} WHERE ({pred}) IS NULL) u"
                ),
            ),
            _ => (
                Class::Norec,
                format!("SELECT count(*) FROM {from} WHERE {pred}"),
                format!("SELECT sum(CASE WHEN {pred} THEN 1 ELSE 0 END) FROM {from}"),
            ),
        };
        let ea = pair.engine(&a);
        let eb = pair.engine(&b);
        if ea.panic.is_some() || eb.panic.is_some() {
            let case = Case { class: Class::Crash, ordered: false, setup: history.to_vec(), stmt: if ea.panic.is_some() { a } else { b }, other: None, note: String::new() };
            let p = ea.panic.clone().or(eb.panic.clone()).unwrap();
            self.record(seed, idx, case, Some(&ea), Some(&eb), &p);
            return;
        }
        if ea.error.is_some() || eb.error.is_some() {
            return;
        }
        if norm(&ea) != norm(&eb) {
            // Confirm it on the reference: PostgreSQL must agree with itself.
            let ra = pair.reference(&a);
            let rb = pair.reference(&b);
            let ref_agrees = ra.error.is_none() && rb.error.is_none() && norm(&ra) == norm(&rb);
            if ref_agrees {
                let a_wrong = norm(&ea) != norm(&ra);
                let (wrong, other) = if a_wrong { (a, b) } else { (b, a) };
                let case = Case { class: kind, ordered: false, setup: history.to_vec(), stmt: wrong, other: Some(other), note: String::new() };
                let (ew, rw) = if a_wrong { (&ea, &ra) } else { (&eb, &rb) };
                self.record(seed, idx, case, Some(ew), Some(rw), "partitions disagree");
            }
        }
    }
}

/// Rows sorted, NULL as 0 (a sum over no rows is NULL where a count is 0).
fn norm(o: &Out) -> Vec<String> {
    let mut v: Vec<String> = o.rows.iter().map(|r| if r == "NULL" { "0".to_string() } else { r.clone() }).collect();
    v.sort();
    v
}

fn seeds() -> Vec<u64> {
    let s = env("DIFFTEST_SEEDS", "1-20");
    let mut v = Vec::new();
    for part in s.split(',') {
        if let Some((a, b)) = part.split_once('-') {
            v.extend(a.trim().parse::<u64>().unwrap()..=b.trim().parse::<u64>().unwrap());
        } else {
            v.push(part.trim().parse().unwrap());
        }
    }
    v
}

fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            pg_engine::stack::set_limit(60 << 20);
            f()
        })
        .unwrap()
        .join()
        .unwrap()
}

fn run_seeds(seeds: &[u64], n: usize) -> Score {
    install();
    let mut runner = Runner {
        out: out_dir(),
        plan_share: env("DIFFTEST_PLAN", "0.25").parse().unwrap(),
        oracle_share: env("DIFFTEST_ORACLES", "0.2").parse().unwrap(),
        score: Score::default(),
        seen: Default::default(),
    };
    let prefix = env("DIFFTEST_DB", "difftest");
    let started = Instant::now();
    for &seed in seeds {
        let t = Instant::now();
        runner.run_seed(seed, n, &prefix);
        eprintln!("seed {seed}: {:.1}s", t.elapsed().as_secs_f64());
    }
    let s = &runner.score;
    let mut line = format!(
        "seeds={} statements={} queries={} both_error={} plan_runs={} oracle_runs={} secs={:.0}",
        s.seeds,
        s.statements,
        s.queries,
        s.both_error,
        s.plan_runs,
        s.oracle_runs,
        started.elapsed().as_secs_f64()
    );
    for (c, k) in &s.by_class {
        let _ = write!(line, " {}={k}", c.name());
    }
    eprintln!("difftest: {line}");
    let mut top: Vec<(&String, &usize)> = s.both_codes.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    for (k, n) in top.iter().take(12) {
        eprintln!("  both error {n:5}  {k}");
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(runner.out.join("runs.log"))
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{} {line}", env("DIFFTEST_SEEDS", "1-20"))
        });
    std::mem::take(&mut runner.score)
}

#[test]
#[ignore]
fn run() {
    let n: usize = env("DIFFTEST_N", "150").parse().unwrap();
    let seeds = seeds();
    on_big_stack(move || {
        run_seeds(&seeds, n);
    });
}

/// Wider-grammar seeds whose mismatches were fixed, each run up to the statement that showed
/// one (the smoke run checks them too; their other mismatches are in smoke_known.txt).
const SMOKE_EXTRA: &[(u64, usize)] = &[(1604, 74), (1777, 35), (1827, 40), (1829, 98), (1830, 10), (1832, 5)];

/// The regression check other agents run: fixed seeds, a few minutes. It fails on a
/// mismatch (of a class above `error_message`) that `difftest/smoke_known.txt` doesn't list:
/// the open ones STATUS.md describes. `DIFFTEST_SMOKE_RECORD=1` rewrites the list (after a
/// fix, or a generator change, which changes every seed's statements).
#[test]
#[ignore]
fn smoke() {
    let score = on_big_stack(|| {
        let seeds: Vec<u64> = (1..=env("DIFFTEST_SMOKE_SEEDS", "25").parse().unwrap()).collect();
        let mut score = run_seeds(&seeds, 80);
        for &(seed, n) in SMOKE_EXTRA {
            score.found.extend(run_seeds(&[seed], n).found);
        }
        score
    });
    let serious = |f: &&String| {
        let c = f.split(' ').next().and_then(Class::from_name).unwrap_or(Class::Crash);
        c < Class::ErrorMessage
    };
    let mut found: Vec<&String> = score.found.iter().filter(serious).collect();
    found.dedup();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/difftest/smoke_known.txt");
    if std::env::var("DIFFTEST_SMOKE_RECORD").is_ok() {
        let mut text = String::from("# Mismatches the smoke run still finds (difftest.rs `smoke`).\n");
        for f in &found {
            let _ = writeln!(text, "{f}");
        }
        std::fs::write(&path, text).unwrap();
        eprintln!("difftest smoke: recorded {} known mismatches", found.len());
        return;
    }
    let known: std::collections::HashSet<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(String::from)
        .collect();
    let new: Vec<&&String> = found.iter().filter(|f| !known.contains(f.as_str())).collect();
    let gone = known.iter().filter(|k| !found.contains(k)).count();
    if gone > 0 {
        eprintln!("difftest smoke: {gone} known mismatches no longer found (DIFFTEST_SMOKE_RECORD=1 updates the list)");
    }
    assert!(
        new.is_empty(),
        "difftest smoke found new mismatches (cases in DIFFTEST_OUT): {}",
        new.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
    );
}

// ---------------------------------------------------------------------- replay and reduce

// The reducer replays the same setup many times with a different query: keep the
// databases while the setup stays and the statements only read.
thread_local! {
    static KEPT: std::cell::RefCell<Option<(Vec<String>, Pair)>> = const { std::cell::RefCell::new(None) };
}

/// Drop the kept databases (before the thread ends: a thread-local's destructor can't
/// talk to the server).
fn forget_kept() {
    KEPT.with(|k| *k.borrow_mut() = None);
}

/// Run a case on fresh databases (or the kept ones): the outcome of its statement on both
/// sides, and of the statement it is compared with, for oracle cases.
fn replay(case: &Case, db: &str) -> (Out, Out, Option<(Out, Out)>) {
    let reads = |s: &str| {
        let u = s.trim_start().to_ascii_uppercase();
        (u.starts_with("SELECT") || u.starts_with("WITH") || u.starts_with("VALUES") || u.starts_with('('))
            && !["INSERT ", "UPDATE ", "DELETE ", "MERGE ", "NEXTVAL", "SETVAL", "SET_CONFIG", " INTO "]
                .iter()
                .any(|w| u.contains(w))
    };
    let pure = reads(&case.stmt) && case.other.as_deref().is_none_or(reads);
    KEPT.with(|k| {
        let mut k = k.borrow_mut();
        let reuse = pure && k.as_ref().is_some_and(|(s, p)| *s == case.setup && p.db == db);
        if !reuse {
            *k = None;
            let mut p = Pair::open(db);
            for s in &case.setup {
                p.run(s);
            }
            *k = Some((case.setup.clone(), p));
        }
        let p = &mut k.as_mut().unwrap().1;
        let (e, r) = p.run(&case.stmt);
        let other = case.other.as_ref().map(|o| p.run(o));
        if !pure {
            *k = None;
        }
        (e, r, other)
    })
}

/// Whether a case still shows its class.
fn still_fails(case: &Case, db: &str, want: &(Class, String)) -> bool {
    let (e, r, other) = replay(case, db);
    match case.class {
        Class::Tlp | Class::Norec | Class::PlanVariance => {
            let Some((eo, ro)) = other else { return false };
            if [&e, &r, &eo, &ro].iter().any(|o| o.error.is_some() || o.panic.is_some()) {
                return false;
            }
            norm(&e) != norm(&eo) && norm(&r) == norm(&ro)
        }
        Class::WrongState => matches!(compare(&e, &r, true), Some((Class::WrongRows, _))),
        _ => match compare(&e, &r, case.ordered) {
            Some((c, d)) => {
                c == want.0
                    && match c {
                        // Keep the same error on the side that has one.
                        Class::EngineError | Class::MissingError | Class::WrongError | Class::EvalError => d == want.1,
                        Class::Crash => d.split(" at ").next() == want.1.split(" at ").next(),
                        _ => true,
                    }
                    // A statement the reference can't parse any more is not a reduction.
                    && r.error.as_ref().is_none_or(|f| f.code != "42601")
            }
            None => false,
        },
    }
}

#[test]
#[ignore]
fn replay_case() {
    let path = std::env::var("DIFFTEST_CASE").expect("DIFFTEST_CASE");
    on_big_stack(move || {
        install();
        let case = Case::parse(&std::fs::read_to_string(&path).unwrap());
        let (e, r, other) = replay(&case, &format!("{}_replay", env("DIFFTEST_DB", "difftest")));
        println!("== engine\n{}\n== reference\n{}", e.summary(), r.summary());
        if let Some((eo, ro)) = other {
            println!("== other, engine\n{}\n== other, reference\n{}", eo.summary(), ro.summary());
        }
        println!("== verdict: {:?}", compare(&e, &r, case.ordered));
        forget_kept();
    });
}

/// Run every statement of a file (`DIFFTEST_CASE`) on both sides and print each outcome,
/// once when both agree.
#[test]
#[ignore]
fn probe() {
    let path = std::env::var("DIFFTEST_CASE").expect("DIFFTEST_CASE");
    on_big_stack(move || {
        install();
        let src = std::fs::read_to_string(&path).unwrap();
        let mut p = Pair::open(&format!("{}_probe", env("DIFFTEST_DB", "difftest")));
        for s in common::statements(&src) {
            let s = strip_comments(&s);
            if s.is_empty() {
                continue;
            }
            let (e, r) = p.run(&s);
            println!("> {s}");
            match compare(&e, &r, false) {
                None => print!("{}", e.summary()),
                Some((c, _)) => print!("!! {}\n-- engine:\n{}-- reference:\n{}", c.name(), e.summary(), r.summary()),
            }
            println!();
        }
    });
}

/// Tokens of a statement, for the reducer: words, literals, punctuation.
fn tokens(s: &str) -> Vec<String> {
    let b: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if c == '\'' || c == '"' {
            i += 1;
            while i < b.len() {
                if b[i] == c {
                    if i + 1 < b.len() && b[i + 1] == c {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
        } else if c == '$' && b.get(i + 1) == Some(&'$') {
            // A dollar-quoted body as one token.
            i += 2;
            while i + 1 < b.len() && !(b[i] == '$' && b[i + 1] == '$') {
                i += 1;
            }
            i += 2;
        } else if c.is_alphanumeric() || c == '_' || c == '.' {
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '.' || b[i] == '$') {
                i += 1;
            }
        } else if "(),;[]".contains(c) {
            i += 1;
        } else {
            while i < b.len() && "+-*/<>=~!@#%^&|`?:".contains(b[i]) {
                i += 1;
            }
            if i == start {
                i += 1;
            }
        }
        out.push(b[start..i.min(b.len())].iter().collect());
    }
    out
}

fn join_tokens(t: &[String]) -> String {
    let mut s = String::new();
    for (i, x) in t.iter().enumerate() {
        let glue = x == "::" || (i > 0 && t[i - 1] == "::");
        if i > 0 && !glue && x != ")" && x != "," && x != "]" && t[i - 1] != "(" && t[i - 1] != "[" {
            s.push(' ');
        }
        s.push_str(x);
    }
    s
}

/// Balanced (parentheses) token spans: (start, end inclusive).
fn groups(t: &[String]) -> Vec<(usize, usize)> {
    let mut st = Vec::new();
    let mut out = Vec::new();
    for (i, x) in t.iter().enumerate() {
        if x == "(" {
            st.push(i);
        } else if x == ")"
            && let Some(s) = st.pop()
        {
            out.push((s, i));
        }
    }
    out
}

fn is_clause(x: &str) -> bool {
    matches!(
        x.to_ascii_uppercase().as_str(),
        "SELECT" | "FROM" | "WHERE" | "GROUP" | "HAVING" | "ORDER" | "LIMIT" | "OFFSET" | "UNION" | "INTERSECT" | "EXCEPT" | "WINDOW" | "RETURNING" | "ON" | "USING" | "JOIN" | "LEFT" | "RIGHT" | "FULL" | "CROSS" | "INNER" | "SET" | "WHEN" | "VALUES" | "FOR" | "AS"
    )
}

/// Candidate simplifications of one statement, shortest first.
fn candidates(stmt: &str) -> Vec<String> {
    let t = tokens(stmt);
    let mut out = Vec::new();
    let gs = groups(&t);
    let kw = |x: &str| x.to_ascii_uppercase();
    let splice = |a: usize, b: usize, with: &[String]| -> String {
        let mut v = t[..a].to_vec();
        v.extend_from_slice(with);
        v.extend_from_slice(&t[b..]);
        join_tokens(&v)
    };
    // A parenthesized group as a simple value, or one side of its top-level operator.
    for &(s, e) in gs.iter().rev() {
        for rep in ["NULL", "1", "true", "'a'"] {
            out.push(splice(s, e + 1, &[rep.to_string()]));
        }
        let inner = &t[s + 1..e];
        let mut depth = 0;
        for (k, x) in inner.iter().enumerate() {
            if x == "(" {
                depth += 1;
            } else if x == ")" {
                depth -= 1;
            } else if depth == 0
                && matches!(kw(x).as_str(), "AND" | "OR" | "+" | "-" | "*" | "/" | "||" | "%" | "UNION" | "EXCEPT" | "INTERSECT" | "=" | "<" | ">" | "<>" | "<=" | ">=")
            {
                out.push(splice(s + 1, e, &inner[..k]));
                out.push(splice(s + 1, e, &inner[k + 1..]));
            }
        }
    }
    let mut depth_of = vec![0i32; t.len()];
    let mut d = 0;
    for (i, x) in t.iter().enumerate() {
        if x == ")" {
            d -= 1;
        }
        depth_of[i] = d;
        if x == "(" {
            d += 1;
        }
    }
    // List items on either side of a comma.
    for i in 0..t.len() {
        if t[i] != "," {
            continue;
        }
        let mut j = i;
        while j > 0 && !(depth_of[j - 1] == depth_of[i] && (t[j - 1] == "," || t[j - 1] == "(" || is_clause(&t[j - 1]))) {
            j -= 1;
        }
        out.push(splice(j, i + 1, &[]));
        let mut k = i + 1;
        while k < t.len() && !(depth_of[k] == depth_of[i] && (t[k] == "," || t[k] == ")" || is_clause(&t[k]))) {
            k += 1;
        }
        out.push(splice(i, k, &[]));
    }
    // Top-level AND / OR operands outside parentheses.
    for i in 0..t.len() {
        if matches!(kw(&t[i]).as_str(), "AND" | "OR") {
            let mut j = i;
            while j > 0 && !(depth_of[j - 1] == depth_of[i] && (is_clause(&t[j - 1]) || t[j - 1] == "(" || matches!(kw(&t[j - 1]).as_str(), "AND" | "OR"))) {
                j -= 1;
            }
            out.push(splice(j, i + 1, &[]));
            let mut k = i + 1;
            while k < t.len() && !(depth_of[k] == depth_of[i] && (is_clause(&t[k]) || t[k] == ")" || matches!(kw(&t[k]).as_str(), "AND" | "OR"))) {
                k += 1;
            }
            out.push(splice(i, k, &[]));
        }
    }
    // Whole clauses, and modifier keywords.
    for i in 0..t.len() {
        let k = kw(&t[i]);
        if matches!(k.as_str(), "DISTINCT" | "LATERAL" | "MATERIALIZED" | "NOT" | "ONLY" | "ALL" | "DESC" | "SYMMETRIC") {
            out.push(splice(i, i + 1, &[]));
            continue;
        }
        if !matches!(k.as_str(), "WHERE" | "ORDER" | "LIMIT" | "OFFSET" | "HAVING" | "GROUP" | "FILTER" | "RETURNING" | "WHEN" | "FOR" | "ELSE") {
            continue;
        }
        let mut j = i + 1;
        while j < t.len() && !(depth_of[j] == depth_of[i] && ((is_clause(&t[j]) && j > i + 1 && kw(&t[j]) != "BY") || t[j] == ")" || t[j] == ";" || (k == "ELSE" && kw(&t[j]) == "END"))) {
            j += 1;
        }
        out.push(splice(i, j, &[]));
    }
    // A join as its left side: `(a JOIN b ON c)` -> `a`.
    for &(s, e) in &gs {
        let inner = &t[s + 1..e];
        if let Some(p) = inner.iter().position(|x| kw(x) == "JOIN") {
            let mut l = p;
            while l > 0 && matches!(kw(&inner[l - 1]).as_str(), "LEFT" | "RIGHT" | "FULL" | "CROSS" | "INNER" | "OUTER") {
                l -= 1;
            }
            out.push(splice(s, e + 1, &inner[..l]));
        }
    }
    // CASE ... END -> its first THEN value or its ELSE.
    for i in 0..t.len() {
        if kw(&t[i]) != "CASE" {
            continue;
        }
        let mut depth = 0;
        let mut then: Option<usize> = None;
        let mut els: Option<usize> = None;
        for j in i + 1..t.len() {
            let k = kw(&t[j]);
            if k == "CASE" {
                depth += 1;
            } else if k == "END" {
                if depth == 0 {
                    if let Some(th) = then {
                        let mut stop = th + 1;
                        while stop < j && !matches!(kw(t[stop].as_str()).as_str(), "WHEN" | "ELSE") {
                            stop += 1;
                        }
                        out.push(splice(i, j + 1, &t[th + 1..stop]));
                    }
                    if let Some(el) = els {
                        out.push(splice(i, j + 1, &t[el + 1..j]));
                    }
                    break;
                }
                depth -= 1;
            } else if k == "THEN" && depth == 0 && then.is_none() {
                then = Some(j);
            } else if k == "ELSE" && depth == 0 {
                els = Some(j);
            }
        }
    }
    // f(x) -> x; f(x, y) -> x.
    for &(s, e) in &gs {
        if s > 0 && t[s - 1].chars().all(|c| c.is_alphanumeric() || c == '_') && !is_clause(&t[s - 1]) && e > s + 1 {
            let inner = &t[s + 1..e];
            let first_end = (0..inner.len()).find(|&k| inner[k] == "," && depth_of[s + 1 + k] == depth_of[s] + 1).unwrap_or(inner.len());
            let mut v = vec!["(".to_string()];
            v.extend_from_slice(&inner[..first_end]);
            v.push(")".into());
            out.push(splice(s - 1, e + 1, &v));
        }
    }
    // Casts `x::type` -> `x`.
    for i in 1..t.len().saturating_sub(1) {
        if t[i] == "::" {
            let mut end = i + 2;
            if t.get(end).map(|x| x == "[") == Some(true) {
                end += 2;
            }
            out.push(splice(i, end.min(t.len()), &[]));
        }
    }
    out.retain(|c| c.len() < stmt.len());
    out.sort_by_key(|c| c.len());
    out.dedup();
    out
}

/// Rows of an INSERT ... VALUES: (prefix, rows, suffix).
fn insert_rows(s: &str) -> Option<(String, Vec<String>, String)> {
    if !s.starts_with("INSERT") {
        return None;
    }
    let t = tokens(s);
    let vi = t.iter().position(|x| x.eq_ignore_ascii_case("VALUES"))?;
    let mut rows = Vec::new();
    let mut i = vi + 1;
    let mut depth = 0;
    let mut start = i;
    while i < t.len() {
        if t[i] == "(" {
            depth += 1;
        } else if t[i] == ")" {
            depth -= 1;
            if depth == 0 {
                rows.push(join_tokens(&t[start..=i]));
                if t.get(i + 1).map(|x| x == ",") != Some(true) {
                    return Some((join_tokens(&t[..=vi]), rows, join_tokens(&t[i + 1..])));
                }
                i += 1;
                start = i + 1;
            }
        }
        i += 1;
    }
    None
}

/// ddmin-style: drop chunks of a list while the case still fails.
fn reduce_list<T: Clone>(items: &mut Vec<T>, mut test: impl FnMut(&[T]) -> bool) {
    let mut chunk = items.len().div_ceil(2).max(1);
    loop {
        let mut i = 0;
        let mut changed = false;
        while i < items.len() {
            let mut cand = items.clone();
            let end = (i + chunk).min(cand.len());
            cand.drain(i..end);
            if test(&cand) {
                *items = cand;
                changed = true;
            } else {
                i += chunk;
            }
        }
        if chunk == 1 && !changed {
            break;
        }
        if !changed {
            chunk = (chunk / 2).max(1);
        }
    }
}

fn reduce_case(mut case: Case, db: &str) -> Case {
    let (e, r, _) = replay(&case, db);
    let want = match case.class {
        Class::Tlp | Class::Norec | Class::PlanVariance | Class::WrongState => (case.class, String::new()),
        _ => match compare(&e, &r, case.ordered) {
            Some(x) => x,
            None => {
                eprintln!("reduce: the case doesn't fail any more");
                return case;
            }
        },
    };
    case.class = want.0;
    if !still_fails(&case, db, &want) {
        eprintln!("reduce: the case doesn't fail any more");
        return case;
    }
    let mut checks = 0;
    // 1. Setup statements.
    let mut setup = case.setup.clone();
    reduce_list(&mut setup, |cand| {
        checks += 1;
        let c = Case { setup: cand.to_vec(), ..case.clone() };
        still_fails(&c, db, &want)
    });
    case.setup = setup;
    eprintln!("reduce: {} setup statements left ({checks} checks)", case.setup.len());
    // 2. Rows of INSERTs.
    for i in 0..case.setup.len() {
        let Some((pre, mut rows, post)) = insert_rows(&case.setup[i]) else { continue };
        let base = case.clone();
        let mk = |rows: &[String]| format!("{pre} {}{}{post}", rows.join(", "), if post.is_empty() { "" } else { " " });
        reduce_list(&mut rows, |cand| {
            if cand.is_empty() {
                return false;
            }
            let mut c = base.clone();
            c.setup[i] = mk(cand);
            still_fails(&c, db, &want)
        });
        case.setup[i] = mk(&rows);
    }
    // 3. The statements' text, greedily, until nothing shrinks.
    loop {
        let mut improved = false;
        for target in (0..=case.setup.len()).rev() {
            loop {
                let cur = if target == case.setup.len() { case.stmt.clone() } else { case.setup[target].clone() };
                let mut hit = None;
                for cand in candidates(&cur) {
                    let mut c = case.clone();
                    if target == case.setup.len() {
                        c.stmt = cand.clone();
                    } else {
                        c.setup[target] = cand.clone();
                    }
                    if still_fails(&c, db, &want) {
                        hit = Some(c);
                        break;
                    }
                }
                match hit {
                    Some(c) => {
                        case = c;
                        improved = true;
                    }
                    None => break,
                }
            }
        }
        if !improved {
            break;
        }
    }
    // Statements the smaller text no longer needs.
    let mut setup = case.setup.clone();
    reduce_list(&mut setup, |cand| {
        let c = Case { setup: cand.to_vec(), ..case.clone() };
        still_fails(&c, db, &want)
    });
    case.setup = setup;
    case
}

#[test]
#[ignore]
fn reduce() {
    let path = std::env::var("DIFFTEST_CASE").expect("DIFFTEST_CASE");
    on_big_stack(move || {
        install();
        let src = std::fs::read_to_string(&path).unwrap();
        let case = Case::parse(&src);
        let db = format!("{}_reduce", env("DIFFTEST_DB", "difftest"));
        let min = reduce_case(case, &db);
        let (e, r, _) = replay(&min, &db);
        let out = Path::new(&path).with_extension("min.sql");
        std::fs::write(&out, min.text(Some(&e), Some(&r))).unwrap();
        println!("{}", min.text(Some(&e), Some(&r)));
        eprintln!("reduce: wrote {}", out.display());
        forget_kept();
    });
}
