//! PostgreSQL's isolation suite (`src/test/isolation`, REL_18_6) run through the engine and
//! through the reference Postgres 18.6, compared permutation by permutation and step by step.
//! It writes `tests/isolation/REPORT.md` and the first differing permutation of each spec to
//! `tests/isolation/hunks/`.
//!
//!     CARGO_BUILD_JOBS=2 cargo test -p pg-engine --test pg_isolation -- --ignored --nocapture
//!
//! How it works (mirrors `pg_isolation_regress --schedule=isolation_schedule`):
//!
//! - The suite is sparse-cloned into `PGISOLATION_DIR` (default `/tmp/pgisolation`) if missing.
//! - The engine (`roll-engine`, built by cargo for this test) is started on 127.0.0.1:55700 with
//!   a fresh data directory. The reference is the `roll-engine-ref` container at
//!   127.0.0.1:55499 (user `postgres`, password `pw`), reached over TCP like the goldens.
//! - Every spec of `isolation_schedule` runs in order, on both servers at once (a thread per
//!   server), each in a database made fresh for it the way pg_regress makes its database
//!   (template0, UTF8, locale C, lc_* / bytea_output / timezone_abbreviations settings), with
//!   pg_regress's environment as startup options (PGTZ, PGDATESTYLE, PGOPTIONS, PGAPPNAME).
//! - The spec is run by a port of `isolationtester.c`: a control connection and one connection
//!   per session; steps are sent with the simple query protocol and read the way libpq reads
//!   them (results parsed lazily, notices printed as they are parsed, notifications after a
//!   step completes); a step counts as waiting when, at one of the 10 ms polls while its result
//!   is incomplete, the control connection's prepared
//!   `pg_isolation_test_session_is_blocked($1, '{pids}')` says so; `(*)`, `(other)` and
//!   `(other notices n)` markers behave as isolationtester's. The output is isolationtester's
//!   own text (PQprint's aligned format, `step x: <sql> <waiting ...>`, `<... completed>`), so
//!   the reference's output is also checked against `expected/<spec>*.out`.
//! - Outputs are cut into permutations (`starting permutation:`) and each permutation into steps
//!   (a `step ...` line and what follows it); the engine's are compared with the reference's.
//!
//! Environment:
//! - `PGISOLATION_ONLY=a,b,c`: run only these specs. `PGISOLATION_SKIP=a,b`: leave these out.
//! - `PGISOLATION_EXTRA=1`: also run the engine's own specs, `tests/isolation/specs/*.spec`
//!   (concurrency semantics beyond the suite's: advisory locks, NOWAIT / SKIP LOCKED, lock
//!   queues, LISTEN / NOTIFY, sequences, serialization failures, DETACH ... FINALIZE), after
//!   the schedule; the reference's output is their expected one. (Use another
//!   `PGISOLATION_REPORT_DIR`: they aren't the suite's score.)
//! - `PGISOLATION_REPORT_ONLY=1`: don't run anything, rebuild the report from saved outputs.
//! - `PGISOLATION_REUSE_REFERENCE=1`: reuse a saved reference output when there is one (runs
//!   the engine only).
//! - `PGISOLATION_STEP_TIMEOUT=<secs>`: isolationtester's max_step_wait (default 60): a step is
//!   canceled after it, and the spec given up after twice it.
//! - `PGISOLATION_PORT` (55700), `PGISOLATION_REF` (`127.0.0.1:55499`), `PGISOLATION_PASSWORD`
//!   (`pw`), `PGISOLATION_ENGINE_BIN` (default: the one cargo built).
//! - `PGISOLATION_MAX_PREPARED`: the engine's max_prepared_transactions (default 0), for a
//!   reference started with `-c max_prepared_transactions=<n>` (the 2PC specs:
//!   prepared-transactions, prepared-transactions-cic, stats).
//! - `PGISOLATION_DB`: the database name (default `isolation_regression`, the one
//!   pg_isolation_regress uses; specs naming it get the name given here instead).
//! - `PGISOLATION_REPORT_DIR`: where `REPORT.md` and `hunks/` go (default `tests/isolation`).
//!
//! The database is dropped on both servers at the end, and so are roles the specs create that
//! a spec left behind on the reference.

use bytes::{Buf, BytesMut};
use fallible_iterator::FallibleIterator;
use postgres_protocol::authentication::sasl::{ChannelBinding, ScramSha256};
use postgres_protocol::message::backend::{ErrorFields, Message};
use postgres_protocol::message::frontend;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const GIT_TAG: &str = "REL_18_6";
const SPEC_DB: &str = "isolation_regression";

struct Config {
    root: PathBuf,
    src: PathBuf,
    out: PathBuf,
    port: u16,
    reference: String,
    password: String,
    db: String,
    step_wait: Duration,
    only: Option<BTreeSet<String>>,
    skip: BTreeSet<String>,
    report_only: bool,
    reuse_reference: bool,
    report_dir: PathBuf,
}

fn list(v: &str) -> BTreeSet<String> {
    v.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

impl Config {
    fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok();
        let root = PathBuf::from(env("PGISOLATION_DIR").unwrap_or_else(|| "/tmp/pgisolation".into()));
        Config {
            src: root.join("src-repo/src/test/isolation"),
            out: root.join("out"),
            root,
            port: env("PGISOLATION_PORT").and_then(|p| p.parse().ok()).unwrap_or(55700),
            reference: env("PGISOLATION_REF").unwrap_or_else(|| "127.0.0.1:55499".into()),
            password: env("PGISOLATION_PASSWORD").unwrap_or_else(|| "pw".into()),
            db: env("PGISOLATION_DB").unwrap_or_else(|| SPEC_DB.into()),
            step_wait: Duration::from_secs(
                env("PGISOLATION_STEP_TIMEOUT").and_then(|p| p.parse().ok()).unwrap_or(60),
            ),
            only: env("PGISOLATION_ONLY").map(|s| list(&s)),
            skip: env("PGISOLATION_SKIP").map(|s| list(&s)).unwrap_or_default(),
            report_only: env("PGISOLATION_REPORT_ONLY").is_some_and(|v| v == "1"),
            reuse_reference: env("PGISOLATION_REUSE_REFERENCE").is_some_and(|v| v == "1"),
            report_dir: env("PGISOLATION_REPORT_DIR").map_or_else(
                || Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/isolation"),
                PathBuf::from,
            ),
        }
    }
}

#[test]
#[ignore = "runs PostgreSQL's isolation suite against a reference container; slow"]
fn pg_isolation() {
    let cfg = Config::from_env();
    if !cfg.report_only {
        ensure_suite(&cfg);
    }
    let mut schedule = read_schedule(&cfg.src.join("isolation_schedule"));
    if let Some(only) = &cfg.only {
        // (Specs outside the schedule, such as installcheck-prepared-txns' 2PC ones, run when
        // named.)
        let extra: Vec<_> = only
            .iter()
            .filter(|t| !schedule.contains(t) && spec_path(&cfg, t).exists())
            .cloned()
            .collect();
        schedule.retain(|t| only.contains(t));
        schedule.extend(extra);
    }
    if std::env::var("PGISOLATION_EXTRA").is_ok_and(|v| v == "1") {
        let mut own: Vec<String> = std::fs::read_dir(extra_specs_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_suffix(".spec").map(str::to_string))
            .filter(|t| !schedule.contains(t))
            .collect();
        own.sort();
        schedule.extend(own);
    }
    schedule.retain(|t| !cfg.skip.contains(t));
    if !cfg.report_only {
        run_all(&cfg, &schedule);
    }
    let results: Vec<SpecResult> = schedule.iter().filter_map(|t| analyze(&cfg, t)).collect();
    write_report(&cfg, &results);
    let sum = |f: fn(&SpecResult) -> usize| results.iter().map(f).sum::<usize>();
    eprintln!(
        "pg_isolation: {}/{} specs, {}/{} permutations, {}/{} steps match; report in {}",
        results.iter().filter(|r| r.matches()).count(),
        results.len(),
        sum(|r| r.perms_matched),
        sum(|r| r.perms),
        sum(|r| r.steps_matched),
        sum(|r| r.steps),
        cfg.report_dir.join("REPORT.md").display()
    );
}

// ---------------------------------------------------------------------------------------------
// Fetching the suite

fn ensure_suite(cfg: &Config) {
    if cfg.src.join("isolation_schedule").exists() {
        return;
    }
    std::fs::create_dir_all(&cfg.root).unwrap();
    let repo = cfg.root.join("src-repo");
    let ok = sh(Command::new("git").args([
        "clone",
        "-q",
        "--depth",
        "1",
        "--filter=blob:none",
        "--sparse",
        "--branch",
        GIT_TAG,
        "https://github.com/postgres/postgres.git",
        repo.to_str().unwrap(),
    ])) && sh(Command::new("git")
        .current_dir(&repo)
        .args(["sparse-checkout", "set", "src/test/isolation"]));
    assert!(ok, "could not fetch the isolation suite");
}

fn sh(cmd: &mut Command) -> bool {
    cmd.status().is_ok_and(|s| s.success())
}

fn read_schedule(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("isolation_schedule")
        .lines()
        .filter_map(|l| l.strip_prefix("test:"))
        .flat_map(|l| l.split_whitespace().map(str::to_string).collect::<Vec<_>>())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Spec files (specscanner.l / specparse.y)

#[derive(Debug, Clone)]
struct Step {
    name: String,
    sql: String,
    session: usize,
}

#[derive(Debug, Default)]
struct Session {
    name: String,
    setup: Option<String>,
    steps: Vec<usize>,
    teardown: Option<String>,
}

#[derive(Debug, Clone)]
enum Blocker {
    /// `*`
    Once,
    /// `other`: not completed while that step is active.
    Step(String),
    /// `other notices n`
    Notices(String, usize),
}

#[derive(Debug, Clone)]
struct PStep {
    name: String,
    step: usize,
    blockers: Vec<Blocker>,
}

#[derive(Debug, Default)]
struct Spec {
    setup: Vec<String>,
    teardown: Option<String>,
    sessions: Vec<Session>,
    steps: Vec<Step>,
    permutations: Vec<Vec<PStep>>,
    /// What isolationtester writes to stderr while checking the spec ("unused step name").
    warnings: String,
}

#[derive(Debug, PartialEq)]
enum Tok {
    Ident(String),
    Sql(String),
    Int(usize),
    Char(char),
    Kw(&'static str),
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    let ident_start = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80;
    let ident_cont = |c: u8| ident_start(c) || c.is_ascii_digit() || c == b'$';
    let space = |c: u8| matches!(c, b' ' | b'\t' | b'\r' | 0x0c);
    while i < b.len() {
        let c = b[i];
        if c == b'\n' || space(c) {
            i += 1;
        } else if c == b'#' {
            while i < b.len() && b[i] != b'\n' && b[i] != b'\r' {
                i += 1;
            }
        } else if ident_start(c) {
            let s = i;
            while i < b.len() && ident_cont(b[i]) {
                i += 1;
            }
            let w = &src[s..i];
            out.push(
                match ["notices", "permutation", "session", "setup", "step", "teardown"]
                    .into_iter()
                    .find(|k| *k == w)
                {
                    Some(k) => Tok::Kw(k),
                    None => Tok::Ident(w.to_string()),
                },
            );
        } else if c == b'"' {
            let mut v = Vec::new();
            i += 1;
            loop {
                match b.get(i) {
                    None => return Err("unterminated quoted identifier".into()),
                    Some(b'\n') => return Err("unexpected newline in quoted identifier".into()),
                    Some(b'"') if b.get(i + 1) == Some(&b'"') => {
                        v.push(b'"');
                        i += 2;
                    }
                    Some(b'"') => {
                        i += 1;
                        break;
                    }
                    Some(&x) => {
                        v.push(x);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Ident(String::from_utf8_lossy(&v).into_owned()));
        } else if c == b'{' {
            i += 1;
            while i < b.len() && space(b[i]) {
                i += 1;
            }
            let s = i;
            let Some(end) = src[s..].find('}') else {
                return Err("unterminated sql block".into());
            };
            let mut e = s + end;
            i = e + 1;
            while e > s && space(b[e - 1]) {
                e -= 1;
            }
            out.push(Tok::Sql(src[s..e].to_string()));
        } else if c.is_ascii_digit() {
            let s = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            out.push(Tok::Int(src[s..i].parse().unwrap()));
        } else if matches!(c, b',' | b'(' | b')' | b'*') {
            out.push(Tok::Char(c as char));
            i += 1;
        } else {
            return Err(format!("unexpected character \"{}\"", c as char));
        }
    }
    Ok(out)
}

fn parse_spec(src: &str) -> Result<Spec, String> {
    let toks = lex(src)?;
    let mut p = 0;
    let mut spec = Spec::default();
    let peek = |p: usize| toks.get(p);
    let sql = |p: &mut usize| -> Result<String, String> {
        match toks.get(*p) {
            Some(Tok::Sql(s)) => {
                *p += 1;
                Ok(s.clone())
            }
            t => Err(format!("expected an sql block, found {t:?}")),
        }
    };
    let ident = |p: &mut usize| -> Result<String, String> {
        match toks.get(*p) {
            Some(Tok::Ident(s)) => {
                *p += 1;
                Ok(s.clone())
            }
            t => Err(format!("expected an identifier, found {t:?}")),
        }
    };
    while peek(p) == Some(&Tok::Kw("setup")) {
        p += 1;
        spec.setup.push(sql(&mut p)?);
    }
    if peek(p) == Some(&Tok::Kw("teardown")) {
        p += 1;
        spec.teardown = Some(sql(&mut p)?);
    }
    while peek(p) == Some(&Tok::Kw("session")) {
        p += 1;
        let mut s = Session {
            name: ident(&mut p)?,
            ..Default::default()
        };
        if peek(p) == Some(&Tok::Kw("setup")) {
            p += 1;
            s.setup = Some(sql(&mut p)?);
        }
        while peek(p) == Some(&Tok::Kw("step")) {
            p += 1;
            let name = ident(&mut p)?;
            let body = sql(&mut p)?;
            s.steps.push(spec.steps.len());
            spec.steps.push(Step {
                name,
                sql: body,
                session: spec.sessions.len(),
            });
        }
        if s.steps.is_empty() {
            return Err(format!("session {} has no steps", s.name));
        }
        if peek(p) == Some(&Tok::Kw("teardown")) {
            p += 1;
            s.teardown = Some(sql(&mut p)?);
        }
        spec.sessions.push(s);
    }
    if spec.sessions.is_empty() {
        return Err("no sessions".into());
    }
    let find = |steps: &[Step], n: &str| steps.iter().position(|s| s.name == n);
    while peek(p) == Some(&Tok::Kw("permutation")) {
        p += 1;
        let mut perm = Vec::new();
        while let Some(Tok::Ident(name)) = peek(p) {
            p += 1;
            let step = find(&spec.steps, name)
                .ok_or_else(|| format!("undefined step \"{name}\" specified in permutation"))?;
            let mut blockers = Vec::new();
            if peek(p) == Some(&Tok::Char('(')) {
                p += 1;
                loop {
                    match peek(p) {
                        Some(Tok::Char('*')) => {
                            p += 1;
                            blockers.push(Blocker::Once);
                        }
                        Some(Tok::Ident(other)) => {
                            p += 1;
                            if peek(p) == Some(&Tok::Kw("notices")) {
                                p += 1;
                                let Some(Tok::Int(n)) = peek(p) else {
                                    return Err("expected a notice count".into());
                                };
                                p += 1;
                                blockers.push(Blocker::Notices(other.clone(), *n));
                            } else {
                                blockers.push(Blocker::Step(other.clone()));
                            }
                        }
                        t => return Err(format!("bad blocker {t:?}")),
                    }
                    match peek(p) {
                        Some(Tok::Char(',')) => p += 1,
                        Some(Tok::Char(')')) => {
                            p += 1;
                            break;
                        }
                        t => return Err(format!("bad blocker list at {t:?}")),
                    }
                }
            }
            perm.push(PStep {
                name: name.clone(),
                step,
                blockers,
            });
        }
        if perm.is_empty() {
            return Err("empty permutation".into());
        }
        spec.permutations.push(perm);
    }
    if p != toks.len() {
        return Err(format!("syntax error at {:?}", toks[p]));
    }
    // check_testspec: step names unique; blockers name a step of the permutation, of another
    // session; unused steps reported.
    let mut names: Vec<&str> = spec.steps.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    if let Some(w) = names.windows(2).find(|w| w[0] == w[1]) {
        return Err(format!("duplicate step name: {}", w[0]));
    }
    for perm in &spec.permutations {
        for ps in perm {
            for b in &ps.blockers {
                let (Blocker::Step(o) | Blocker::Notices(o, _)) = b else {
                    continue;
                };
                let Some(other) = perm.iter().find(|x| &x.name == o) else {
                    return Err(format!(
                        "undefined blocking step \"{o}\" referenced in permutation step \"{}\"",
                        ps.name
                    ));
                };
                if spec.steps[other.step].session == spec.steps[ps.step].session {
                    return Err(format!(
                        "permutation step \"{}\" cannot block on its own session",
                        ps.name
                    ));
                }
            }
        }
    }
    if !spec.permutations.is_empty() {
        let mut sorted: Vec<&Step> = spec.steps.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for s in sorted {
            if !spec.permutations.iter().flatten().any(|p| p.name == s.name) {
                let _ = writeln!(spec.warnings, "unused step name: {}", s.name);
            }
        }
    }
    Ok(spec)
}

// ---------------------------------------------------------------------------------------------
// A libpq-like connection: asynchronous simple queries, results parsed lazily the way
// pqParseInput3 parses them.

#[derive(Debug)]
enum PgResult {
    Command,
    Empty,
    Tuples {
        cols: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
    },
    Error {
        severity: String,
        primary: String,
        /// The whole message as PQerrorMessage / a notice processor gets it.
        full: String,
    },
    /// An error libpq made itself (no fields), e.g. the connection lost.
    Local(String),
}

const CONNECTION_LOST: &str = "server closed the connection unexpectedly\n\tThis probably means the server terminated abnormally\n\tbefore or while processing the request.\n";

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Async {
    Idle,
    Busy,
    Ready,
}

struct Conn {
    stream: TcpStream,
    buf: BytesMut,
    addr: String,
    pid: i32,
    secret: i32,
    state: Async,
    building: Option<PgResult>,
    ready: Option<PgResult>,
    /// Formatted notices parsed and not yet printed.
    notices: Vec<String>,
    notifies: Vec<(i32, String, String)>,
    /// The query being run, for error cursor positions.
    query: String,
    eof: bool,
    /// How long a blocking read for a result may wait (setup, teardown, a step's later
    /// statements); past it the spec is given up.
    timeout: Option<Duration>,
}

#[derive(Default)]
struct ErrFields {
    severity: Option<String>,
    primary: Option<String>,
    detail: Option<String>,
    hint: Option<String>,
    position: Option<usize>,
    internal_position: Option<usize>,
    internal_query: Option<String>,
    context: Option<String>,
}

fn err_fields(mut f: ErrorFields<'_>) -> ErrFields {
    let mut out = ErrFields::default();
    while let Ok(Some(x)) = f.next() {
        let v = String::from_utf8_lossy(x.value_bytes()).into_owned();
        match x.type_() {
            b'S' => out.severity = Some(v),
            b'V' if out.severity.is_none() => out.severity = Some(v),
            b'M' => out.primary = Some(v),
            b'D' => out.detail = Some(v),
            b'H' => out.hint = Some(v),
            b'P' => out.position = v.parse().ok(),
            b'p' => out.internal_position = v.parse().ok(),
            b'q' => out.internal_query = Some(v),
            b'W' => out.context = Some(v),
            _ => {}
        }
    }
    out
}

/// pqBuildErrorMessage3 at the default verbosity, CONTEXT for errors only.
fn build_message(f: &ErrFields, query: &str, is_error: bool) -> String {
    let mut m = String::new();
    if let Some(s) = &f.severity {
        let _ = write!(m, "{s}:  ");
    }
    if let Some(p) = &f.primary {
        m.push_str(p);
    }
    let mut cursor: Option<(String, usize)> = None;
    if let Some(pos) = f.position {
        cursor = Some((query.to_string(), pos));
    } else if let Some(pos) = f.internal_position {
        match &f.internal_query {
            Some(q) => cursor = Some((q.clone(), pos)),
            None => {
                let _ = write!(m, " at character {pos}");
            }
        }
    }
    m.push('\n');
    if let Some((q, pos)) = cursor
        && pos > 0
    {
        report_error_position(&mut m, &q, pos);
    }
    if let Some(d) = &f.detail {
        let _ = writeln!(m, "DETAIL:  {d}");
    }
    if let Some(h) = &f.hint {
        let _ = writeln!(m, "HINT:  {h}");
    }
    if let Some(q) = &f.internal_query {
        let _ = writeln!(m, "QUERY:  {q}");
    }
    if is_error && let Some(c) = &f.context {
        let _ = writeln!(m, "CONTEXT:  {c}");
    }
    m
}

/// libpq's reportErrorPosition (single-byte widths; the specs' queries are ASCII).
fn report_error_position(m: &mut String, query: &str, loc: usize) {
    const DISPLAY_SIZE: usize = 60;
    const MIN_RIGHT_CUT: usize = 10;
    let chars: Vec<char> = query.chars().collect();
    let loc = loc - 1;
    if loc > chars.len() {
        return;
    }
    // Lines, with tabs as spaces; the location's line.
    let mut line_no = 1;
    let mut line_start = 0;
    let mut i = 0;
    let mut text: Vec<char> = chars.iter().map(|&c| if c == '\t' { ' ' } else { c }).collect();
    while i < loc {
        if chars[i] == '\n' {
            line_no += 1;
            line_start = i + 1;
        } else if chars[i] == '\r' {
            if chars.get(i + 1) != Some(&'\n') {
                line_no += 1;
                line_start = i + 1;
            } else {
                line_no += 1;
                line_start = i + 2;
                i += 1;
            }
        }
        i += 1;
    }
    let mut line_end = line_start;
    while line_end < text.len() && text[line_end] != '\n' && text[line_end] != '\r' {
        line_end += 1;
    }
    let mut cut_l = line_start;
    let mut cut_r = line_end;
    let mut scroffset = loc - line_start;
    let mut beg_trunc = false;
    let mut end_trunc = false;
    if line_end - line_start > DISPLAY_SIZE {
        if scroffset + MIN_RIGHT_CUT > DISPLAY_SIZE {
            let skip = scroffset + MIN_RIGHT_CUT - DISPLAY_SIZE;
            cut_l += skip.min(line_end - line_start);
            scroffset -= skip.min(scroffset);
            beg_trunc = true;
        }
        if cut_r - cut_l > DISPLAY_SIZE {
            cut_r = cut_l + DISPLAY_SIZE;
            end_trunc = true;
        }
    }
    let prefix = format!("LINE {line_no}: ");
    let mut s = prefix.clone();
    if beg_trunc {
        s.push_str("...");
        scroffset += 3;
    }
    s.extend(text.drain(cut_l..cut_r));
    if end_trunc {
        s.push_str("...");
    }
    s.push('\n');
    s.push_str(&" ".repeat(prefix.len() + scroffset));
    s.push_str("^\n");
    m.push_str(&s);
}

struct ConnOpts<'a> {
    addr: &'a str,
    db: &'a str,
    password: &'a str,
    appname: &'a str,
}

impl Conn {
    fn connect(o: &ConnOpts<'_>) -> std::io::Result<Conn> {
        let stream = TcpStream::connect(o.addr)?;
        stream.set_nodelay(true)?;
        let mut c = Conn {
            stream,
            buf: BytesMut::new(),
            addr: o.addr.to_string(),
            pid: 0,
            secret: 0,
            state: Async::Idle,
            building: None,
            ready: None,
            notices: Vec::new(),
            notifies: Vec::new(),
            query: String::new(),
            eof: false,
            timeout: None,
        };
        let mut out = BytesMut::new();
        // pg_regress's environment, as libpq sends it (PGDATESTYLE, PGTZ, PGOPTIONS, PGAPPNAME).
        frontend::startup_message(
            [
                ("user", "postgres"),
                ("database", o.db),
                ("application_name", o.appname),
                ("datestyle", "Postgres, MDY"),
                ("timezone", "America/Los_Angeles"),
                ("options", "-c intervalstyle=postgres_verbose"),
            ],
            &mut out,
        )?;
        c.stream.write_all(&out)?;
        let mut scram: Option<ScramSha256> = None;
        loop {
            let m = c.read_message()?;
            let mut out = BytesMut::new();
            match m {
                Message::AuthenticationOk => {}
                Message::AuthenticationCleartextPassword => {
                    frontend::password_message(o.password.as_bytes(), &mut out)?;
                }
                Message::AuthenticationMd5Password(b) => {
                    let h = postgres_protocol::authentication::md5_hash(
                        b"postgres",
                        o.password.as_bytes(),
                        b.salt(),
                    );
                    frontend::password_message(h.as_bytes(), &mut out)?;
                }
                Message::AuthenticationSasl(_) => {
                    let s = ScramSha256::new(o.password.as_bytes(), ChannelBinding::unsupported());
                    frontend::sasl_initial_response("SCRAM-SHA-256", s.message(), &mut out)?;
                    scram = Some(s);
                }
                Message::AuthenticationSaslContinue(b) => {
                    let s = scram.as_mut().unwrap();
                    s.update(b.data())?;
                    frontend::sasl_response(s.message(), &mut out)?;
                }
                Message::AuthenticationSaslFinal(b) => scram.as_mut().unwrap().finish(b.data())?,
                Message::BackendKeyData(k) => {
                    c.pid = k.process_id();
                    c.secret = k.secret_key();
                }
                Message::ErrorResponse(e) => {
                    let f = err_fields(e.fields());
                    return Err(std::io::Error::other(build_message(&f, "", true)));
                }
                Message::ReadyForQuery(_) => break,
                _ => {}
            }
            if !out.is_empty() {
                c.stream.write_all(&out)?;
            }
        }
        Ok(c)
    }

    /// A blocking read of one whole message (connection startup only).
    fn read_message(&mut self) -> std::io::Result<Message> {
        loop {
            if let Some(m) = Message::parse(&mut self.buf)? {
                return Ok(m);
            }
            self.fill(None)?;
        }
    }

    /// Read what the socket has: waits up to `wait` (None: until something arrives).
    /// Ok(false): nothing arrived in time.
    fn fill(&mut self, wait: Option<Duration>) -> std::io::Result<bool> {
        if self.eof {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "connection closed"));
        }
        self.stream.set_read_timeout(wait)?;
        let mut tmp = [0u8; 65536];
        match self.stream.read(&mut tmp) {
            Ok(0) => {
                self.eof = true;
                Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "connection closed"))
            }
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(true)
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// PQconsumeInput: whatever has arrived, without waiting.
    fn consume_input(&mut self) -> std::io::Result<()> {
        self.stream.set_nonblocking(true)?;
        let r = loop {
            match self.fill(None) {
                Ok(true) => continue,
                Ok(false) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        self.stream.set_nonblocking(false)?;
        r
    }

    fn send_query(&mut self, sql: &str) -> std::io::Result<()> {
        if self.eof {
            return Err(std::io::Error::other("no connection to the server"));
        }
        let mut out = BytesMut::new();
        frontend::query(sql, &mut out)?;
        self.stream.write_all(&out)?;
        self.state = Async::Busy;
        self.building = None;
        self.ready = None;
        self.query = sql.to_string();
        Ok(())
    }

    /// pqParseInput3: notices and notifications always; other messages only while busy, up to
    /// the end of a result.
    fn parse_input(&mut self) -> std::io::Result<()> {
        loop {
            if self.buf.len() < 5 {
                return Ok(());
            }
            let len = u32::from_be_bytes(self.buf[1..5].try_into().unwrap()) as usize;
            if self.buf.len() < 1 + len {
                return Ok(());
            }
            let id = self.buf[0];
            if id != b'N' && id != b'A' && self.state == Async::Ready {
                return Ok(());
            }
            let Some(m) = Message::parse(&mut self.buf)? else {
                return Ok(());
            };
            match m {
                Message::NoticeResponse(n) => {
                    let f = err_fields(n.fields());
                    self.notices.push(build_message(&f, &self.query, false));
                }
                Message::NotificationResponse(n) => self.notifies.push((
                    n.process_id(),
                    n.channel()?.to_string(),
                    n.message()?.to_string(),
                )),
                _ if self.state == Async::Idle => {}
                Message::RowDescription(d) => {
                    let mut f = d.fields();
                    let mut cols = Vec::new();
                    while let Some(x) = f.next()? {
                        cols.push(x.name().to_string());
                    }
                    self.building = Some(PgResult::Tuples { cols, rows: Vec::new() });
                }
                Message::DataRow(r) => {
                    let buf = r.buffer();
                    let mut ranges = r.ranges();
                    let mut row = Vec::new();
                    while let Some(rg) = ranges.next()? {
                        row.push(rg.map(|rg| String::from_utf8_lossy(&buf[rg]).into_owned()));
                    }
                    if let Some(PgResult::Tuples { rows, .. }) = &mut self.building {
                        rows.push(row);
                    }
                }
                Message::CommandComplete(_) => {
                    self.ready = Some(self.building.take().unwrap_or(PgResult::Command));
                    self.state = Async::Ready;
                }
                Message::EmptyQueryResponse => {
                    self.ready = Some(PgResult::Empty);
                    self.state = Async::Ready;
                }
                Message::ErrorResponse(e) => {
                    let f = err_fields(e.fields());
                    self.building = None;
                    self.ready = Some(PgResult::Error {
                        severity: f.severity.clone().unwrap_or_default(),
                        primary: f.primary.clone().unwrap_or_default(),
                        full: build_message(&f, &self.query, true),
                    });
                    self.state = Async::Ready;
                }
                Message::ReadyForQuery(_) => self.state = Async::Idle,
                Message::CopyInResponse(_) => {
                    let mut out = BytesMut::new();
                    frontend::copy_fail("COPY FROM STDIN is not supported here", &mut out)?;
                    self.stream.write_all(&out)?;
                }
                Message::CopyData(mut d) => {
                    // COPY TO STDOUT: isolationtester would print "unexpected result status".
                    let _ = d.data().remaining();
                    let _ = &mut d;
                }
                _ => {}
            }
        }
    }

    /// PQisBusy.
    fn is_busy(&mut self) -> std::io::Result<bool> {
        self.parse_input()?;
        Ok(self.state == Async::Busy)
    }

    /// PQgetResult: waits for the next result; None once the query is done.
    fn get_result(&mut self) -> std::io::Result<Option<PgResult>> {
        let mut started: Option<Instant> = None;
        loop {
            self.parse_input()?;
            match self.state {
                Async::Ready => {
                    self.state = Async::Busy;
                    return Ok(self.ready.take());
                }
                Async::Idle => return Ok(None),
                Async::Busy => {
                    if self.eof {
                        self.state = Async::Idle;
                        return Ok(None);
                    }
                    let wait = self.timeout.map(|t| {
                        let deadline = *started.get_or_insert_with(Instant::now) + t;
                        deadline.saturating_duration_since(Instant::now())
                    });
                    if wait.is_some_and(|w| w.is_zero()) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "a query blocked past the step timeout",
                        ));
                    }
                    if let Err(e) = self.fill(wait) {
                        if e.kind() != std::io::ErrorKind::UnexpectedEof {
                            return Err(e);
                        }
                        // pqReadData's report of the lost connection, then no more results.
                        self.state = Async::Ready;
                        self.ready = Some(PgResult::Local(CONNECTION_LOST.into()));
                    }
                }
            }
        }
    }

    /// PQexec: every result read; the last one returned.
    fn exec(&mut self, sql: &str) -> std::io::Result<Option<PgResult>> {
        if self.eof {
            return Ok(Some(PgResult::Local("no connection to the server\n".into())));
        }
        self.send_query(sql)?;
        let mut last = None;
        while let Some(r) = self.get_result()? {
            last = Some(r);
        }
        Ok(last)
    }

    /// PQprepare.
    fn prepare(&mut self, name: &str, sql: &str) -> std::io::Result<Result<(), String>> {
        let mut out = BytesMut::new();
        frontend::parse(name, sql, std::iter::empty(), &mut out)?;
        frontend::sync(&mut out);
        self.stream.write_all(&out)?;
        self.extended_reply()
    }

    /// PQexecPrepared with one text parameter, one value returned.
    fn exec_prepared(&mut self, name: &str, param: &str) -> std::io::Result<Result<String, String>> {
        let mut body = Vec::new();
        body.push(0); // unnamed portal
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&(param.len() as i32).to_be_bytes());
        body.extend_from_slice(param.as_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        let mut msg = vec![b'B'];
        msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        msg.extend_from_slice(&body);
        let mut out = BytesMut::from(&msg[..]);
        frontend::execute("", 0, &mut out)?;
        frontend::sync(&mut out);
        self.stream.write_all(&out)?;
        self.state = Async::Busy;
        let mut value = None;
        let mut error = None;
        loop {
            let m = self.read_message()?;
            match m {
                Message::DataRow(r) => {
                    let buf = r.buffer();
                    let mut ranges = r.ranges();
                    if let Some(Some(rg)) = ranges.next()? {
                        value = Some(String::from_utf8_lossy(&buf[rg]).into_owned());
                    }
                }
                Message::ErrorResponse(e) => {
                    let f = err_fields(e.fields());
                    error = Some(build_message(&f, "", true));
                }
                Message::ReadyForQuery(_) => break,
                _ => {}
            }
        }
        self.state = Async::Idle;
        Ok(match (error, value) {
            (Some(e), _) => Err(e),
            (None, Some(v)) => Ok(v),
            (None, None) => Err("no row returned\n".into()),
        })
    }

    fn extended_reply(&mut self) -> std::io::Result<Result<(), String>> {
        let mut error = None;
        loop {
            match self.read_message()? {
                Message::ErrorResponse(e) => {
                    let f = err_fields(e.fields());
                    error = Some(build_message(&f, "", true));
                }
                Message::ReadyForQuery(_) => break,
                _ => {}
            }
        }
        Ok(error.map_or(Ok(()), Err))
    }

    /// PQcancel.
    fn cancel(&self) -> bool {
        let Ok(mut s) = TcpStream::connect(&self.addr) else {
            return false;
        };
        let mut m = Vec::new();
        m.extend_from_slice(&16i32.to_be_bytes());
        m.extend_from_slice(&80877102i32.to_be_bytes());
        m.extend_from_slice(&self.pid.to_be_bytes());
        m.extend_from_slice(&self.secret.to_be_bytes());
        s.write_all(&m).is_ok() && {
            let mut b = [0u8; 1];
            let _ = s.read(&mut b);
            true
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let mut out = BytesMut::new();
        frontend::terminate(&mut out);
        let _ = self.stream.write_all(&out);
    }
}

/// PQprint with header, align and fieldSep "|".
fn print_result(out: &mut String, cols: &[String], rows: &[Vec<Option<String>>]) {
    let n = cols.len();
    if n == 0 {
        return;
    }
    let mut max: Vec<usize> = cols.iter().map(|c| c.len()).collect();
    let mut not_num = vec![false; n];
    for row in rows {
        for (j, v) in row.iter().enumerate() {
            let Some(v) = v.as_deref().filter(|v| !v.is_empty()) else {
                continue;
            };
            if !not_num[j] {
                let mut last = b'0';
                for &ch in v.as_bytes() {
                    last = ch;
                    if !(ch.is_ascii_digit() || matches!(ch, b'.' | b'E' | b'e' | b' ' | b'-')) {
                        not_num[j] = true;
                        break;
                    }
                }
                let first = v.as_bytes()[0];
                if first == b'E' || first == b'e' || !last.is_ascii_digit() {
                    not_num[j] = true;
                }
            }
            max[j] = max[j].max(v.len());
        }
    }
    let pad = |s: &str, w: usize, left: bool| {
        let fill = " ".repeat(w.saturating_sub(s.len()));
        if left { format!("{s}{fill}") } else { format!("{fill}{s}") }
    };
    let header: Vec<String> = (0..n).map(|j| pad(&cols[j], max[j], not_num[j])).collect();
    out.push_str(&header.join("|"));
    out.push('\n');
    let border: Vec<String> = (0..n).map(|j| "-".repeat(max[j])).collect();
    out.push_str(&border.join("+"));
    out.push('\n');
    for row in rows {
        let cells: Vec<String> = (0..n)
            .map(|j| {
                let v = row.get(j).cloned().flatten().unwrap_or_default();
                pad(&v, max[j], not_num[j])
            })
            .collect();
        out.push_str(&cells.join("|"));
        out.push('\n');
    }
    let _ = write!(out, "({} row{})\n\n", rows.len(), if rows.len() == 1 { "" } else { "s" });
}

// ---------------------------------------------------------------------------------------------
// isolationtester

const STEP_NONBLOCK: u8 = 1;
const STEP_RETRY: u8 = 2;
const PREP_WAITING: &str = "isolationtester_waiting";

struct IsoConn {
    conn: Conn,
    name: String,
    active: Option<usize>,
    total_notices: usize,
}

/// One permutation step as run: its spec step, blockers and notice targets.
struct RunStep {
    pstep: PStep,
    targets: Vec<usize>,
}

struct Tester<'a> {
    spec: &'a Spec,
    conns: Vec<IsoConn>,
    out: String,
    any_new_notice: bool,
    max_wait: Duration,
    steps: Vec<RunStep>,
}

/// Why a spec stopped early (isolationtester exits).
struct Fatal(String);

impl From<std::io::Error> for Fatal {
    fn from(e: std::io::Error) -> Self {
        Fatal(format!("connection failed: {e}\n"))
    }
}

impl<'a> Tester<'a> {
    fn flush_notices(&mut self, c: usize) {
        let notices = std::mem::take(&mut self.conns[c].conn.notices);
        if c == 0 {
            return;
        }
        for n in notices {
            let _ = write!(self.out, "{}: {n}", self.conns[c].name);
            self.conns[c].total_notices += 1;
            self.any_new_notice = true;
        }
    }

    fn print_res(&mut self, r: &PgResult) {
        if let PgResult::Tuples { cols, rows } = r {
            print_result(&mut self.out, cols, rows);
        }
    }

    fn run_spec(&mut self) -> Result<(), Fatal> {
        if self.spec.permutations.is_empty() {
            let mut piles = vec![0; self.spec.sessions.len()];
            let mut chosen = Vec::new();
            self.all_permutations(&mut piles, &mut chosen)
        } else {
            for p in &self.spec.permutations {
                self.run_permutation(p.clone())?;
            }
            Ok(())
        }
    }

    fn all_permutations(&mut self, piles: &mut [usize], chosen: &mut Vec<PStep>) -> Result<(), Fatal> {
        let mut found = false;
        for i in 0..self.spec.sessions.len() {
            if piles[i] < self.spec.sessions[i].steps.len() {
                let step = self.spec.sessions[i].steps[piles[i]];
                chosen.push(PStep {
                    name: self.spec.steps[step].name.clone(),
                    step,
                    blockers: Vec::new(),
                });
                piles[i] += 1;
                self.all_permutations(piles, chosen)?;
                piles[i] -= 1;
                chosen.pop();
                found = true;
            }
        }
        if !found {
            self.run_permutation(chosen.clone())?;
        }
        Ok(())
    }

    fn session_conn(&self, step: usize) -> usize {
        1 + self.spec.steps[step].session
    }

    fn run_permutation(&mut self, perm: Vec<PStep>) -> Result<(), Fatal> {
        let spec = self.spec;
        self.out.push_str("\nstarting permutation:");
        for p in &perm {
            let _ = write!(self.out, " {}", p.name);
        }
        self.out.push('\n');
        for sql in &spec.setup {
            let r = self.conns[0].conn.exec(sql)?;
            self.flush_notices(0);
            match r {
                Some(PgResult::Error { full, .. }) => {
                    return Err(Fatal(format!("setup failed: {full}")));
                }
                Some(r) => self.print_res(&r),
                None => {}
            }
        }
        for i in 0..spec.sessions.len() {
            if let Some(sql) = &spec.sessions[i].setup {
                let r = self.conns[i + 1].conn.exec(sql)?;
                self.flush_notices(i + 1);
                match r {
                    Some(PgResult::Error { full, .. }) => {
                        return Err(Fatal(format!(
                            "setup of session {} failed: {full}",
                            self.conns[i + 1].name
                        )));
                    }
                    Some(r) => self.print_res(&r),
                    None => {}
                }
            }
        }
        self.steps = perm
            .into_iter()
            .map(|pstep| RunStep {
                targets: vec![0; pstep.blockers.len()],
                pstep,
            })
            .collect();
        let mut waiting: Vec<usize> = Vec::new();
        for i in 0..self.steps.len() {
            let c = self.session_conn(self.steps[i].pstep.step);
            if self.conns[c].active.is_some() {
                let start = Instant::now();
                while let Some(old) = self.conns[c].active {
                    if !self.try_complete_step(old, STEP_RETRY)? {
                        let w = waiting.iter().position(|&x| x == old).expect("waiting step");
                        waiting.remove(w);
                    }
                    self.try_complete_steps(&mut waiting, STEP_NONBLOCK | STEP_RETRY)?;
                    if let Some(a) = self.conns[c].active
                        && start.elapsed() > 2 * self.max_wait
                    {
                        let mut msg = format!(
                            "step {} timed out after {} seconds\nactive steps are:",
                            self.steps[a].pstep.name,
                            start.elapsed().as_secs()
                        );
                        for k in 1..self.conns.len() {
                            if let Some(a) = self.conns[k].active {
                                let _ = write!(msg, " {}", self.steps[a].pstep.name);
                            }
                        }
                        msg.push('\n');
                        return Err(Fatal(msg));
                    }
                }
            }
            let sql = spec.steps[self.steps[i].pstep.step].sql.clone();
            if let Err(e) = self.conns[c].conn.send_query(&sql) {
                return Err(Fatal(format!(
                    "failed to send query for step {}: {e}\n",
                    self.steps[i].pstep.name
                )));
            }
            self.conns[c].active = Some(i);
            for j in 0..self.steps[i].pstep.blockers.len() {
                if let Blocker::Notices(other, n) = &self.steps[i].pstep.blockers[j] {
                    let other = spec.steps.iter().position(|s| &s.name == other).unwrap();
                    let oc = self.session_conn(other);
                    self.steps[i].targets[j] = n + self.conns[oc].total_notices;
                }
            }
            let mustwait = self.try_complete_step(i, STEP_NONBLOCK)?;
            self.try_complete_steps(&mut waiting, STEP_NONBLOCK | STEP_RETRY)?;
            if mustwait {
                waiting.push(i);
            }
        }
        self.try_complete_steps(&mut waiting, STEP_RETRY)?;
        if !waiting.is_empty() {
            return Err(Fatal(
                "failed to complete permutation due to mutually-blocking steps\n".into(),
            ));
        }
        for i in 0..spec.sessions.len() {
            if let Some(sql) = &spec.sessions[i].teardown {
                let r = self.conns[i + 1].conn.exec(sql)?;
                self.flush_notices(i + 1);
                match r {
                    Some(PgResult::Error { full, .. }) => {
                        let _ = write!(
                            self.out,
                            "teardown of session {} failed: {full}",
                            self.conns[i + 1].name
                        );
                    }
                    Some(r) => self.print_res(&r),
                    None => {}
                }
            }
        }
        if let Some(sql) = &spec.teardown {
            let r = self.conns[0].conn.exec(sql)?;
            self.flush_notices(0);
            match r {
                Some(PgResult::Error { full, .. }) => {
                    let _ = write!(self.out, "teardown failed: {full}");
                }
                Some(r) => self.print_res(&r),
                None => {}
            }
        }
        Ok(())
    }

    fn try_complete_steps(&mut self, waiting: &mut Vec<usize>, flags: u8) -> Result<(), Fatal> {
        loop {
            self.any_new_notice = false;
            let old = waiting.len();
            let mut have_blocker = false;
            let mut w = 0;
            while w < waiting.len() {
                if self.try_complete_step(waiting[w], flags)? {
                    if !self.steps[waiting[w]].pstep.blockers.is_empty() {
                        have_blocker = true;
                    }
                    w += 1;
                } else {
                    waiting.remove(w);
                }
            }
            if !(have_blocker && (waiting.len() < old || self.any_new_notice)) {
                return Ok(());
            }
        }
    }

    fn step_has_blocker(&self, i: usize) -> bool {
        let rs = &self.steps[i];
        for (j, b) in rs.pstep.blockers.iter().enumerate() {
            match b {
                Blocker::Once => {}
                Blocker::Step(other) => {
                    let other = self.spec.steps.iter().position(|s| &s.name == other).unwrap();
                    let c = self.session_conn(other);
                    if self.conns[c].active.is_some_and(|a| self.steps[a].pstep.step == other) {
                        return true;
                    }
                }
                Blocker::Notices(other, _) => {
                    let other = self.spec.steps.iter().position(|s| &s.name == other).unwrap();
                    let c = self.session_conn(other);
                    if self.conns[c].total_notices < rs.targets[j] {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// True if the step did not complete.
    fn try_complete_step(&mut self, i: usize, flags: u8) -> Result<bool, Fatal> {
        let step = self.steps[i].pstep.step;
        let c = self.session_conn(step);
        let name = self.spec.steps[step].name.clone();
        let sql = self.spec.steps[step].sql.clone();
        if flags & STEP_RETRY == 0
            && self.steps[i].pstep.blockers.iter().any(|b| matches!(b, Blocker::Once))
        {
            let _ = writeln!(self.out, "step {name}: {sql} <waiting ...>");
            return Ok(true);
        }
        let start = Instant::now();
        let mut canceled = false;
        loop {
            let busy = self.conns[c].conn.is_busy()?;
            self.flush_notices(c);
            if !busy {
                break;
            }
            if self.conns[c].conn.fill(Some(Duration::from_millis(10)))? {
                continue;
            }
            if flags & STEP_NONBLOCK != 0 {
                let pid = self.conns[c].conn.pid.to_string();
                let waiting = match self.conns[0].conn.exec_prepared(PREP_WAITING, &pid)? {
                    Ok(v) => v.starts_with('t'),
                    Err(e) => return Err(Fatal(format!("lock wait query failed: {e}"))),
                };
                if waiting {
                    self.conns[c].conn.consume_input()?;
                    let busy = self.conns[c].conn.is_busy()?;
                    self.flush_notices(c);
                    if !busy {
                        break;
                    }
                    if flags & STEP_RETRY == 0 {
                        let _ = writeln!(self.out, "step {name}: {sql} <waiting ...>");
                    }
                    return Ok(true);
                }
            }
            let td = start.elapsed();
            if td > self.max_wait && !canceled && self.conns[c].conn.cancel() {
                {
                    let _ = writeln!(
                        self.out,
                        "isolationtester: canceling step {name} after {} seconds",
                        td.as_secs()
                    );
                    canceled = true;
                }
            }
            if td > 2 * self.max_wait {
                return Err(Fatal(format!(
                    "step {name} timed out after {} seconds\n",
                    td.as_secs()
                )));
            }
        }
        if self.step_has_blocker(i) {
            if flags & STEP_RETRY == 0 {
                let _ = writeln!(self.out, "step {name}: {sql} <waiting ...>");
            }
            return Ok(true);
        }
        if flags & STEP_RETRY != 0 {
            let _ = writeln!(self.out, "step {name}: <... completed>");
        } else {
            let _ = writeln!(self.out, "step {name}: {sql}");
        }
        loop {
            let r = self.conns[c].conn.get_result()?;
            self.flush_notices(c);
            let Some(r) = r else { break };
            match &r {
                PgResult::Command | PgResult::Empty => {}
                PgResult::Tuples { .. } => self.print_res(&r),
                PgResult::Error { severity, primary, .. } => {
                    let _ = writeln!(self.out, "{severity}:  {primary}");
                }
                PgResult::Local(m) => {
                    let _ = writeln!(self.out, "{m}");
                }
            }
        }
        // (PQconsumeInput's failure on a lost connection is ignored here.)
        let _ = self.conns[c].conn.consume_input();
        self.conns[c].conn.parse_input()?;
        self.flush_notices(c);
        let notifies = std::mem::take(&mut self.conns[c].conn.notifies);
        for (pid, channel, payload) in notifies {
            let sender = (1..self.conns.len())
                .find(|&k| self.conns[k].conn.pid == pid)
                .map_or_else(|| format!("PID {pid}"), |k| self.conns[k].name.clone());
            let _ = writeln!(
                self.out,
                "{}: NOTIFY \"{channel}\" with payload \"{payload}\" from {sender}",
                self.conns[c].name
            );
        }
        self.conns[c].active = None;
        Ok(false)
    }
}

/// Runs one spec on one server; returns isolationtester's output.
fn run_spec_on(addr: &str, cfg: &Config, test: &str, spec: &Spec) -> String {
    let mut out = spec.warnings.clone();
    let admin = ConnOpts {
        addr,
        db: "postgres",
        password: &cfg.password,
        appname: "pg_regress",
    };
    match Conn::connect(&admin) {
        Ok(mut c) => {
            let db = &cfg.db;
            let mut setup = vec![
                "SET client_min_messages = warning".to_string(),
                format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"),
                format!("CREATE DATABASE \"{db}\" TEMPLATE=template0 ENCODING='UTF8' LOCALE='C'"),
            ];
            for set in [
                "lc_messages TO 'C'",
                "lc_monetary TO 'C'",
                "lc_numeric TO 'C'",
                "lc_time TO 'C'",
                "bytea_output TO 'hex'",
                "timezone_abbreviations TO 'Default'",
            ] {
                setup.push(format!("ALTER DATABASE \"{db}\" SET {set}"));
            }
            for s in setup {
                // (A spec given up on may leave a session still busy in the database.)
                let deadline = Instant::now() + Duration::from_secs(300);
                while let Ok(Some(PgResult::Error { full, .. })) = c.exec(&s) {
                    if full.contains("is being accessed by other users") && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                    let _ = write!(out, "database setup failed: {full}");
                    return out;
                }
            }
        }
        Err(e) => {
            let _ = writeln!(out, "cannot connect to {addr}: {e}");
            return out;
        }
    }
    let appname = format!("isolation/{test}");
    let mut conns = Vec::new();
    for i in 0..=spec.sessions.len() {
        let name = if i == 0 {
            "control connection".to_string()
        } else {
            spec.sessions[i - 1].name.clone()
        };
        let o = ConnOpts {
            addr,
            db: &cfg.db,
            password: &cfg.password,
            appname: &appname,
        };
        let mut conn = match Conn::connect(&o) {
            Ok(c) => c,
            Err(e) => {
                let _ = write!(out, "Connection {i} failed: {e}");
                return out;
            }
        };
        let q = format!(
            "SELECT set_config('application_name',\n  current_setting('application_name') || '/' || {},\n  false)",
            quote_literal(&name)
        );
        match conn.exec(&q) {
            Ok(Some(PgResult::Tuples { .. })) => {}
            r => {
                let msg = match r {
                    Ok(Some(PgResult::Error { full, .. })) => full,
                    other => format!("{other:?}\n"),
                };
                let _ = write!(out, "setting of application name failed: {msg}");
                return out;
            }
        }
        conn.notices.clear();
        // (Setup and teardown may take long: a big table's load.)
        conn.timeout = Some(Duration::from_secs(900).max(4 * cfg.step_wait));
        conns.push(IsoConn {
            conn,
            name,
            active: None,
            total_notices: 0,
        });
    }
    let pids: Vec<String> = conns[1..].iter().map(|c| c.conn.pid.to_string()).collect();
    let wait_query = format!(
        "SELECT pg_catalog.pg_isolation_test_session_is_blocked($1, '{{{}}}')",
        pids.join(",")
    );
    match conns[0].conn.prepare(PREP_WAITING, &wait_query) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let _ = write!(out, "prepare of lock wait query failed: {e}");
            return out;
        }
        Err(e) => {
            let _ = writeln!(out, "prepare of lock wait query failed: {e}");
            return out;
        }
    }
    let _ = writeln!(out, "Parsed test spec with {} sessions", spec.sessions.len());
    let mut t = Tester {
        spec,
        conns,
        out,
        any_new_notice: false,
        max_wait: cfg.step_wait,
        steps: Vec::new(),
    };
    if let Err(Fatal(msg)) = t.run_spec() {
        t.out.push_str(&msg);
    }
    t.out
}

fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ---------------------------------------------------------------------------------------------
// Running the schedule

struct EngineProc {
    child: Child,
}

impl EngineProc {
    fn start(bin: &Path, data: &Path, log: &Path, port: u16) -> Self {
        assert!(
            TcpStream::connect(("127.0.0.1", port)).is_err(),
            "port {port} is in use (PGISOLATION_PORT picks another)"
        );
        let logf = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap();
        let child = Command::new(bin)
            .args([
                "--data",
                data.to_str().unwrap(),
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--no-sync",
                "--idle-close-secs",
                "100000",
            ])
            // (A reference started with max_prepared_transactions runs the 2PC specs'
            // prepared paths: the engine takes the same setting.)
            .args([
                "--max-prepared-transactions",
                &std::env::var("PGISOLATION_MAX_PREPARED").unwrap_or_else(|_| "0".into()),
            ])
            .env("RUST_LOG", "warn")
            .env("RUST_BACKTRACE", "0")
            .stdin(Stdio::null())
            .stdout(logf.try_clone().unwrap())
            .stderr(logf)
            .spawn()
            .expect("start roll-engine");
        let deadline = Instant::now() + Duration::from_secs(180);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "roll-engine did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        EngineProc { child }
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for EngineProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The engine's own specs (`tests/isolation/specs`): concurrency semantics applications rely
/// on beyond the suite's, run with `PGISOLATION_EXTRA=1` (after the schedule) or by name in
/// `PGISOLATION_ONLY`, with the reference's output as their expected one.
fn extra_specs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/isolation/specs")
}

/// A spec's file: the suite's, else the engine's own.
fn spec_path(cfg: &Config, test: &str) -> PathBuf {
    let suite = cfg.src.join("specs").join(format!("{test}.spec"));
    if suite.exists() { suite } else { extra_specs_dir().join(format!("{test}.spec")) }
}

fn load_spec(cfg: &Config, test: &str) -> Result<Spec, String> {
    let path = spec_path(cfg, test);
    let src = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // Specs naming pg_isolation_regress's database get the one this run uses.
    let src = if cfg.db != SPEC_DB { src.replace(SPEC_DB, &cfg.db) } else { src };
    parse_spec(&src)
}

/// Roles the specs create (cluster-global), so leftovers can be dropped at the end.
fn spec_roles(cfg: &Config, schedule: &[String]) -> BTreeSet<String> {
    let mut roles = BTreeSet::new();
    for t in schedule {
        let Ok(src) = std::fs::read_to_string(spec_path(cfg, t)) else {
            continue;
        };
        let words: Vec<&str> = src.split(|c: char| c.is_whitespace() || c == ';').filter(|w| !w.is_empty()).collect();
        for w in words.windows(3) {
            if w[0].eq_ignore_ascii_case("create")
                && (w[1].eq_ignore_ascii_case("role") || w[1].eq_ignore_ascii_case("user"))
            {
                roles.insert(w[2].trim_matches('"').to_string());
            }
        }
    }
    roles
}

fn existing_roles(addr: &str, cfg: &Config) -> BTreeSet<String> {
    let o = ConnOpts {
        addr,
        db: "postgres",
        password: &cfg.password,
        appname: "pg_regress",
    };
    let Ok(mut c) = Conn::connect(&o) else {
        return BTreeSet::new();
    };
    match c.exec("SELECT rolname FROM pg_roles") {
        Ok(Some(PgResult::Tuples { rows, .. })) => {
            rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect()
        }
        _ => BTreeSet::new(),
    }
}

fn cleanup(addr: &str, cfg: &Config, roles: &BTreeSet<String>) {
    let o = ConnOpts {
        addr,
        db: "postgres",
        password: &cfg.password,
        appname: "pg_regress",
    };
    let Ok(mut c) = Conn::connect(&o) else {
        return;
    };
    let _ = c.exec(&format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", cfg.db));
    for r in roles {
        if let Ok(Some(PgResult::Error { full, .. })) = c.exec(&format!("DROP ROLE IF EXISTS \"{r}\"")) {
            eprintln!("cleanup: {full}");
        }
    }
}

fn run_all(cfg: &Config, schedule: &[String]) {
    for d in ["engine", "reference"] {
        std::fs::create_dir_all(cfg.out.join(d)).unwrap();
    }
    let data = cfg.root.join("enginedata");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let log = cfg.out.join("engine.log");
    let _ = std::fs::remove_file(&log);
    // A private copy: other builds may relink target/debug/roll-engine during the run.
    let bin = cfg.root.join("roll-engine.bin");
    let src = std::env::var("PGISOLATION_ENGINE_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_roll-engine").to_string());
    std::fs::copy(&src, &bin).expect("copy roll-engine");
    let mut engine = EngineProc::start(&bin, &data, &log, cfg.port);
    let engine_addr = format!("127.0.0.1:{}", cfg.port);
    let roles = spec_roles(cfg, schedule);
    let ref_before = existing_roles(&cfg.reference, cfg);
    assert!(!ref_before.is_empty(), "cannot reach the reference at {}", cfg.reference);
    for test in schedule {
        let spec = match load_spec(cfg, test) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{test}: {e}");
                continue;
            }
        };
        let ref_path = cfg.out.join("reference").join(format!("{test}.out"));
        let reuse = cfg.reuse_reference && ref_path.exists();
        let t0 = Instant::now();
        let (eng_out, ref_out) = std::thread::scope(|s| {
            let r = (!reuse).then(|| s.spawn(|| run_spec_on(&cfg.reference, cfg, test, &spec)));
            let e = run_spec_on(&engine_addr, cfg, test, &spec);
            (e, r.map(|h| h.join().unwrap()))
        });
        if let Some(r) = ref_out {
            std::fs::write(&ref_path, r).unwrap();
        }
        std::fs::write(cfg.out.join("engine").join(format!("{test}.out")), &eng_out).unwrap();
        let same = std::fs::read_to_string(&ref_path).is_ok_and(|r| r == eng_out);
        eprintln!(
            "{test}: {} ({:.1}s)",
            if same { "ok" } else { "differs" },
            t0.elapsed().as_secs_f64()
        );
        if !engine.alive() {
            eprintln!("{test}: the engine died; restarting it");
            engine = EngineProc::start(&bin, &data, &log, cfg.port);
        }
    }
    cleanup(&engine_addr, cfg, &roles);
    let left: BTreeSet<String> = existing_roles(&cfg.reference, cfg)
        .into_iter()
        .filter(|r| roles.contains(r) && !ref_before.contains(r))
        .collect();
    cleanup(&cfg.reference, cfg, &left);
    drop(engine);
}

// ---------------------------------------------------------------------------------------------
// Comparing and reporting

struct SpecResult {
    name: String,
    perms: usize,
    perms_matched: usize,
    steps: usize,
    steps_matched: usize,
    whole: bool,
    /// The reference's output equals expected/<name>.out (or a variant).
    reference_expected: bool,
    /// The engine's output equals an expected file.
    engine_expected: bool,
    /// The first differing permutation: (reference, engine).
    first_diff: Option<(String, String)>,
}

impl SpecResult {
    fn matches(&self) -> bool {
        self.whole
    }
}

/// The output cut into its header and permutations.
fn permutations(out: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let mut start = 0;
    let marker = "\nstarting permutation:";
    let mut from = 0;
    while let Some(i) = out[from..].find(marker) {
        let at = from + i;
        v.push(&out[start..at]);
        start = at;
        from = at + 1;
    }
    v.push(&out[start..]);
    v
}

/// A permutation cut into units: what precedes the first step line, then each `step` line with
/// what follows it.
fn step_units(perm: &str) -> Vec<String> {
    let mut v = vec![String::new()];
    for line in perm.split_inclusive('\n') {
        if line.starts_with("step ") {
            v.push(String::new());
        }
        v.last_mut().unwrap().push_str(line);
    }
    v
}

fn expected_files(cfg: &Config, test: &str) -> Vec<String> {
    let mut v = Vec::new();
    for suffix in ["".to_string(), "_1".into(), "_2".into(), "_3".into(), "_4".into()] {
        if let Ok(s) = std::fs::read_to_string(cfg.src.join("expected").join(format!("{test}{suffix}.out"))) {
            v.push(if cfg.db != SPEC_DB { s.replace(SPEC_DB, &cfg.db) } else { s });
        }
    }
    v
}

fn analyze(cfg: &Config, test: &str) -> Option<SpecResult> {
    let r = std::fs::read_to_string(cfg.out.join("reference").join(format!("{test}.out"))).ok()?;
    let e = std::fs::read_to_string(cfg.out.join("engine").join(format!("{test}.out"))).ok()?;
    let rp = permutations(&r);
    let ep = permutations(&e);
    let mut res = SpecResult {
        name: test.to_string(),
        perms: rp.len().saturating_sub(1),
        perms_matched: 0,
        steps: 0,
        steps_matched: 0,
        whole: r == e,
        reference_expected: false,
        engine_expected: false,
        first_diff: None,
    };
    let exp = expected_files(cfg, test);
    res.reference_expected = exp.contains(&r);
    res.engine_expected = exp.contains(&e);
    for (i, p) in rp.iter().enumerate() {
        let q = ep.get(i).copied().unwrap_or("");
        let ru = step_units(p);
        let eu = step_units(q);
        if i > 0 {
            res.steps += ru.len() - 1;
            res.steps_matched += ru
                .iter()
                .zip(&eu)
                .skip(1)
                .filter(|(a, b)| a == b)
                .count();
            if *p == q {
                res.perms_matched += 1;
            }
        }
        if *p != q && res.first_diff.is_none() {
            res.first_diff = Some((p.to_string(), q.to_string()));
        }
    }
    if res.first_diff.is_none() && ep.len() > rp.len() {
        res.first_diff = Some((String::new(), ep[rp.len()..].concat()));
    }
    Some(res)
}

/// A line diff (LCS) of two short texts, unified-style without hunk headers.
fn diff(a: &str, b: &str) -> String {
    let a: Vec<&str> = a.lines().collect();
    let b: Vec<&str> = b.lines().collect();
    let (n, m) = (a.len(), b.len());
    if n * m > 4_000_000 {
        return format!("(too long to diff: {n} vs {m} lines)\n");
    }
    let mut l = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i][j] = if a[i] == b[j] { l[i + 1][j + 1] + 1 } else { l[i + 1][j].max(l[i][j + 1]) };
        }
    }
    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            let _ = writeln!(out, " {}", a[i]);
            i += 1;
            j += 1;
        } else if j < m && (i == n || l[i][j + 1] >= l[i + 1][j]) {
            let _ = writeln!(out, "+{}", b[j]);
            j += 1;
        } else {
            let _ = writeln!(out, "-{}", a[i]);
            i += 1;
        }
    }
    out
}

fn pct(a: usize, b: usize) -> f64 {
    if b == 0 { 100.0 } else { a as f64 * 100.0 / b as f64 }
}

fn write_report(cfg: &Config, results: &[SpecResult]) {
    let hunks = cfg.report_dir.join("hunks");
    let _ = std::fs::create_dir_all(&hunks);
    let sum = |f: fn(&SpecResult) -> usize| results.iter().map(f).sum::<usize>();
    let (perms, pm, steps, sm) = (
        sum(|r| r.perms),
        sum(|r| r.perms_matched),
        sum(|r| r.steps),
        sum(|r| r.steps_matched),
    );
    let specs_ok = results.iter().filter(|r| r.matches()).count();
    let mut s = String::new();
    s.push_str("# PostgreSQL isolation suite: run report\n\n");
    s.push_str("Generated by `tests/pg_isolation.rs` (see its header for the method). The classified list of\ndifferences is in `STATUS.md`.\n\n");
    let _ = writeln!(s, "| | |\n|---|---|");
    let _ = writeln!(s, "| specs | {} |", results.len());
    let _ = writeln!(s, "| specs matching | {specs_ok} ({:.1}%) |", pct(specs_ok, results.len()));
    let _ = writeln!(s, "| permutations | {perms} |");
    let _ = writeln!(s, "| permutations matching | {pm} ({:.1}%) |", pct(pm, perms));
    let _ = writeln!(s, "| steps | {steps} |");
    let _ = writeln!(s, "| steps matching | {sm} ({:.1}%) |", pct(sm, steps));
    let _ = writeln!(
        s,
        "| reference output == expected/*.out | {} of {} |",
        results.iter().filter(|r| r.reference_expected).count(),
        results.len()
    );
    let _ = writeln!(
        s,
        "| engine output == expected/*.out | {} of {} |\n",
        results.iter().filter(|r| r.engine_expected).count(),
        results.len()
    );
    s.push_str("| spec | permutations | steps | ref = expected |\n|---|---:|---:|:---:|\n");
    for r in results {
        let _ = writeln!(
            s,
            "| {}{} | {}/{} | {}/{} | {} |",
            r.name,
            if r.matches() { "" } else { " ✗" },
            r.perms_matched,
            r.perms,
            r.steps_matched,
            r.steps,
            if r.reference_expected { "yes" } else { "no" }
        );
        let path = hunks.join(format!("{}.diff", r.name));
        match &r.first_diff {
            Some((a, b)) => {
                let mut d = diff(a, b);
                if d.len() > 20_000 {
                    let mut cut = 20_000;
                    while !d.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    d.truncate(cut);
                    d.push_str("\n...\n");
                }
                let _ = std::fs::write(&path, format!("--- reference\n+++ engine\n{d}"));
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    std::fs::write(cfg.report_dir.join("REPORT.md"), s).unwrap();
}
