//! The server log, as PostgreSQL writes it to stderr: what log_statement,
//! log_min_duration_statement, log_duration, log_min_messages / log_min_error_statement,
//! log_connections / log_disconnections, auto_explain and the postmaster (reloads) report,
//! every line prefixed by log_line_prefix and laid out as elog.c's
//! send_message_to_server_log lays it out, so log analysers (pgBadger and the like) read it.
//!
//! The engine is a library: nothing is written until the embedder sets a sink (`set_sink`;
//! `roll-engine` writes to its stderr, as PostgreSQL's logging_collector = off does).

use crate::error::{Error, Severity};
use crate::types::datetime::{TimeZone, timestamp2tm};
use parking_lot::RwLock;
use std::sync::Arc;

pub type Sink = Arc<dyn Fn(&str) + Send + Sync>;

static SINK: RwLock<Option<Sink>> = RwLock::new(None);

/// Where log lines go (None: nowhere). Each call gets one message's lines.
pub fn set_sink(sink: Option<Sink>) {
    *SINK.write() = sink;
}

/// The process's stderr.
pub fn stderr_sink() -> Sink {
    Arc::new(|s: &str| {
        use std::io::Write;
        let _ = std::io::stderr().lock().write_all(s.as_bytes());
    })
}

/// Whether anything is written at all (callers skip building messages otherwise).
pub fn active() -> bool {
    SINK.read().is_some()
}

fn emit(s: &str) {
    let sink = SINK.read().clone();
    if let Some(f) = sink {
        f(s);
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

/// What log_line_prefix's escapes read of the process writing.
#[derive(Clone, Debug, Default)]
pub struct Who {
    pub pid: i32,
    /// A session's (None: the postmaster or another process without one).
    pub session: Option<SessionWho>,
    pub backend_type: &'static str,
}

#[derive(Clone, Debug, Default)]
pub struct SessionWho {
    pub user: String,
    pub database: String,
    pub application_name: String,
    /// The client's address and port (None: no network client).
    pub remote: Option<(String, u16)>,
    /// When the session started (microseconds since 2000-01-01).
    pub start: i64,
    /// The session's log line number (%l), counted by the caller.
    pub line: u64,
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
        }
    }

    pub fn from_settings(st: &crate::session::settings::Settings) -> Format {
        let get = |n: &str| st.get_str(n).unwrap_or_default().to_string();
        Format {
            prefix: get("log_line_prefix"),
            tz: TimeZone::parse(st.get_str("log_timezone").unwrap_or("UTC")).unwrap_or_else(TimeZone::gmt),
            verbosity: match st.get_str("log_error_verbosity") {
                Some("terse") => Verbosity::Terse,
                Some("verbose") => Verbosity::Verbose,
                _ => Verbosity::Default,
            },
            min_messages: get("log_min_messages"),
            min_error_statement: get("log_min_error_statement"),
        }
    }
}

/// log_line_prefix expanded for one message (log_status_format). `sqlstate`: %e.
fn prefix(f: &Format, who: &Who, now: i64, sqlstate: &str) -> String {
    let mut out = String::new();
    let mut it = f.prefix.chars().peekable();
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
        let s = who.session.as_ref();
        let v: Option<String> = match e {
            'a' => s.map(|s| s.application_name.clone()),
            'u' => s.map(|s| s.user.clone()),
            'd' => s.map(|s| s.database.clone()),
            'b' => Some(who.backend_type.to_string()),
            'c' => Some(format!("{:x}.{:x}", s.map_or(0, |s| (s.start / 1_000_000) + 946_684_800), who.pid)),
            'p' => Some(who.pid.to_string()),
            'P' => None,
            'l' => Some(s.map_or(0, |s| s.line).to_string()),
            'm' => Some(stamp(now, &f.tz, true)),
            't' => Some(stamp(now, &f.tz, false)),
            'n' => Some(format!("{}.{:03}", now.div_euclid(1_000_000) + 946_684_800, now.rem_euclid(1_000_000) / 1000)),
            's' => s.map(|s| stamp(s.start, &f.tz, false)),
            'i' => s.map(|s| s.command_tag.clone()),
            'e' => Some(sqlstate.to_string()),
            'r' => s.and_then(|s| s.remote.as_ref()).map(|(h, p)| format!("{h}({p})")),
            'h' => s.and_then(|s| s.remote.as_ref()).map(|(h, _)| h.clone()),
            'L' => s.map(|_| "[local]".to_string()),
            'v' => s.map(|s| s.vxid.clone()),
            'x' => Some(s.map_or(0, |s| s.xid).to_string()),
            'Q' => Some(s.map_or(0, |s| s.query_id).to_string()),
            'q' if s.is_none() => break,
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
            location: Some((routine, file, line)),
        }
    }
}

/// Whether `sev` goes to the server log under `f` (log_min_messages).
pub fn wanted(f: &Format, sev: Severity) -> bool {
    active() && level_output(sev, &f.min_messages)
}

/// Write `m` (send_message_to_server_log), if log_min_messages wants it.
pub fn write(f: &Format, who: &Who, m: &Message) {
    let sev = m.severity.unwrap_or(Severity::Log);
    if !wanted(f, sev) {
        return;
    }
    let now = crate::types::datetime::Clock::system().now;
    let p = prefix(f, who, now, m.code);
    let tabs = |s: &str| s.replace('\n', "\n\t");
    let mut out = format!("{p}{}:  ", sev.as_str());
    if f.verbosity == Verbosity::Verbose {
        out.push_str(&format!("{}: ", m.code));
    }
    out.push_str(&tabs(m.text));
    out.push('\n');
    if f.verbosity >= Verbosity::Default {
        for (label, v) in [("DETAIL", m.detail), ("HINT", m.hint), ("CONTEXT", m.context)] {
            if let Some(v) = v {
                out.push_str(&format!("{p}{label}:  {}\n", tabs(v)));
            }
        }
    }
    if f.verbosity == Verbosity::Verbose
        && let Some((routine, file, line)) = &m.location
    {
        out.push_str(&format!("{p}LOCATION:  {routine}, {file}:{line}\n"));
    }
    if let Some(q) = m.statement
        && level_output(sev, &f.min_error_statement)
    {
        out.push_str(&format!("{p}STATEMENT:  {}\n", tabs(q)));
    }
    emit(&out);
}

/// A message of the postmaster (reloads, startup): its own process, under the configuration
/// files' settings `files` (None: the defaults).
pub fn postmaster(files: Option<&crate::sysconf::Loaded>, sev: Severity, text: &str) {
    if !active() {
        return;
    }
    let mut f = Format::postmaster();
    if let Some(l) = files {
        let get = |n: &str| l.values.get(n).map(|v| v.value.clone());
        if let Some(v) = get("log_line_prefix") {
            f.prefix = v;
        }
        if let Some(tz) = get("log_timezone").and_then(|v| TimeZone::parse(&v)) {
            f.tz = tz;
        }
        if let Some(v) = get("log_min_messages") {
            f.min_messages = v;
        }
    }
    let who = Who { pid: std::process::id() as i32, session: None, backend_type: "postmaster" };
    write(&f, &who, &Message { severity: Some(sev), ..Message::log(text) });
}
