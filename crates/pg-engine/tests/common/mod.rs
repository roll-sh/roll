//! Shared test helpers: an in-process engine session, a wire client for the reference
//! Postgres, and the golden-file rendering both sides are compared through.

#![allow(dead_code)]

pub mod pgclient;

use pg_engine::session::Session;
use pg_engine::session::state::{SessionState, SingleCluster};
use pg_engine::storage::cache::BlockCache;
use pg_engine::storage::{Store, StoreOptions};
use pg_engine::txn::Database;
use pg_engine::types::text::format_value;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Field {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub position: Option<u32>,
    /// CONTEXT: where it happened (one line per level).
    pub context: Option<String>,
    /// The internal query and the position in it.
    pub internal: Option<(u32, String)>,
    /// SCHEMA / TABLE / COLUMN / DATATYPE / CONSTRAINT NAME, as `schema=x` …
    pub objects: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Notice(Field),
    Error(Field),
    Result {
        cols: Option<Vec<(String, u32)>>,
        rows: Vec<Vec<Option<String>>>,
        tag: String,
    },
}

/// A small store: tests must not take much memory.
pub fn open_store(dir: &Path) -> Arc<Store> {
    let opts = StoreOptions {
        memtable_bytes: 2 << 20,
        sync: false,
        cache: BlockCache::new(4 << 20),
        ..StoreOptions::default()
    };
    Store::open(dir, opts).expect("open store")
}

pub fn engine_session(store: Arc<Store>, dbname: &str) -> Session {
    let db = Database::open(dbname, 16384, store, "postgres").expect("open database");
    let cluster = Arc::new(SingleCluster {
        name: dbname.to_string(),
        started: 0,
    });
    Session::new(SessionState::new(db, cluster, "postgres", 4242))
}

/// A statement and, for COPY FROM STDIN, its data (the lines up to `\.`).
pub fn split_copy(stmt: &str) -> (&str, Option<String>) {
    match stmt.split_once("\n\\copy-data\n") {
        Some((sql, data)) => (sql, Some(data.to_string())),
        None => (stmt, None),
    }
}

/// Run one statement on the engine, reporting what a client would see.
pub fn engine_query(s: &mut Session, stmt: &str) -> Vec<Event> {
    let (sql, data) = split_copy(stmt);
    let r = s.query_copy(sql, data.as_deref().unwrap_or("").as_bytes());
    let mut events: Vec<Event> = s
        .notices
        .drain(..)
        .map(|n| Event::Notice(field(&n)))
        .collect();
    match r {
        Ok(results) => {
            let fmt = s.format_ctx();
            for q in results {
                let (cols, rows) = match &q.cols {
                    Some(cols) => (
                        Some(
                            cols.iter()
                                .map(|c| (c.name.clone(), c.type_oid()))
                                .collect(),
                        ),
                        q.rows
                            .iter()
                            .map(|r| {
                                r.iter()
                                    .zip(cols)
                                    .map(|(v, c)| {
                                        if v.is_null() {
                                            None
                                        } else {
                                            Some(format_value(v, &c.ty, &fmt))
                                        }
                                    })
                                    .collect()
                            })
                            .collect(),
                    ),
                    // COPY TO STDOUT: one text row per line.
                    None if !q.rows.is_empty() => (
                        Some(vec![("copy".to_string(), 25)]),
                        q.rows
                            .iter()
                            .map(|r| {
                                vec![match &r[0] {
                                    pg_engine::types::Value::Text(t) => {
                                        Some(t.trim_end_matches('\n').to_string())
                                    }
                                    _ => None,
                                }]
                            })
                            .collect(),
                    ),
                    None => (None, Vec::new()),
                };
                events.push(Event::Result {
                    cols,
                    rows,
                    tag: q.tag,
                });
            }
        }
        Err(e) => events.push(Event::Error(field(&e))),
    }
    events
}

fn field(e: &pg_engine::error::Error) -> Field {
    Field {
        severity: e.severity.to_string(),
        code: e.code.to_string(),
        message: e.message.clone(),
        detail: e.detail.clone(),
        hint: e.hint.as_deref().map(String::from),
        position: e.position,
        context: e.context.as_deref().map(String::from),
        internal: e.objects.as_ref().and_then(|o| o.internal.clone()),
        objects: objects(e.objects.as_deref()),
    }
}

/// A fastpath function call (the FunctionCall message) in a test file:
///
///     \fcall[b] <oid or regprocedure> [<arg> ...];
///
/// An argument is `NULL`, `'text'` (text format), or binary: `i4:<n>`, `i8:<n>` or
/// `x:<hex>`. `\fcallb` asks for the result in binary (shown as hex). A function given by
/// signature is looked up with a query first.
pub struct Fcall {
    pub binary: bool,
    pub target: String,
    pub args: Vec<FcArg>,
}

#[derive(Clone, Debug)]
pub enum FcArg {
    Null,
    Text(String),
    Bin(Vec<u8>),
}

impl FcArg {
    pub fn format(&self) -> i16 {
        matches!(self, FcArg::Bin(_)) as i16
    }

    pub fn bytes(&self) -> Option<Vec<u8>> {
        match self {
            FcArg::Null => None,
            FcArg::Text(t) => Some(t.as_bytes().to_vec()),
            FcArg::Bin(b) => Some(b.clone()),
        }
    }
}

pub fn parse_fcall(stmt: &str) -> Option<Fcall> {
    let rest = stmt.trim().strip_prefix("\\fcall")?;
    let (binary, rest) = match rest.strip_prefix('b') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let mut tokens = Vec::new();
    let mut chars = rest.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut t = String::new();
        if c == '\'' {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        break;
                    }
                }
                t.push(c);
            }
            tokens.push((true, t));
            continue;
        }
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            t.push(c);
            chars.next();
        }
        tokens.push((false, t));
    }
    let mut it = tokens.into_iter();
    let target = it.next()?.1;
    let args = it
        .map(|(quoted, t)| {
            if quoted {
                return FcArg::Text(t);
            }
            if t == "NULL" {
                FcArg::Null
            } else if let Some(n) = t.strip_prefix("i4:") {
                FcArg::Bin(n.parse::<i32>().unwrap().to_be_bytes().to_vec())
            } else if let Some(n) = t.strip_prefix("i8:") {
                FcArg::Bin(n.parse::<i64>().unwrap().to_be_bytes().to_vec())
            } else if let Some(h) = t.strip_prefix("x:") {
                FcArg::Bin(
                    (0..h.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
                        .collect(),
                )
            } else {
                panic!("bad fastpath argument {t}")
            }
        })
        .collect();
    Some(Fcall {
        binary,
        target,
        args,
    })
}

impl Fcall {
    /// The query looking up the function's oid (None: given as a number).
    pub fn lookup_sql(&self) -> Option<String> {
        if self.target.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(format!(
            "SELECT '{}'::regprocedure::oid",
            self.target.replace('\'', "''")
        ))
    }

    pub fn result(&self, v: Option<Vec<u8>>) -> Event {
        let v = v.map(|b| {
            if self.binary {
                format!(
                    "\\x{}",
                    b.iter().map(|x| format!("{x:02x}")).collect::<String>()
                )
            } else {
                String::from_utf8_lossy(&b).into_owned()
            }
        });
        Event::Result {
            cols: Some(vec![("result".into(), 25)]),
            rows: vec![vec![v]],
            tag: "FASTPATH".into(),
        }
    }
}

/// The oid a lookup query's events carry, or the events to report instead.
pub fn fcall_oid(fc: &Fcall, lookup: Option<Vec<Event>>) -> Result<u32, Vec<Event>> {
    let Some(events) = lookup else {
        return Ok(fc.target.parse().unwrap());
    };
    match events.iter().find_map(|e| match e {
        Event::Result { rows, .. } => rows.first().and_then(|r| r.first().cloned().flatten()),
        _ => None,
    }) {
        Some(oid) => Ok(oid.parse().unwrap()),
        None => Err(events),
    }
}

/// Make a fastpath call on the engine, reporting what a client would see.
pub fn engine_fcall(s: &mut Session, fc: &Fcall) -> Vec<Event> {
    let oid = match fcall_oid(fc, fc.lookup_sql().map(|q| engine_query(s, &q))) {
        Ok(oid) => oid,
        Err(events) => return events,
    };
    let formats: Vec<i16> = fc.args.iter().map(FcArg::format).collect();
    let args = fc.args.iter().map(FcArg::bytes).collect();
    let r = s
        .fastpath_function(oid)
        .and_then(|f| s.fastpath_call(&f, &formats, args, fc.binary as i16));
    let mut events: Vec<Event> = s
        .notices
        .drain(..)
        .map(|n| Event::Notice(field(&n)))
        .collect();
    events.push(match r {
        Ok(v) => fc.result(v),
        Err(e) => Event::Error(field(&e)),
    });
    events
}

fn objects(o: Option<&pg_engine::error::Objects>) -> Vec<String> {
    let Some(o) = o else { return Vec::new() };
    [
        ("schema", &o.schema),
        ("table", &o.table),
        ("column", &o.column),
        ("datatype", &o.datatype),
        ("constraint", &o.constraint),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={v}")))
    .collect()
}

fn type_name(oid: u32) -> String {
    let n = match oid {
        16 => "bool",
        17 => "bytea",
        18 => "char",
        19 => "name",
        20 => "int8",
        21 => "int2",
        23 => "int4",
        24 => "regproc",
        25 => "text",
        26 => "oid",
        114 => "json",
        142 => "xml",
        1790 => "refcursor",
        700 => "float4",
        701 => "float8",
        790 => "money",
        869 => "inet",
        650 => "cidr",
        829 => "macaddr",
        1042 => "bpchar",
        1043 => "varchar",
        1082 => "date",
        1083 => "time",
        1114 => "timestamp",
        1184 => "timestamptz",
        1186 => "interval",
        1266 => "timetz",
        1560 => "bit",
        1562 => "varbit",
        1700 => "numeric",
        2205 => "regclass",
        2206 => "regtype",
        2249 => "record",
        2278 => "void",
        2950 => "uuid",
        3802 => "jsonb",
        3220 => "pg_lsn",
        705 => "unknown",
        1000 => "bool[]",
        1001 => "bytea[]",
        1005 => "int2[]",
        1007 => "int4[]",
        1016 => "int8[]",
        1009 => "text[]",
        1015 => "varchar[]",
        1021 => "float4[]",
        1022 => "float8[]",
        1231 => "numeric[]",
        1115 => "timestamp[]",
        1185 => "timestamptz[]",
        1182 => "date[]",
        2951 => "uuid[]",
        3807 => "jsonb[]",
        199 => "json[]",
        1028 => "oid[]",
        1003 => "name[]",
        1014 => "bpchar[]",
        o if o >= 16384 => "user",
        o => return o.to_string(),
    };
    n.to_string()
}

thread_local! {
    static RAW_ORDER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a test file asks for its rows in the order the server sent them (a line
/// `-- golden: raw-order`): results without ORDER BY are otherwise compared sorted.
pub fn wants_raw_order(src: &str) -> bool {
    src.lines().any(|l| l.trim() == "-- golden: raw-order")
}

/// Render the following results unsorted (`raw`) or with unordered rows sorted.
pub fn set_raw_order(raw: bool) {
    RAW_ORDER.with(|c| c.set(raw));
}

fn has_order_by(sql: &str) -> bool {
    sql.to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|w| w[0] == "order" && w[1] == "by")
}

fn render_field(out: &mut String, kind: &str, f: &Field) {
    let _ = write!(out, "{kind} {}: {}", f.code, f.message);
    if let Some(p) = f.position {
        let _ = write!(out, " (at {p})");
    }
    out.push('\n');
    if let Some(d) = &f.detail {
        for l in d.lines() {
            let _ = writeln!(out, "DETAIL: {l}");
        }
    }
    if let Some(h) = &f.hint {
        let _ = writeln!(out, "HINT: {h}");
    }
    if let Some((p, q)) = &f.internal {
        let _ = writeln!(out, "QUERY: {q} (at {p})");
    }
    if let Some(c) = &f.context {
        for l in c.lines() {
            let _ = writeln!(out, "CONTEXT: {}", temp_names(l));
        }
    }
    if !f.objects.is_empty() {
        let _ = writeln!(out, "OBJECTS: {}", f.objects.join(" "));
    }
}

/// `pg_temp_<digits>` (a session's temporary schema, a temporary table named after an
/// OID) as `pg_temp_N`: the numbers differ between servers.
fn temp_names(l: &str) -> String {
    let mut out = String::new();
    let mut rest = l;
    while let Some(i) = rest.find("pg_temp_") {
        out.push_str(&rest[..i + 8]);
        rest = &rest[i + 8..];
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits > 0 {
            out.push('N');
            rest = &rest[digits..];
        }
    }
    out.push_str(rest);
    out
}

/// The golden text for one statement and what it produced.
pub fn render(stmt: &str, events: &[Event]) -> String {
    let mut out = String::new();
    let (sql, data) = split_copy(stmt);
    for l in sql.trim().lines() {
        let _ = writeln!(out, "> {l}");
    }
    for l in data.as_deref().unwrap_or("").lines() {
        let _ = writeln!(out, ">> {l}");
    }
    for e in events {
        match e {
            Event::Notice(f) => {
                let kind = if f.severity.is_empty() {
                    "NOTICE"
                } else {
                    f.severity.as_str()
                };
                render_field(&mut out, kind, f);
            }
            Event::Error(f) => render_field(&mut out, "ERROR", f),
            Event::Result { cols, rows, tag } => {
                if let Some(cols) = cols {
                    let head: Vec<String> = cols
                        .iter()
                        .map(|(n, t)| format!("{n}:{}", type_name(*t)))
                        .collect();
                    let _ = writeln!(out, "{}", head.join(" | "));
                    let mut lines: Vec<String> = rows
                        .iter()
                        .map(|r| {
                            r.iter()
                                .map(|v| v.clone().unwrap_or_else(|| "NULL".into()))
                                .collect::<Vec<_>>()
                                .join(" | ")
                        })
                        .collect();
                    // EXPLAIN output is ordered (a plan tree).
                    if !has_order_by(sql) && tag != "EXPLAIN" && !RAW_ORDER.with(|c| c.get()) {
                        lines.sort();
                    }
                    for l in lines {
                        let _ = writeln!(out, "{l}");
                    }
                }
                let _ = writeln!(out, "-- {tag}");
            }
        }
    }
    out.push('\n');
    out
}

/// Split a test file into statements the way psql does: on `;` outside quotes, dollar
/// quotes and comments. Works for files with deliberate syntax errors.
pub fn statements(src: &str) -> Vec<String> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    let mut depth = 0i32; // parentheses
    // BEGIN ATOMIC ... END in CREATE FUNCTION / PROCEDURE (CASE ... END nests), like psql.
    let mut atomic = 0i32;
    // `SET standard_conforming_strings = off`: backslashes escape in ordinary literals too.
    let mut standard = true;
    while i < b.len() {
        let c = b[i];
        // -- comment
        if c == '-' && b.get(i + 1) == Some(&'-') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // /* nested */ comment
        if c == '/' && b.get(i + 1) == Some(&'*') {
            let mut n = 0;
            while i < b.len() {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    n += 1;
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    n -= 1;
                    i += 2;
                    if n == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            cur.push(' ');
            continue;
        }
        if c == '\'' || c == '"' {
            let prefixed = |p: &[char]| {
                i > 0 && p.contains(&b[i - 1]) && (i < 2 || !b[i - 2].is_alphanumeric())
            };
            let escape = c == '\''
                && (prefixed(&['E', 'e'])
                    || !standard && !prefixed(&['B', 'b', 'X', 'x', '&']));
            cur.push(c);
            i += 1;
            while i < b.len() {
                cur.push(b[i]);
                if escape && b[i] == '\\' && i + 1 < b.len() {
                    cur.push(b[i + 1]);
                    i += 2;
                    continue;
                }
                if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        cur.push(c);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == '$' && (i == 0 || !(b[i - 1].is_alphanumeric() || b[i - 1] == '_')) {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
                j += 1;
            }
            if j < b.len()
                && b[j] == '$'
                && !b[i + 1..j].first().is_some_and(|c| c.is_ascii_digit())
            {
                let tag: String = b[i..=j].iter().collect();
                cur.push_str(&tag);
                i = j + 1;
                let rest: String = b[i..].iter().collect();
                match rest.find(&tag) {
                    Some(k) => {
                        cur.push_str(&rest[..k + tag.len()]);
                        i += rest[..k + tag.len()].chars().count();
                    }
                    None => {
                        cur.push_str(&rest);
                        i = b.len();
                    }
                }
                continue;
            }
        }
        if c.is_alphabetic() && (i == 0 || !(b[i - 1].is_alphanumeric() || b[i - 1] == '_')) {
            let mut j = i;
            while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
                j += 1;
            }
            let word: String = b[i..j].iter().collect();
            let head = cur.trim_start().to_lowercase();
            let routine = head.starts_with("create")
                && (head.contains(" function ") || head.contains(" procedure "));
            match word.to_lowercase().as_str() {
                "begin" if routine => atomic += 1,
                "case" if atomic > 0 => atomic += 1,
                "end" if atomic > 0 => atomic -= 1,
                _ => {}
            }
            cur.push_str(&word);
            i = j;
            continue;
        }
        // psql's `\;`: a semicolon that doesn't end the query (several statements in one
        // simple query).
        if c == '\\' && b.get(i + 1) == Some(&';') {
            cur.push(';');
            i += 2;
            continue;
        }
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ';' if depth <= 0 && atomic == 0 => {
                let mut t = cur.trim().to_string();
                cur.clear();
                i += 1;
                if is_copy_from_stdin(&t) {
                    // The data: the lines after this one, up to `\.`.
                    while i < b.len() && b[i] != '\n' {
                        i += 1;
                    }
                    i += 1;
                    let mut data = String::new();
                    while i < b.len() {
                        let end = b[i..]
                            .iter()
                            .position(|c| *c == '\n')
                            .map_or(b.len(), |k| i + k);
                        let line: String = b[i..end].iter().collect();
                        i = end + 1;
                        if line == "\\." {
                            break;
                        }
                        data.push_str(&line);
                        data.push('\n');
                    }
                    t.push_str("\n\\copy-data\n");
                    t.push_str(&data);
                }
                let low = t.to_lowercase();
                if low.contains("standard_conforming_strings") && low.starts_with("set ") {
                    standard = !(low.contains("off") || low.contains("false"));
                } else if low == "reset all" || low == "reset standard_conforming_strings" {
                    standard = true;
                }
                if !t.is_empty() {
                    out.push(t);
                }
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    let t = cur.trim().to_string();
    if !t.is_empty() {
        out.push(t);
    }
    out
}

fn is_copy_from_stdin(sql: &str) -> bool {
    let w: Vec<String> = sql
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect();
    w.first().is_some_and(|f| f == "copy")
        && w.windows(2).any(|p| p[0] == "from" && p[1] == "stdin")
}
