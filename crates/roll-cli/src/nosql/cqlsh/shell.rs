//! The shell: input loop, statement dispatch, the cqlsh-only commands and result printing.
//!
//! Ported from Apache Cassandra's `pylib/cqlshlib/cqlshmain.py` (`Shell`, Apache-2.0). The
//! ScyllaDB image ships an older cqlsh (6.0); where its output differs (the ON/OFF switches,
//! `CAPTURE OFF`, durations, client-side DESCRIBE, HELP topics) [`Flavor::Scylla`] reproduces
//! what that cqlsh prints, learned from running it.

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::rc::Rc;

use bytes::Bytes;
use nosql_proto::{Consistency, QueryParams, ResultBody, Rows};
use nosql_types::CqlType;

use super::format::{self, Flavor, Fv, Opts};
use super::lex::{self, Kind, Token};
use super::render;
use super::session::{Conn, Failure, Target, py_repr};

/// Where output goes. Shared with SOURCE's sub-shells.
pub struct Io {
    pub stdout: Box<dyn Write>,
    pub stderr: Box<dyn Write>,
}

pub type SharedIo = Rc<RefCell<Io>>;

/// Lines of input.
pub enum Input {
    /// An interactive terminal.
    Tty(Box<super::complete::Editor>),
    /// A file or a pipe, with the name errors are prefixed with.
    Lines(Box<dyn BufRead>),
    /// `-e`.
    Single(String),
    /// `--tty` without a terminal: Python's `input(prompt)` (prompts written to stdout).
    Prompted(Box<dyn BufRead>),
}

pub const CQLSH_VERSION_CASSANDRA: &str = "6.2.0";
pub const CQLSH_VERSION_SCYLLA: &str = "6.0.34";
/// What Cassandra's cqlsh was built against (`check_build_versions`).
const BUILD_VERSION: &str = "5.0.9";
pub const DEFAULT_PAGE_SIZE: i32 = 100;

/// The versions `system.local` reports.
#[derive(Clone, Debug, Default)]
pub struct Versions {
    pub build: String,
    pub cql: String,
    pub protocol: u8,
    /// ScyllaDB's `system.versions` version.
    pub scylla: Option<String>,
    pub cluster_name: String,
}

pub struct Shell {
    pub conn: Conn,
    pub flavor: Flavor,
    pub opts: Opts,
    pub io: SharedIo,
    pub input: Input,
    pub username: Option<String>,
    pub keyspace: Option<String>,
    pub consistency: Consistency,
    pub serial: Consistency,
    pub tracing: bool,
    pub expand: bool,
    pub paging: bool,
    pub page_size: i32,
    pub tty: bool,
    pub show_line_nums: bool,
    pub lineno: usize,
    pub stdin_name: String,
    pub statement_error: bool,
    pub stop: bool,
    pub statement: String,
    pub prompt: String,
    pub empty_lines: usize,
    pub lastcmd: String,
    pub capture: Option<(String, std::fs::File)>,
    pub versions: Versions,
    pub max_trace_wait: f64,
    /// ScyllaDB's cqlsh describes from driver metadata unless it runs a single `-e` statement.
    pub client_describe: bool,
    pub history: Vec<String>,
    pub max_history_shown: usize,
    pub date_overflow_warned: bool,
    /// cqlsh's `decoding_errors`, reported after a result.
    pub decoding_errors: Vec<String>,
    pub meta: MetaTrack,
    /// `(column, kind)` of the current statement's table, for coloured headers.
    pub col_kinds: Vec<(String, String)>,
}

/// The driver's schema metadata as ScyllaDB's cqlsh sees it during an `-e` run: loaded in scan
/// order at connect, then extended in the order this session's schema changes arrived. Views
/// behind indexes created in the session never get an event, so they are not attached.
#[derive(Clone, Debug, Default)]
pub struct MetaTrack {
    pub at_start: std::collections::HashSet<(String, String)>,
    pub created_keyspaces: Vec<String>,
    pub created: Vec<(String, String)>,
}

impl MetaTrack {
    /// Keyspaces in metadata order, from the current scan order.
    pub fn keyspaces(&self, scan: Vec<String>) -> Vec<String> {
        let mut out: Vec<String> = scan.iter().filter(|k| !self.created_keyspaces.contains(k)).cloned().collect();
        out.extend(self.created_keyspaces.iter().filter(|k| scan.contains(k)).cloned());
        out
    }

    /// A keyspace's tables (or views) in metadata order, from the current scan order.
    pub fn tables(&self, ks: &str, scan: Vec<String>) -> Vec<String> {
        let created: Vec<&String> = self.created.iter().filter(|(k, _)| k == ks).map(|(_, t)| t).collect();
        let mut out: Vec<String> = scan.iter().filter(|t| !created.contains(t)).cloned().collect();
        out.extend(created.into_iter().filter(|t| scan.contains(t)).cloned());
        out
    }

    /// Whether the driver knows a view.
    pub fn knows_view(&self, ks: &str, view: &str) -> bool {
        let key = (ks.to_string(), view.to_string());
        self.at_start.contains(&key) || self.created.contains(&key)
    }

    pub fn apply(&mut self, sc: &nosql_proto::SchemaChange) {
        use nosql_proto::{SchemaChangeType as C, SchemaTarget as T};
        match (sc.change, sc.target, &sc.name) {
            (C::Created, T::Keyspace, _) => self.created_keyspaces.push(sc.keyspace.clone()),
            (C::Dropped, T::Keyspace, _) => {
                self.created_keyspaces.retain(|k| *k != sc.keyspace);
                self.created.retain(|(k, _)| *k != sc.keyspace);
                self.at_start.retain(|(k, _)| *k != sc.keyspace);
            }
            (C::Created, T::Table, Some(n)) => self.created.push((sc.keyspace.clone(), n.clone())),
            (C::Dropped, T::Table, Some(n)) => {
                let key = (sc.keyspace.clone(), n.clone());
                self.created.retain(|x| *x != key);
                self.at_start.remove(&key);
            }
            _ => {}
        }
    }
}

/// What a statement printed through, for tests and `CAPTURE`.
pub fn version_line(flavor: Flavor, v: &Versions) -> String {
    match flavor {
        Flavor::Cassandra => format!(
            "[cqlsh {CQLSH_VERSION_CASSANDRA} | Cassandra {} | CQL spec {} | Native protocol v{}]",
            v.build,
            full_cql_version(&v.cql),
            v.protocol
        ),
        Flavor::Scylla => format!(
            "[cqlsh {CQLSH_VERSION_SCYLLA} | Scylla {} | CQL spec {} | Native protocol v{}]",
            v.scylla.as_deref().unwrap_or(&v.build),
            full_cql_version(&v.cql),
            v.protocol
        ),
    }
}

/// `full_cql_version`: pads to three components.
pub fn full_cql_version(v: &str) -> String {
    let mut v = v.to_string();
    while v.matches('.').count() < 2 {
        v.push_str(".0");
    }
    v
}

/// Python's `OSError` text: `[Errno 2] No such file or directory: '/x'`.
pub fn os_error(e: &std::io::Error, path: &str) -> String {
    let msg = e.to_string();
    let msg = match msg.rfind(" (os error ") {
        Some(i) => msg[..i].to_string(),
        None => msg,
    };
    match e.raw_os_error() {
        Some(n) => format!("[Errno {n}] {msg}: {}", py_repr(path)),
        None => msg,
    }
}

pub fn expand_user(p: &str) -> String {
    if let Some(rest) = p.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}{rest}");
    }
    p.to_string()
}

/// A tiny matcher over a statement's tokens (its final `;` or newline dropped).
pub struct P<'a> {
    pub src: &'a str,
    pub toks: &'a [Token],
    pub i: usize,
}

impl<'a> P<'a> {
    pub fn new(src: &'a str, toks: &'a [Token]) -> Self {
        let n = if toks.last().is_some_and(|t| t.kind == Kind::Endtoken) { toks.len() - 1 } else { toks.len() };
        P { src, toks: &toks[..n], i: 0 }
    }

    pub fn peek(&self) -> Option<&'a Token> {
        self.toks.get(self.i)
    }

    pub fn text(&self) -> Option<&'a str> {
        self.peek().map(|t| t.text(self.src))
    }

    pub fn word(&mut self, w: &str) -> bool {
        match self.peek() {
            Some(t) if t.kind == Kind::Identifier && t.text(self.src).eq_ignore_ascii_case(w) => {
                self.i += 1;
                true
            }
            _ => false,
        }
    }

    pub fn op(&mut self, o: &str) -> bool {
        match self.peek() {
            Some(t) if t.kind != Kind::Identifier && t.text(self.src) == o => {
                self.i += 1;
                true
            }
            _ => false,
        }
    }

    pub fn kind(&mut self, k: Kind) -> Option<&'a str> {
        match self.peek() {
            Some(t) if t.kind == k => {
                self.i += 1;
                Some(t.text(self.src))
            }
            _ => None,
        }
    }

    /// `<cfOrKsName>`: an unreserved identifier or a quoted name.
    pub fn name(&mut self) -> Option<&'a str> {
        match self.peek() {
            Some(t) if (t.kind == Kind::Identifier && !t.reserved) || t.kind == Kind::QuotedName => {
                self.i += 1;
                Some(t.text(self.src))
            }
            _ => None,
        }
    }

    /// `[ks .] name`.
    pub fn qualified(&mut self) -> Option<(Option<&'a str>, &'a str)> {
        let save = self.i;
        let a = self.name()?;
        if self.op(".") {
            match self.name() {
                Some(b) => Some((Some(a), b)),
                None => {
                    self.i = save;
                    None
                }
            }
        } else {
            Some((None, a))
        }
    }

    pub fn done(&self) -> bool {
        self.i >= self.toks.len()
    }
}

impl Shell {
    pub fn print(&self, s: &str) {
        let mut io = self.io.borrow_mut();
        let _ = writeln!(io.stdout, "{s}");
    }

    pub fn print_raw(&self, s: &str) {
        let mut io = self.io.borrow_mut();
        let _ = write!(io.stdout, "{s}");
    }

    /// `writeresult`: query output, which CAPTURE redirects.
    pub fn writeresult(&mut self, s: &str) {
        self.write_query(&format!("{s}\n"));
    }

    pub fn write_query(&mut self, s: &str) {
        match &mut self.capture {
            Some((_, f)) => {
                let _ = f.write_all(s.as_bytes());
            }
            None => {
                let mut io = self.io.borrow_mut();
                let _ = io.stdout.write_all(s.as_bytes());
            }
        }
    }

    pub fn writeresult_colored(&mut self, s: &str, color: &str) {
        if self.opts.color && self.capture.is_none() {
            self.writeresult(&format!("{color}{s}{}", format::RESET));
        } else {
            self.writeresult(s);
        }
    }

    pub fn printerr(&mut self, s: &str) {
        self.statement_error = true;
        let text = if self.show_line_nums { format!("{}:{}:{s}", self.stdin_name, self.lineno) } else { s.to_string() };
        let text = if self.opts.color { format!("{}{text}{}", format::RED, format::RESET) } else { text };
        let mut io = self.io.borrow_mut();
        let _ = writeln!(io.stderr, "{text}");
        let _ = io.stderr.flush();
    }

    pub fn flush(&self) {
        let mut io = self.io.borrow_mut();
        let _ = io.stdout.flush();
    }

    pub fn reset_prompt(&mut self) {
        let base = match &self.keyspace {
            None => "cqlsh> ".to_string(),
            Some(ks) => format!("cqlsh:{ks}> "),
        };
        let custom = std::env::var("CQLSH_PROMPT").ok().filter(|p| !p.is_empty()).map(|p| format!("{p}\n")).unwrap_or_default();
        self.prompt = match &self.username {
            Some(u) => format!("{u}@{custom}{base}"),
            None => format!("{custom}{base}"),
        };
    }

    fn set_continue_prompt(&mut self) {
        if self.empty_lines >= 3 {
            self.prompt = "Statements are terminated with a ';'.  You can press CTRL-C to cancel an incomplete statement.".into();
            self.empty_lines = 0;
            return;
        }
        self.prompt = match &self.keyspace {
            None => "   ... ".into(),
            Some(ks) => format!("{}    ... ", " ".repeat(ks.chars().count())),
        };
        self.empty_lines = if self.lastcmd.is_empty() { self.empty_lines + 1 } else { 0 };
    }

    fn reset_statement(&mut self) {
        self.reset_prompt();
        self.statement.clear();
        self.empty_lines = 0;
    }

    /// The next input line with its newline; `None` at the end.
    fn input_line(&mut self) -> Option<String> {
        let line = match &mut self.input {
            Input::Tty(ed) => {
                let prompt = self.prompt.clone();
                match tokio::task::block_in_place(|| ed.readline(&prompt)) {
                    Ok(l) => {
                        self.lastcmd = l.clone();
                        format!("{l}\n")
                    }
                    Err(rustyline::error::ReadlineError::Interrupted) => {
                        self.reset_statement();
                        self.print("");
                        return Some(String::new());
                    }
                    Err(_) => return None,
                }
            }
            Input::Lines(r) => {
                let mut l = String::new();
                match r.read_line(&mut l) {
                    Ok(0) | Err(_) => return None,
                    Ok(_) => {
                        self.lastcmd = l.clone();
                        l
                    }
                }
            }
            Input::Prompted(_) => {
                let prompt = self.prompt.clone();
                self.print_raw(&prompt);
                self.flush();
                let Input::Prompted(r) = &mut self.input else { return None };
                let mut l = String::new();
                match r.read_line(&mut l) {
                    Ok(0) | Err(_) => return None,
                    Ok(_) => {
                        let l = l.trim_end_matches('\n').to_string();
                        self.lastcmd = l.clone();
                        format!("{l}\n")
                    }
                }
            }
            Input::Single(_) => return None,
        };
        self.lineno += 1;
        Some(line)
    }

    pub async fn cmdloop(&mut self) {
        while !self.stop {
            if matches!(self.input, Input::Tty(_)) {
                self.refresh_completion().await;
            }
            let single = matches!(self.input, Input::Single(_));
            let line = if let Input::Single(s) = &self.input {
                s.clone()
            } else {
                match self.input_line() {
                    Some(l) => l,
                    None => {
                        self.handle_eof().await;
                        break;
                    }
                }
            };
            if line.is_empty() && matches!(self.input, Input::Tty(_)) {
                continue;
            }
            self.statement.push_str(&line);
            let text = self.statement.clone();
            if Box::pin(self.onecmd(&text)).await {
                self.reset_statement();
            }
            if single {
                self.stop = true;
            }
        }
    }

    /// Hands the completer a fresh schema snapshot and the statement typed so far.
    async fn refresh_completion(&mut self) {
        let Input::Tty(ed) = &self.input else { return };
        let Some(h) = ed.helper() else { return };
        let schema = h.schema.clone();
        let keyspaces = super::describe::keyspace_names(self).await;
        let rows = |r: Vec<Vec<Option<String>>>| -> Vec<Vec<String>> {
            r.into_iter().map(|r| r.into_iter().map(Option::unwrap_or_default).collect()).collect()
        };
        let tables = rows(self.conn.strings("SELECT keyspace_name, table_name FROM system_schema.tables").await.unwrap_or_default());
        let columns =
            rows(self.conn.strings("SELECT keyspace_name, table_name, column_name FROM system_schema.columns").await.unwrap_or_default());
        let mut s = schema.lock().unwrap_or_else(|e| e.into_inner());
        s.keyspaces = keyspaces;
        s.tables = tables.into_iter().filter(|r| r.len() == 2).map(|r| (r[0].clone(), r[1].clone())).collect();
        s.columns = columns.into_iter().filter(|r| r.len() == 3).map(|r| (r[0].clone(), r[1].clone(), r[2].clone())).collect();
        s.current = self.keyspace.clone();
        s.pending = self.statement.clone();
    }

    async fn handle_eof(&mut self) {
        if self.tty {
            self.print("");
        }
        let text = self.statement.clone();
        if !text.trim().is_empty() && !Box::pin(self.onecmd(&text)).await {
            self.printerr("Incomplete statement at end of file");
        }
        self.stop = true;
    }

    /// Runs the complete statements in `text`; `false` when more input is needed.
    pub async fn onecmd(&mut self, text: &str) -> bool {
        let split = match lex::split(text) {
            Ok(s) => s,
            Err(e) => {
                if self.show_line_nums {
                    self.printerr(&format!("Invalid syntax at line {}, char {}", e.line, e.char));
                } else {
                    self.printerr(&format!("Invalid syntax at char {}", e.char));
                }
                let line = text.split('\n').nth(e.line - 1).unwrap_or("").to_string();
                self.printerr(&format!("  {line}"));
                self.printerr(&format!(" {}^", " ".repeat(e.char)));
                return true;
            }
        };
        let mut stmts = split.statements;
        while stmts.last().is_some_and(|s| s.is_empty()) {
            stmts.pop();
        }
        if stmts.is_empty() {
            return true;
        }
        if split.escaped || stmts.last().and_then(|s| s.last()).is_none_or(|t| t.kind != Kind::Endtoken) {
            self.set_continue_prompt();
            return false;
        }
        for st in stmts {
            self.handle_statement(&st, text).await;
            if self.stop {
                break;
            }
        }
        true
    }

    fn improper(&mut self, cmdword: &str) {
        self.printerr(&format!("Improper {cmdword} command."));
    }

    async fn handle_statement(&mut self, toks: &[Token], src: &str) {
        if let Input::Tty(ed) = &mut self.input {
            let nl = src.matches('\n').count();
            let hist = src.replace('\n', " ").trim_end().to_string();
            if nl > 1 && self.history.last() != Some(&hist) {
                let _ = ed.add_history_entry(hist.as_str());
            }
            self.history.push(hist);
        }
        let cmdword = toks[0].text(src);
        let lower = if cmdword == "?" { "help".to_string() } else { cmdword.to_lowercase() };
        let mut p = P::new(src, toks);
        p.i = 1;
        match lower.as_str() {
            "use" => match p.name() {
                Some(ks) if p.done() => {
                    let stmt = lex::extract(src, toks).to_string();
                    if self.perform_simple_statement(&stmt, None).await.is_some() {
                        self.keyspace = Some(lex::unprotect(ks));
                    }
                }
                _ => self.improper(cmdword),
            },
            "select" => {
                let was = self.tracing;
                if self.select_on_traces(&mut P::new(src, toks)) {
                    self.tracing = false;
                }
                self.perform_statement(lex::extract(src, toks)).await;
                self.tracing = was;
            }
            "describe" | "desc" => super::describe::do_describe(self, src, toks, cmdword).await,
            "copy" => super::copy::do_copy(self, src, toks, cmdword).await,
            "show" => self.do_show(&mut p, cmdword).await,
            "source" => match p.kind(Kind::QuotedString).or_else(|| p.kind(Kind::PgString)) {
                Some(f) if p.done() => self.do_source(&lex::dequote_value(f)).await,
                _ => self.improper(cmdword),
            },
            "capture" => self.do_capture(&mut p, cmdword),
            "tracing" | "expand" => match self.switch(&mut p, false) {
                Some(sw) => {
                    let (name, desc) = if lower == "tracing" { ("TRACING", "Tracing") } else { ("EXPAND", "Expanded output") };
                    let cur = if lower == "tracing" { self.tracing } else { self.expand };
                    let new = self.on_off(name, desc, cur, sw.as_deref());
                    if lower == "tracing" {
                        self.tracing = new;
                    } else {
                        self.expand = new;
                    }
                }
                None => self.improper(cmdword),
            },
            "paging" => match self.switch(&mut p, true) {
                Some(sw) => self.do_paging(sw.as_deref()),
                None => self.improper(cmdword),
            },
            "consistency" => match p.peek() {
                None => self.print(&format!("Current consistency level is {}.", self.consistency.name())),
                Some(t) => {
                    let l = t.text(src).to_uppercase();
                    p.i += 1;
                    match Consistency::from_name(&l) {
                        Some(c) if t.kind == Kind::Identifier && p.done() => {
                            self.consistency = c;
                            self.print(&format!("Consistency level set to {l}."));
                        }
                        _ => self.improper(cmdword),
                    }
                }
            },
            "serial" => {
                if !p.word("consistency") {
                    return self.improper(cmdword);
                }
                match p.text().map(str::to_uppercase) {
                    None => self.print(&format!("Current serial consistency level is {}.", self.serial.name())),
                    Some(l) if (l == "SERIAL" || l == "LOCAL_SERIAL") && {
                        p.i += 1;
                        p.done()
                    } =>
                    {
                        self.serial = Consistency::from_name(&l).unwrap_or(Consistency::Serial);
                        self.print(&format!("Serial consistency level set to {l}."));
                    }
                    Some(_) => self.improper(cmdword),
                }
            }
            "login" => self.do_login(&mut p, cmdword).await,
            "exit" | "quit" => {
                if p.done() {
                    self.stop = true;
                } else {
                    self.improper(cmdword);
                }
            }
            "clear" | "cls" => {
                if p.done() {
                    if self.tty {
                        self.print_raw("\x1b[H\x1b[2J");
                        self.flush();
                    }
                } else {
                    self.improper(cmdword);
                }
            }
            "debug" => {
                p.word("things");
                if !p.done() {
                    self.improper(cmdword);
                }
            }
            "help" => self.do_help(&mut p),
            "history" if self.flavor == Flavor::Cassandra => {
                let n = p.kind(Kind::WholeNumber);
                if !p.done() {
                    return self.improper(cmdword);
                }
                if !matches!(self.input, Input::Tty(_)) {
                    return self.printerr("'NoneType' object has no attribute 'get_current_history_length'");
                }
                if let Some(n) = n {
                    self.max_history_shown = n.parse().unwrap_or(50);
                }
                let len = self.history.len();
                let from = len.saturating_sub(self.max_history_shown).max(1);
                let shown: Vec<String> = self.history.get(from - 1..len.saturating_sub(1)).unwrap_or_default().to_vec();
                for h in shown {
                    self.print(&h);
                }
            }
            "unicode" => self.improper(cmdword),
            _ => {
                self.perform_statement(lex::extract(src, toks)).await;
            }
        }
    }

    /// `SELECT … FROM system_traces.x`, or unqualified while in `system_traces`: not traced.
    fn select_on_traces(&self, p: &mut P) -> bool {
        while let Some(t) = p.peek() {
            p.i += 1;
            if t.kind == Kind::Identifier && t.text(p.src).eq_ignore_ascii_case("from") {
                return match p.qualified() {
                    Some((Some(ks), _)) => lex::unprotect(ks) == "system_traces",
                    Some((None, _)) => self.keyspace.as_deref() == Some("system_traces"),
                    None => false,
                };
            }
        }
        false
    }

    /// `( ON | OFF | <wholenumber> )?`; `None` when it does not parse.
    fn switch(&self, p: &mut P, number: bool) -> Option<Option<String>> {
        if p.done() {
            return Some(None);
        }
        let v = if p.word("on") {
            "ON".to_string()
        } else if p.word("off") {
            "OFF".to_string()
        } else if number && let Some(n) = p.kind(Kind::WholeNumber) {
            n.to_string()
        } else {
            return None;
        };
        p.done().then_some(Some(v))
    }

    /// TRACING / EXPAND / PAGING switches, worded as each cqlsh words them.
    fn on_off(&mut self, name: &str, desc: &str, current: bool, state: Option<&str>) -> bool {
        let onoff = |b: bool| if b { "ON" } else { "OFF" };
        match self.flavor {
            Flavor::Cassandra => match state {
                None => {
                    self.print(&format!("{name} is {}", onoff(current)));
                    current
                }
                Some(s) => {
                    let new = s == "ON";
                    if new == current {
                        self.print(&format!("{name} is already {}", onoff(current)));
                    } else {
                        self.print(&format!("{name} set to {}", onoff(new)));
                    }
                    new
                }
            },
            Flavor::Scylla => match state {
                None => {
                    if current {
                        self.print(&format!("{desc} is currently enabled. Use {name} OFF to disable"));
                    } else {
                        self.print(&format!("{desc} is currently disabled. Use {name} ON to enable."));
                    }
                    current
                }
                Some("ON") => {
                    if current {
                        self.printerr(&format!("{desc} is already enabled. Use {name} OFF to disable."));
                    } else {
                        self.print(&format!("Now {desc} is enabled"));
                    }
                    true
                }
                Some(_) => {
                    if current {
                        self.print(&format!("Disabled {desc}."));
                    } else {
                        self.printerr(&format!("{desc} is not enabled."));
                    }
                    false
                }
            },
        }
    }

    fn do_paging(&mut self, sw: Option<&str>) {
        let (on, size) = match sw {
            Some(n) if n.chars().all(|c| c.is_ascii_digit()) => (true, n.parse::<i32>().ok()),
            other => (self.on_off("PAGING", "Query paging", self.paging, other), None),
        };
        self.paging = on;
        if let Some(s) = size {
            self.page_size = s;
        }
        if self.paging {
            self.print(&format!("Page size: {}", self.page_size));
        } else {
            self.page_size = DEFAULT_PAGE_SIZE;
        }
    }

    fn do_capture(&mut self, p: &mut P<'_>, cmdword: &str) {
        let fname = p.kind(Kind::QuotedString).or_else(|| p.kind(Kind::PgString));
        let off = fname.is_none() && p.word("off");
        if !p.done() {
            return self.improper(cmdword);
        }
        // Cassandra's grammar binds `fname` only to the string, so CAPTURE OFF reports status.
        let off = off && self.flavor == Flavor::Scylla;
        if fname.is_none() && !off {
            match &self.capture {
                Some((name, _)) => self.print(&format!("Currently capturing query output to {}.", py_repr(name))),
                None => self.print("Currently not capturing query output."),
            }
            return;
        }
        if off {
            if self.capture.take().is_none() {
                self.printerr("Not currently capturing output.");
            }
            return;
        }
        if let Some((name, _)) = &self.capture {
            let msg = format!("Already capturing output to {name}. Use CAPTURE OFF to disable.");
            return self.printerr(&msg);
        }
        let name = expand_user(&lex::dequote_value(fname.unwrap_or_default()));
        match std::fs::OpenOptions::new().append(true).create(true).open(&name) {
            Ok(f) => {
                self.flush();
                self.print(&format!("Now capturing query output to {}.", py_repr(&name)));
                self.capture = Some((name, f));
            }
            Err(e) => {
                let msg = format!("Could not open {} for append: {}", py_repr(&name), os_error(&e, &name));
                self.printerr(&msg);
            }
        }
    }

    fn do_help(&mut self, p: &mut P) {
        let mut topics = vec![];
        while let Some(t) = p.peek() {
            p.i += 1;
            topics.push(t.text(p.src).to_string());
        }
        if topics.is_empty() {
            let index = match self.flavor {
                Flavor::Cassandra => include_str!("help/index_cassandra.txt"),
                Flavor::Scylla => include_str!("help/index_scylla.txt"),
            };
            self.print_raw(index);
            return;
        }
        for t in topics {
            match super::help::shell_topic(self.flavor, &t) {
                Some(doc) => self.print_raw(doc),
                None => match super::help::cql_topic(&t) {
                    Some(anchor) => {
                        let url = match self.flavor {
                            Flavor::Cassandra => "https://cassandra.apache.org/doc/latest/cassandra/cql/cql_singlefile.html",
                            Flavor::Scylla => "https://cassandra.apache.org/doc/latest/cql/index.html",
                        };
                        self.printerr(&format!("*** No browser to display CQL help. URL for help topic {t} : {url}#{anchor}"));
                    }
                    None => self.printerr(&format!("*** No help on {t}")),
                },
            }
        }
    }

    async fn do_show(&mut self, p: &mut P<'_>, cmdword: &str) {
        if p.word("version") && p.done() {
            let _ = self.load_versions().await;
            self.print(&version_line(self.flavor, &self.versions));
        } else if p.word("host") && p.done() {
            self.show_host();
        } else if p.word("session")
            && let Some(id) = p.kind(Kind::Uuid)
            && p.done()
        {
            let id = id.to_lowercase();
            match nosql_types::timeuuid::parse_uuid(&id) {
                Some(u) => super::trace::show_session(self, u, false).await,
                None => self.improper(cmdword),
            }
        } else if p.word("replicas") {
            p.op("-");
            if p.kind(Kind::WholeNumber).is_none() {
                return self.improper(cmdword);
            }
            let _ = p.name();
            if !p.done() {
                return self.improper(cmdword);
            }
            let addrs = [self.rpc_address().await];
            self.print(&format!("[{}]", addrs.iter().map(|a| py_repr(a)).collect::<Vec<_>>().join(", ")));
        } else {
            self.improper(cmdword);
        }
    }

    pub async fn rpc_address(&self) -> String {
        let q = "SELECT rpc_address FROM system.local WHERE key = 'local'";
        match self.conn.all_rows(q, Consistency::One).await {
            Ok(Some(rows)) => rows
                .rows()
                .next()
                .and_then(|mut r| r.next().flatten())
                .and_then(|b| nosql_types::inet::format_inet(&b, nosql_types::Dialect::Scylla))
                .unwrap_or_else(|| self.conn.target.host.clone()),
            _ => self.conn.target.host.clone(),
        }
    }

    pub fn show_host(&self) {
        let name = if self.opts.color {
            format!("{}{}{}", format::BLUE, self.versions.cluster_name, format::RESET)
        } else {
            self.versions.cluster_name.clone()
        };
        self.print(&format!("Connected to {name} at {}:{}", self.conn.target.host, self.conn.target.port));
    }

    /// `get_connection_versions` (and the cluster name the driver's metadata holds).
    pub async fn load_versions(&mut self) -> Result<(), Failure> {
        let rows = self.conn.all_rows("select * from system.local where key = 'local'", Consistency::One).await?;
        if let Some(rows) = rows
            && let Some(row) = rows.to_vec().into_iter().next()
        {
            let get = |n: &str| super::session::cell(&rows, &row, n).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
            self.versions.build = get("release_version");
            self.versions.cql = get("cql_version");
            self.versions.cluster_name = get("cluster_name");
        }
        self.versions.protocol = self.conn.client.version().as_u8();
        if self.flavor == Flavor::Scylla
            && let Ok(r) = self.conn.strings("SELECT version FROM system.versions WHERE key = 'local'").await
        {
            self.versions.scylla = r.into_iter().next().and_then(|r| r.into_iter().next().flatten());
        }
        Ok(())
    }

    /// Cassandra's cqlsh warns when the server is not the version it was built for.
    pub fn check_build_versions(&self) {
        if self.flavor != Flavor::Cassandra {
            return;
        }
        let base = match self.versions.build.rfind('-') {
            Some(i) => &self.versions.build[..i],
            None => &self.versions.build,
        };
        if base != BUILD_VERSION {
            self.print(&format!(
                "WARNING: cqlsh was built against {BUILD_VERSION}, but this server is {base}.  All features may not work!"
            ));
        }
    }

    async fn do_source(&mut self, fname: &str) {
        let path = expand_user(fname);
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                let msg = format!("Could not open {}: {}", py_repr(&path), os_error(&e, &path));
                return self.printerr(&msg);
            }
        };
        let before = self.keyspace.clone();
        let mut sub = Shell {
            conn: self.conn.clone(),
            flavor: self.flavor,
            opts: self.opts.clone(),
            io: self.io.clone(),
            input: Input::Lines(Box::new(std::io::BufReader::new(skip_bom(file)))),
            username: self.username.clone(),
            keyspace: self.keyspace.clone(),
            consistency: Consistency::One,
            serial: Consistency::Serial,
            tracing: self.tracing,
            expand: false,
            paging: true,
            page_size: DEFAULT_PAGE_SIZE,
            tty: false,
            show_line_nums: true,
            lineno: 1,
            stdin_name: path,
            statement_error: false,
            stop: false,
            statement: String::new(),
            prompt: String::new(),
            empty_lines: 0,
            lastcmd: String::new(),
            capture: None,
            versions: self.versions.clone(),
            max_trace_wait: self.max_trace_wait,
            client_describe: false,
            history: vec![],
            max_history_shown: 50,
            date_overflow_warned: self.date_overflow_warned,
            decoding_errors: vec![],
            meta: MetaTrack::default(),
            col_kinds: vec![],
        };
        Box::pin(sub.cmdloop()).await;
        self.flush();
        // The sub-shell had a session of its own: put this one's keyspace back.
        if sub.keyspace != before
            && let Some(ks) = &before
        {
            let _ = self.conn.query(&format!("USE {}", lex::protect_name(ks)), QueryParams::default(), false).await;
        }
    }

    async fn do_login(&mut self, p: &mut P<'_>, cmdword: &str) {
        let user = match p.peek() {
            Some(t) if matches!(t.kind, Kind::Identifier | Kind::QuotedName | Kind::QuotedString) => {
                p.i += 1;
                let s = t.text(p.src);
                match t.kind {
                    Kind::QuotedString => lex::dequote_value(s),
                    Kind::QuotedName => lex::dequote_name(s),
                    _ => s.to_string(),
                }
            }
            _ => return self.improper(cmdword),
        };
        let pass = p.kind(Kind::QuotedString).map(|s| s[1..s.len() - 1].to_string());
        if !p.done() {
            return self.improper(cmdword);
        }
        let pass = match pass {
            Some(p) => p,
            None => super::read_password(),
        };
        let mut target = (*self.conn.target).clone();
        target.credentials = Some((user.clone(), pass));
        target.protocol = Some(self.conn.client.version());
        match target.connect().await {
            Ok(client) => {
                let conn = Conn { client, target: std::sync::Arc::new(target) };
                if let Some(ks) = &self.keyspace
                    && let Err(f) = conn.query(&format!("USE {}", lex::protect_name(ks)), QueryParams::default(), false).await
                {
                    return self.printerr(&super::connect_error(&conn.target, &f.line()));
                }
                self.conn = conn;
                self.username = Some(user);
            }
            Err(e) => {
                let msg = super::connect_failure(&target, &e);
                self.printerr(&msg);
            }
        }
    }

    pub fn params(&self, paging: bool) -> QueryParams {
        let mut p = QueryParams::new(self.consistency);
        p.serial_consistency = Some(self.serial);
        if paging && self.paging {
            p.page_size = Some(self.page_size);
        }
        p
    }

    pub async fn perform_statement(&mut self, stmt: &str) -> bool {
        let params = self.params(true);
        let Some(exec) = self.perform_simple_statement(stmt, Some(params)).await else { return false };
        if !exec.warnings.is_empty() {
            self.print_warnings(&exec.warnings);
        }
        if self.tracing {
            for id in exec.trace_ids {
                super::trace::print_query_trace(self, id).await;
            }
        }
        true
    }

    pub fn print_warnings(&mut self, warnings: &[String]) {
        self.writeresult("");
        self.writeresult("Warnings :");
        for w in warnings {
            self.writeresult(w);
            self.writeresult("");
        }
    }

    /// Sends a statement and prints its result; `None` when it failed.
    pub async fn perform_simple_statement(&mut self, stmt: &str, params: Option<QueryParams>) -> Option<Exec> {
        if stmt.is_empty() {
            return None;
        }
        let params = params.unwrap_or_else(|| self.params(false));
        let reply = match self.conn.query(stmt, params.clone(), self.tracing).await {
            Ok(r) => r,
            Err(f) => {
                self.printerr(&f.line());
                return None;
            }
        };
        let mut exec = Exec { warnings: reply.warnings, trace_ids: reply.tracing_id.into_iter().collect() };
        let lower: String = stmt.chars().take(10).collect::<String>().to_lowercase();
        self.load_col_kinds(stmt).await;
        let rows = match reply.body {
            ResultBody::Rows(r) => Some(r),
            ResultBody::SchemaChange(sc) => {
                self.meta.apply(&sc);
                None
            }
            _ => None,
        };
        if lower.starts_with("select") || lower.starts_with("list") {
            if let Some(rows) = rows
                && let Err(f) = self.print_result(stmt, rows, params, &mut exec).await
            {
                self.printerr(&f.line());
            }
        } else if let Some(rows) = rows
            && !rows.is_empty()
        {
            self.writeresult("");
            self.print_static_result(&rows, true, self.tty, 0);
        }
        self.flush();
        Some(exec)
    }

    async fn print_result(&mut self, stmt: &str, first: Rows, mut params: QueryParams, exec: &mut Exec) -> Result<(), Failure> {
        self.decoding_errors.clear();
        self.writeresult("");
        let mut rows = first;
        let mut num = 0;
        let mut is_first = true;
        loop {
            if !rows.is_empty() || is_first {
                let with_header = is_first || self.tty;
                self.print_static_result(&rows, with_header, self.tty, num);
                num += rows.len();
            }
            match rows.metadata.paging_state.clone() {
                Some(state) => {
                    if self.capture.is_none() && self.tty && !self.more_prompt() {
                        return Err(Failure::Client("EOF when reading a line".into()));
                    }
                    params.paging_state = Some(state);
                    let r = self.conn.query(stmt, params.clone(), self.tracing).await?;
                    exec.warnings = r.warnings;
                    exec.trace_ids.extend(r.tracing_id);
                    match r.body {
                        ResultBody::Rows(r) => rows = r,
                        _ => break,
                    }
                }
                None => {
                    if !self.tty {
                        self.writeresult("");
                    }
                    break;
                }
            }
            is_first = false;
        }
        self.writeresult(&format!("({num} rows)"));
        let errs = std::mem::take(&mut self.decoding_errors);
        for e in errs.iter().take(2) {
            self.writeresult_colored(e, format::RED);
        }
        if errs.len() > 2 {
            self.writeresult_colored(&format!("{} more decoding errors suppressed.", errs.len() - 2), format::RED);
        }
        Ok(())
    }

    /// `input("---MORE---")`; `false` at the end of input.
    fn more_prompt(&mut self) -> bool {
        self.flush();
        match &mut self.input {
            Input::Tty(ed) => tokio::task::block_in_place(|| ed.readline("---MORE---")).is_ok(),
            Input::Prompted(_) => {
                self.print_raw("---MORE---");
                self.flush();
                let Input::Prompted(r) = &mut self.input else { return false };
                let mut l = String::new();
                matches!(r.read_line(&mut l), Ok(n) if n > 0)
            }
            _ => true,
        }
    }

    pub fn column_names(&self, rows: &Rows) -> Vec<Fv> {
        let kind = |n: &str| self.col_kinds.iter().find(|(c, _)| c == n).map(|(_, k)| k.as_str()).unwrap_or("");
        rows.metadata.columns.iter().map(|c| format::colname_as(&c.name, kind(&c.name), &self.opts)).collect()
    }

    /// With colour on, the key roles of the columns of the statement's table (`myformat_colname`).
    async fn load_col_kinds(&mut self, stmt: &str) {
        self.col_kinds.clear();
        if !self.opts.color {
            return;
        }
        let Ok(toks) = lex::lex(stmt) else { return };
        let mut p = P::new(stmt, &toks);
        let mut target = None;
        while let Some(t) = p.peek() {
            p.i += 1;
            if t.kind == Kind::Identifier && ["from", "into", "update"].iter().any(|w| t.text(stmt).eq_ignore_ascii_case(w)) {
                target = p.qualified();
                break;
            }
        }
        let Some((ks, table)) = target else { return };
        let Some(ks) = ks.map(lex::unprotect).or(self.keyspace.clone()) else { return };
        let q = format!(
            "SELECT column_name, kind FROM system_schema.columns WHERE keyspace_name = '{}' AND table_name = '{}'",
            ks.replace('\'', "''"),
            lex::unprotect(table).replace('\'', "''")
        );
        for r in self.conn.strings(&q).await.unwrap_or_default() {
            if let [Some(c), Some(k)] = &r[..] {
                self.col_kinds.push((c.clone(), k.clone()));
            }
        }
    }

    pub fn print_static_result(&mut self, rows: &Rows, with_header: bool, tty: bool, offset: usize) {
        if rows.metadata.columns.is_empty() {
            return;
        }
        let names = self.column_names(rows);
        let color = self.opts.color && self.capture.is_none();
        if rows.is_empty() {
            let out = render::table(&names, None, true, tty, color);
            self.write_query(&out);
            return;
        }
        let types: Vec<CqlType> = rows.metadata.columns.iter().map(|c| format::cql_type(&c.ty)).collect();
        let values: Vec<Vec<Fv>> = rows
            .rows()
            .map(|r| r.enumerate().map(|(i, c)| self.format_cell(&types[i], c)).collect())
            .collect();
        let out = if self.expand {
            render::vertical(&names, &values, offset, color)
        } else {
            render::table(&names, Some(&values), with_header, tty, color)
        };
        self.write_query(&out);
    }

    fn format_cell(&mut self, ty: &CqlType, c: Option<Bytes>) -> Fv {
        if !self.date_overflow_warned
            && *ty == CqlType::Timestamp
            && let Some(b) = &c
            && let Ok(a) = <[u8; 8]>::try_from(&b[..])
        {
            let ms = i64::from_be_bytes(a);
            if !(-62_135_596_800_000..=253_402_300_799_999).contains(&ms) {
                self.date_overflow_warned = true;
                let mut io = self.io.borrow_mut();
                let _ = writeln!(
                    io.stderr,
                    "cqlsh: DateOverFlowWarning: Some timestamps are larger than Python datetime can represent. Timestamps are displayed in milliseconds from epoch."
                );
            }
        }
        if let Some(e) = format::format_error(ty, c.as_deref(), &self.opts) {
            self.decoding_errors.push(e);
        }
        format::format(ty, c.as_deref(), &self.opts)
    }
}

/// What a statement left behind for `perform_statement`.
pub struct Exec {
    /// The last page's warnings (the driver's future keeps only those).
    pub warnings: Vec<String>,
    pub trace_ids: Vec<[u8; 16]>,
}

/// Opens a file the way cqlsh does: a UTF-8 byte order mark is skipped.
pub fn skip_bom(f: std::fs::File) -> Box<dyn std::io::Read> {
    use std::io::Read;
    let mut r = std::io::BufReader::new(f);
    let skip = matches!(r.fill_buf(), Ok(b) if b.starts_with(&[0xef, 0xbb, 0xbf]));
    if skip {
        r.consume(3);
    }
    Box::new(r.take(u64::MAX))
}

#[allow(dead_code)]
pub fn new_target(host: &str, port: u16) -> Target {
    Target {
        host: host.into(),
        port,
        credentials: None,
        tls: None,
        protocol: None,
        connect_timeout: std::time::Duration::from_secs(5),
        request_timeout: std::time::Duration::from_secs(10),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(full_cql_version("3.4"), "3.4.0");
        assert_eq!(full_cql_version("3.4.7"), "3.4.7");
        let v = Versions { build: "5.0.9".into(), cql: "3.4.7".into(), protocol: 5, scylla: None, cluster_name: "Test Cluster".into() };
        assert_eq!(version_line(Flavor::Cassandra, &v), "[cqlsh 6.2.0 | Cassandra 5.0.9 | CQL spec 3.4.7 | Native protocol v5]");
        let v = Versions { build: "3.0.8".into(), cql: "3.3.1".into(), protocol: 4, scylla: Some("2026.3.3".into()), cluster_name: String::new() };
        assert_eq!(version_line(Flavor::Scylla, &v), "[cqlsh 6.0.34 | Scylla 2026.3.3 | CQL spec 3.3.1 | Native protocol v4]");
        let e = std::fs::File::open("/nonexistent/x").unwrap_err();
        assert_eq!(os_error(&e, "/nonexistent/x"), "[Errno 2] No such file or directory: '/nonexistent/x'");
    }

    #[test]
    fn token_matcher() {
        let src = "USE \"Ks\";";
        let toks = lex::lex(src).unwrap();
        let mut p = P::new(src, &toks);
        assert!(p.word("use"));
        assert_eq!(p.name(), Some("\"Ks\""));
        assert!(p.done());
        let src = "x ks.t y";
        let toks = lex::lex(src).unwrap();
        let mut p = P::new(src, &toks);
        p.i = 1;
        assert_eq!(p.qualified(), Some((Some("ks"), "t")));
        let src = "USE select";
        let toks = lex::lex(src).unwrap();
        let mut p = P::new(src, &toks);
        p.i = 1;
        assert_eq!(p.name(), None);
    }
}
