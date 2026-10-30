//! The server log, as PostgreSQL writes it to stderr: what log_statement,
//! log_min_duration_statement, log_duration, log_min_messages / log_min_error_statement,
//! log_connections / log_disconnections, auto_explain and the postmaster (reloads) report,
//! every line prefixed by log_line_prefix and laid out as elog.c's
//! send_message_to_server_log lays it out, so log analysers (pgBadger and the like) read it.
//!
//! The engine is a library: nothing is written until the embedder sets a sink (`set_sink`;
//! `roll-engine` writes to its stderr, as PostgreSQL's logging_collector = off does) or starts
//! the logging collector (`collector::start`: log_directory's files, csvlog and jsonlog,
//! rotation). Lines are formatted on the writing thread and written by a background one, so
//! a slow disk or terminal never holds up a query.

pub mod collector;
mod formats;
pub mod lifecycle;

use crate::error::{Error, Severity};
use crate::types::datetime::{TimeZone, timestamp2tm};
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub type Sink = Arc<dyn Fn(&str) + Send + Sync>;

static SINK: RwLock<Option<Sink>> = RwLock::new(None);
/// A sink is set or the collector runs (one load per message to skip all of it).
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Where log lines go (None: nowhere). Each call gets one message's lines.
pub fn set_sink(sink: Option<Sink>) {
    *SINK.write() = sink;
    refresh_active();
}

fn refresh_active() {
    ACTIVE.store(SINK.read().is_some() || collector::running(), Ordering::Relaxed);
}

/// The process's stderr, written by a background thread (buffered, flushed whenever it has
/// nothing more to write).
pub fn stderr_sink() -> Sink {
    let w = collector::stderr_writer();
    Arc::new(move |s: &str| w.send(s))
}

/// Whether anything is written at all (callers skip building messages otherwise).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// What a message is written as (log_destination's formats).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Stderr,
    Csv,
    Json,
}

fn emit(kind: Kind, s: String) {
    if collector::running() {
        collector::send(kind, s);
        return;
    }
    if kind == Kind::Stderr {
        let sink = SINK.read().clone();
        if let Some(f) = sink {
            f(&s);
        }
    }
}

/// Everything written so far is on its way out (tests, shutdown).
pub fn flush() {
    collector::flush();
}

/// log_destination: which formats each message is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dest {
    pub stderr: bool,
    pub csv: bool,
    pub json: bool,
}

impl Dest {
    pub const STDERR: Dest = Dest { stderr: true, csv: false, json: false };

    /// check_log_destination's list (syslog and eventlog write nothing here).
    pub fn parse(v: &str) -> Dest {
        let mut d = Dest { stderr: false, csv: false, json: false };
        for w in v.split(',').map(|w| w.trim().to_ascii_lowercase()) {
            match w.as_str() {
                "stderr" => d.stderr = true,
                "csvlog" => d.csv = true,
                "jsonlog" => d.json = true,
                _ => {}
            }
        }
        d
    }
}

/// A process's log line counters, one per format (log_status_format's %l, csvlog's and
/// jsonlog's own).
#[derive(Debug, Default)]
pub struct Lines {
    stderr: AtomicU64,
    csv: AtomicU64,
    json: AtomicU64,
}

impl Lines {
    pub const fn new() -> Lines {
        Lines { stderr: AtomicU64::new(0), csv: AtomicU64::new(0), json: AtomicU64::new(0) }
    }

    fn next(&self, kind: Kind) -> u64 {
        let c = match kind {
            Kind::Stderr => &self.stderr,
            Kind::Csv => &self.csv,
            Kind::Json => &self.json,
        };
        c.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// elog levels in their order (elog.h's numbers).
fn rank(sev: Severity) -> i32 {
    match sev {
        Severity::Debug => 14,
        Severity::Log => 15,
        Severity::Info => 17,
        Severity::Notice => 18,
        Severity::Warning => 19,
        Severity::Error => 21,
        Severity::Fatal => 22,
    }
}

/// A log_min_messages / log_min_error_statement value's level.
fn setting_rank(v: &str) -> i32 {
    match v {
        "debug5" => 10,
        "debug4" => 11,
        "debug3" => 12,
        "debug2" => 13,
        "debug1" => 14,
        "log" => 15,
        "info" => 17,
        "notice" => 18,
        "warning" => 19,
        "error" => 21,
        "fatal" => 22,
        _ => 23,
    }
}

/// is_log_level_output: LOG ranks between ERROR and FATAL for the server log.
pub fn level_output(sev: Severity, min: &str) -> bool {
    let (e, m) = (rank(sev), setting_rank(min));
    if sev == Severity::Log {
        m == 15 || m <= 21
    } else if m == 15 {
        e >= 22
    } else {
        e >= m
    }
}

/// What log_line_prefix's escapes (and csvlog's / jsonlog's fields) read of the process
/// writing.
#[derive(Clone, Debug, Default)]
pub struct Who {
    pub pid: i32,
    /// A session's (None: the postmaster or another process without one).
    pub session: Option<SessionWho>,
    pub backend_type: &'static str,
    /// When the process started (MyStartTime; microseconds since 2000-01-01).
    pub start: i64,
    /// A client backend's client address and port (None: no client; "[local]" without a
    /// port: a Unix-socket one).
    pub remote: Option<(String, Option<u16>)>,
    /// A client backend's server address (%L; None: "[local]").
    pub local: Option<String>,
    pub lines: Arc<Lines>,
}

impl Who {
    /// A client backend before its session exists: "not initialized" until the startup
    /// packet is read (connection received), then its user and database (authentication).
    pub fn backend(pid: i32, start: i64, conn: &Conn) -> Who {
        Who {
            pid,
            session: conn.startup.as_ref().map(|s| SessionWho {
                user: s.user.clone(),
                database: s.database.clone(),
                command_tag: s.ps.into(),
                // (InitPostgres's transaction, the backend's first; none before it, while
                // the startup packet is checked.)
                vxid: if s.ps.is_empty() { String::new() } else { "3/1".into() },
                ..SessionWho::default()
            }),
            backend_type: match &conn.startup {
                None => "not initialized",
                Some(_) => conn.backend_type(),
            },
            start,
            remote: conn.remote.clone(),
            local: conn.local.clone(),
            lines: conn.lines.clone(),
        }
    }

    /// The postmaster's.
    pub fn postmaster() -> Who {
        static LINES: std::sync::OnceLock<Arc<Lines>> = std::sync::OnceLock::new();
        Who { backend_type: "postmaster", ..Who::aux(std::process::id() as i32, "postmaster", &LINES) }
    }

    /// An auxiliary process of the server's (checkpointer, autovacuum worker, ...): the
    /// engine's own threads, under the pid pg_stat_activity shows for them, with that
    /// process's line counters.
    pub fn aux(pid: i32, backend_type: &'static str, lines: &std::sync::OnceLock<Arc<Lines>>) -> Who {
        Who {
            pid,
            session: None,
            backend_type,
            start: process_start(),
            remote: None,
            local: None,
            lines: lines.get_or_init(Default::default).clone(),
        }
    }

    /// A process with a client connection (MyProcPort), the not yet initialized backend
    /// that received it included.
    fn is_client(&self) -> bool {
        matches!(self.backend_type, "client backend" | "walsender" | "not initialized")
    }
}

/// What the log reads of a client connection (MyProcPort), kept for its whole life.
#[derive(Clone, Debug, Default)]
pub struct Conn {
    pub remote: Option<(String, Option<u16>)>,
    pub local: Option<String>,
    pub lines: Arc<Lines>,
    /// The startup packet's user and database, and the process title (empty until
    /// authentication starts), until the session exists.
    pub startup: Option<Startup>,
    /// A replication connection's (`replication=...`): a walsender.
    pub walsender: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Startup {
    pub user: String,
    pub database: String,
    pub ps: &'static str,
}

impl Conn {
    /// `unix`: a connection standing for a Unix-socket one (`hba::mark_local`): "[local]",
    /// no port, no server address.
    pub fn new(peer: Option<std::net::SocketAddr>, local: Option<std::net::SocketAddr>, unix: bool) -> Conn {
        Conn {
            remote: if unix {
                Some(("[local]".into(), None))
            } else {
                peer.map(|p| (p.ip().to_canonical().to_string(), Some(p.port())))
            },
            local: local.filter(|_| !unix).map(|l| l.ip().to_canonical().to_string()),
            lines: Arc::default(),
            startup: None,
            walsender: false,
        }
    }

    /// %b once the startup packet is read.
    pub fn backend_type(&self) -> &'static str {
        if self.walsender { "walsender" } else { "client backend" }
    }
}

/// log_connections' aspects: PostgreSQL 18's list (receipt, authentication, authorization,
/// setup_durations, all), or one of the booleans alone (on: the first three).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ConnAspects {
    pub receipt: bool,
    pub authentication: bool,
    pub authorization: bool,
    pub setup_durations: bool,
}

impl ConnAspects {
    /// check_log_connections: the aspects of a value, or the DETAIL of an invalid one.
    pub fn parse(raw: &str) -> std::result::Result<ConnAspects, String> {
        if raw.trim().is_empty() {
            return Ok(ConnAspects::default());
        }
        let Some(list) = crate::session::settings::split_identifiers(raw) else {
            return Err("Invalid list syntax in parameter \"log_connections\".".into());
        };
        let on = ConnAspects { receipt: true, authentication: true, authorization: true, setup_durations: false };
        let mut a = ConnAspects::default();
        if let Some(first) = list.first() {
            let compat = match first.to_ascii_lowercase().as_str() {
                "off" | "false" | "no" | "0" => Some(ConnAspects::default()),
                "on" | "true" | "yes" | "1" => Some(on),
                _ => None,
            };
            if let Some(c) = compat {
                if list.len() > 1 {
                    return Err(format!("Cannot specify log_connections option \"{first}\" in a list with other options."));
                }
                return Ok(c);
            }
        }
        for item in &list {
            match item.to_ascii_lowercase().as_str() {
                "receipt" => a.receipt = true,
                "authentication" => a.authentication = true,
                "authorization" => a.authorization = true,
                "setup_durations" => a.setup_durations = true,
                "all" => a = ConnAspects { setup_durations: true, ..on },
                _ => return Err(format!("Invalid option \"{item}\".")),
            }
        }
        Ok(a)
    }

    /// A value already checked (an invalid one logs nothing).
    pub fn of(raw: &str) -> ConnAspects {
        ConnAspects::parse(raw).unwrap_or_default()
    }
}

/// %c, csvlog's session_id: the process's start time and pid, in hex.
fn session_id(who: &Who) -> String {
    format!("{:x}.{:x}", who.start.div_euclid(1_000_000) + 946_684_800, who.pid)
}

/// When this process (the "postmaster") started.
fn process_start() -> i64 {
    static START: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *START.get_or_init(|| crate::types::datetime::Clock::system().now)
}

#[derive(Clone, Debug, Default)]
pub struct SessionWho {
    pub user: String,
    pub database: String,
    pub application_name: String,
    pub command_tag: String,
    pub vxid: String,
    pub xid: u64,
    pub query_id: i64,
}

/// A timestamp as %m / %t print it, in log_timezone.
fn stamp(now: i64, tz: &TimeZone, millis: bool) -> String {
    let Some((tm, _)) = timestamp2tm(now, Some(tz)) else {
        return String::new();
    };
    let zone = tm.zone.clone().unwrap_or_else(|| tz.abbrev_at_utc(now));
    let base =
        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", tm.year, tm.mon, tm.mday, tm.hour, tm.min, tm.sec);
    if millis {
        format!("{base}.{:03} {zone}", now.rem_euclid(1_000_000) / 1000)
    } else {
        format!("{base} {zone}")
    }
}

/// How lines are laid out: log_line_prefix, log_timezone and log_error_verbosity.
#[derive(Clone, Debug)]
pub struct Format {
    pub prefix: String,
    pub tz: TimeZone,
    pub verbosity: Verbosity,
    pub min_messages: String,
    pub min_error_statement: String,
    pub dest: Dest,
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum Verbosity {
    Terse,
    Default,
    Verbose,
}

impl Format {
    /// The postmaster's (the defaults: '%m [%p] ', UTC, default verbosity).
    pub fn postmaster() -> Format {
        Format {
            prefix: "%m [%p] ".into(),
            tz: TimeZone::parse("UTC").unwrap_or_else(TimeZone::gmt),
            verbosity: Verbosity::Default,
            min_messages: "warning".into(),
            min_error_statement: "error".into(),
            dest: Dest::STDERR,
        }
    }

    /// A process without a session's, under the configuration files' settings `files` and
    /// the server's command line (`set_command_line`).
    pub fn from_files(files: Option<&crate::sysconf::Loaded>) -> Format {
        let mut f = Format::postmaster();
        let argv = ARGV.read();
        let get = |n: &str| {
            argv.iter()
                .rev()
                .find(|(k, _)| k == n)
                .map(|(_, v)| v.clone())
                .or_else(|| files.and_then(|l| l.values.get(n)).map(|v| v.value.clone()))
        };
        {
            if let Some(v) = get("log_line_prefix") {
                f.prefix = v;
            }
            if let Some(tz) = get("log_timezone").and_then(|v| TimeZone::parse(&v)) {
                f.tz = tz;
            }
            if let Some(v) = get("log_min_messages") {
                f.min_messages = v;
            }
            if let Some(v) = get("log_error_verbosity") {
                f.verbosity = verbosity(&v);
            }
            if let Some(v) = get("log_destination") {
                f.dest = Dest::parse(&v);
            }
        }
        f
    }

    pub fn from_settings(st: &crate::session::settings::Settings) -> Format {
        let get = |n: &str| st.get_str(n).unwrap_or_default().to_string();
        Format {
            prefix: get("log_line_prefix"),
            tz: TimeZone::parse(st.get_str("log_timezone").unwrap_or("UTC")).unwrap_or_else(TimeZone::gmt),
            verbosity: verbosity(st.get_str("log_error_verbosity").unwrap_or_default()),
            min_messages: get("log_min_messages"),
            min_error_statement: get("log_min_error_statement"),
            dest: Dest::parse(st.get_str("log_destination").unwrap_or("stderr")),
        }
    }
}

/// The server's command line (`roll-engine -c`), above the files for the postmaster's and
/// connections' lines before a session exists.
static ARGV: RwLock<Vec<(String, String)>> = RwLock::new(Vec::new());

pub fn set_command_line(argv: &[(String, String)]) {
    *ARGV.write() = argv.to_vec();
}

fn verbosity(v: &str) -> Verbosity {
    match v {
        "terse" => Verbosity::Terse,
        "verbose" => Verbosity::Verbose,
        _ => Verbosity::Default,
    }
}

/// log_line_prefix expanded for one line (log_status_format): `sqlstate` is %e, `line` %l.
fn prefix(f: &Format, who: &Who, now: i64, sqlstate: &str, line: u64) -> String {
    let mut out = String::new();
    let mut it = f.prefix.chars().peekable();
    let client = who.is_client();
    let unknown = |v: &str| if v.is_empty() { "[unknown]".to_string() } else { v.to_string() };
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // Padding: %-10a, %10a.
        let mut pad = String::new();
        while let Some(&d) = it.peek() {
            if d == '-' || d.is_ascii_digit() {
                pad.push(d);
                it.next();
            } else {
                break;
            }
        }
        let Some(e) = it.next() else { break };
        let s = who.session.as_ref().filter(|_| client);
        let v: Option<String> = match e {
            'a' if client => Some(unknown(s.map_or("", |s| &s.application_name))),
            'u' if client => Some(unknown(s.map_or("", |s| &s.user))),
            'd' if client => Some(unknown(s.map_or("", |s| &s.database))),
            'a' | 'u' | 'd' => None,
            'b' => Some(who.backend_type.to_string()),
            'c' => Some(session_id(who)),
            'p' => Some(who.pid.to_string()),
            'P' => None,
            'l' => Some(line.to_string()),
            'm' => Some(stamp(now, &f.tz, true)),
            't' => Some(stamp(now, &f.tz, false)),
            'n' => Some(format!("{}.{:03}", now.div_euclid(1_000_000) + 946_684_800, now.rem_euclid(1_000_000) / 1000)),
            's' => Some(stamp(who.start, &f.tz, false)),
            'i' if client => Some(s.map_or(String::new(), |s| s.command_tag.clone())),
            'e' => Some(sqlstate.to_string()),
            'r' => who.remote.as_ref().map(|(h, p)| match p {
                Some(p) => format!("{h}({p})"),
                None => h.clone(),
            }),
            'h' => who.remote.as_ref().map(|(h, _)| h.clone()),
            'L' if client => Some(who.local.clone().unwrap_or_else(|| "[local]".into())),
            'L' => Some("[none]".into()),
            'v' => s.map(|s| s.vxid.clone()),
            'x' => Some(s.map_or(0, |s| s.xid).to_string()),
            'Q' => Some(s.map_or(0, |s| s.query_id).to_string()),
            'q' if !client => break,
            'q' => continue,
            '%' => Some("%".into()),
            _ => continue,
        };
        let v = v.unwrap_or_default();
        match pad.parse::<i64>() {
            Ok(n) if n < 0 => out.push_str(&format!("{v:<w$}", w = (-n) as usize)),
            Ok(n) => out.push_str(&format!("{v:>w$}", w = n as usize)),
            Err(_) => out.push_str(&v),
        }
    }
    out
}

/// One message for the server log.
#[derive(Clone, Debug, Default)]
pub struct Message<'a> {
    pub severity: Option<Severity>,
    pub code: &'a str,
    pub text: &'a str,
    pub detail: Option<&'a str>,
    pub hint: Option<&'a str>,
    pub context: Option<&'a str>,
    /// The query running (debug_query_string), unless the message hides it.
    pub statement: Option<&'a str>,
    /// Where in `statement` the error is (a 1-based character position).
    pub cursorpos: Option<u32>,
    /// The query run internally that the error is in (QUERY), and the position in it.
    pub internal: Option<(u32, &'a str)>,
    /// (routine, file, line) for log_error_verbosity = verbose.
    pub location: Option<(&'a str, &'a str, String)>,
}

impl<'a> Message<'a> {
    pub fn log(text: &'a str) -> Message<'a> {
        Message { severity: Some(Severity::Log), code: "00000", text, ..Default::default() }
    }

    pub fn error(e: &'a Error, statement: Option<&'a str>) -> Message<'a> {
        let (file, line, routine) = crate::error_location::location(e);
        Message {
            severity: Some(e.severity),
            code: e.code,
            text: &e.message,
            detail: e.detail.as_deref(),
            hint: e.hint.as_deref(),
            context: e.context.as_deref(),
            statement,
            cursorpos: e.position.filter(|p| *p > 0),
            internal: e.objects.as_ref().and_then(|o| o.internal.as_ref()).map(|(p, q)| (*p, q.as_str())),
            location: Some((routine, file, line)),
        }
    }

    fn severity(&self) -> Severity {
        self.severity.unwrap_or(Severity::Log)
    }

    /// check_log_of_query: the statement goes with the message past log_min_error_statement.
    fn statement_shown(&self, f: &Format) -> Option<&'a str> {
        self.statement.filter(|_| level_output(self.severity(), &f.min_error_statement))
    }
}

/// Whether `sev` goes to the server log under `f` (log_min_messages).
pub fn wanted(f: &Format, sev: Severity) -> bool {
    active() && level_output(sev, &f.min_messages)
}

/// Write `m` (send_message_to_server_log) in each of log_destination's formats, if
/// log_min_messages wants it. Without the logging collector, csvlog and jsonlog fall back to
/// stderr's format, as PostgreSQL's do.
pub fn write(f: &Format, who: &Who, m: &Message) {
    if !wanted(f, m.severity()) {
        return;
    }
    let now = crate::types::datetime::Clock::system().now;
    let collecting = collector::running();
    if f.dest.stderr || (!collecting && (f.dest.csv || f.dest.json)) {
        emit(Kind::Stderr, text(f, who, now, m));
    }
    if collecting && f.dest.csv {
        emit(Kind::Csv, formats::csv(f, who, now, who.lines.next(Kind::Csv), m));
    }
    if collecting && f.dest.json {
        emit(Kind::Json, formats::json(f, who, now, who.lines.next(Kind::Json), m));
    }
}

/// The stderr format: every line under its own log_line_prefix.
fn text(f: &Format, who: &Who, now: i64, m: &Message) -> String {
    let line = || prefix(f, who, now, m.code, who.lines.next(Kind::Stderr));
    let tabs = |s: &str| s.replace('\n', "\n\t");
    let mut out = format!("{}{}:  ", line(), m.severity().as_str());
    if f.verbosity == Verbosity::Verbose {
        out.push_str(&format!("{}: ", m.code));
    }
    out.push_str(&tabs(m.text));
    if let Some(p) = m.cursorpos.or(m.internal.map(|(p, _)| p).filter(|p| *p > 0)) {
        out.push_str(&format!(" at character {p}"));
    }
    out.push('\n');
    if f.verbosity >= Verbosity::Default {
        let query = m.internal.map(|(_, q)| q);
        for (label, v) in [("DETAIL", m.detail), ("HINT", m.hint), ("QUERY", query), ("CONTEXT", m.context)] {
            if let Some(v) = v {
                out.push_str(&format!("{}{label}:  {}\n", line(), tabs(v)));
            }
        }
        if f.verbosity == Verbosity::Verbose
            && let Some((routine, file, l)) = &m.location
        {
            out.push_str(&format!("{}LOCATION:  {routine}, {file}:{l}\n", line()));
        }
    }
    if let Some(q) = m.statement_shown(f) {
        out.push_str(&format!("{}STATEMENT:  {}\n", line(), tabs(q)));
    }
    out
}

/// A message of the postmaster (reloads, startup): its own process, under the configuration
/// files' settings `files` (None: the defaults).
pub fn postmaster(files: Option<&crate::sysconf::Loaded>, sev: Severity, text: &str) {
    if !active() {
        return;
    }
    let f = Format::from_files(files);
    write(&f, &Who::postmaster(), &Message { severity: Some(sev), ..Message::log(text) });
}

/// A statement's log context for lines written from inside it.
pub struct Backend {
    pub format: Format,
    pub who: Who,
    /// log_lock_waits: the lock manager's lines.
    pub lock_waits: bool,
    /// auto_explain.log_nested_statements: nested statements' plans.
    pub nested: bool,
}

thread_local! {
    /// The statement running on this thread, for lines written from deep inside it (the lock
    /// manager's log_lock_waits lines, auto_explain's nested statements). Set only for
    /// sessions that asked for such lines (`Session::set_ps`).
    static BACKEND: std::cell::RefCell<Option<Backend>> = const { std::cell::RefCell::new(None) };
}

/// The running statement's log context (None: no lines from inside it).
pub fn set_backend(ctx: Option<Backend>) {
    BACKEND.with(|b| *b.borrow_mut() = ctx);
}

/// Whether the running statement has a log context.
pub fn has_backend() -> bool {
    BACKEND.with(|b| b.borrow().is_some())
}

/// Whether the running statement logs its lock waits.
pub fn logs_lock_waits() -> bool {
    BACKEND.with(|b| b.borrow().as_ref().is_some_and(|b| b.lock_waits))
}

/// Whether the running statement explains its nested statements.
pub fn logs_nested() -> bool {
    BACKEND.with(|b| b.borrow().as_ref().is_some_and(|b| b.nested))
}

/// A line of the running statement's backend (nothing when it has no log context).
pub fn backend(m: &Message) {
    BACKEND.with(|b| {
        if let Some(b) = b.borrow().as_ref() {
            write(&b.format, &b.who, m);
        }
    });
}
