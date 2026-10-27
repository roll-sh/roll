//! COPY TO / COPY FROM: CSV export and import with cqlsh's options.
//!
//! Follows Apache Cassandra's `pylib/cqlshlib/copyutil.py` (Apache-2.0) for the options, the
//! messages and the value formats (export through cqlsh's formatter without colours, floats with
//! FLOATPRECISION / DOUBLEPRECISION digits, timestamps to the millisecond), and Python's `csv`
//! module for quoting (`QUOTE_MINIMAL`, the escape character instead of doubled quotes unless
//! QUOTE equals ESCAPE, `\r\n` line ends). cqlsh splits the work across processes; this runs in
//! one, scanning in token order, and imports with unlogged batches of literal INSERTs.

use std::io::{Read, Write};
use std::time::Instant;

use nosql_proto::{QueryParams, ResultBody};
use nosql_types::CqlType;

use super::format::{self, Opts};
use super::lex::{self, Kind, Token};
use super::shell::{Input, P, Shell, expand_user, os_error};

/// The CSV dialect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dialect {
    pub delimiter: char,
    pub quote: char,
    /// `None` when QUOTE equals ESCAPE: quotes are doubled instead.
    pub escape: Option<char>,
}

impl Default for Dialect {
    fn default() -> Self {
        Dialect { delimiter: ',', quote: '"', escape: Some('\\') }
    }
}

/// Python `csv.writer` with `QUOTE_MINIMAL`.
pub fn write_row(fields: &[String], d: &Dialect) -> String {
    let mut out = String::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(d.delimiter);
        }
        let mut quoted = fields.len() == 1 && f.is_empty();
        let mut body = String::new();
        for c in f.chars() {
            let special = c == d.delimiter || Some(c) == d.escape || c == d.quote || c == '\r' || c == '\n';
            if special {
                if c == d.quote {
                    match d.escape {
                        None => {
                            body.push(c);
                            quoted = true;
                        }
                        Some(e) => body.push(e),
                    }
                } else if Some(c) == d.escape {
                    body.push(c);
                } else {
                    quoted = true;
                }
            }
            body.push(c);
        }
        if quoted {
            out.push(d.quote);
            out.push_str(&body);
            out.push(d.quote);
        } else {
            out.push_str(&body);
        }
    }
    out.push_str("\r\n");
    out
}

/// Python `csv.reader` (not strict): every record of `text`.
pub fn read_rows(text: &str, d: &Dialect) -> Vec<Vec<String>> {
    let mut rows = vec![];
    let mut row: Vec<String> = vec![];
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    #[derive(PartialEq)]
    enum St {
        Start,
        Unquoted,
        Quoted,
        AfterQuote,
    }
    let mut st = St::Start;
    let mut any = false;
    while let Some(c) = chars.next() {
        any = true;
        match st {
            St::Start | St::Unquoted => {
                if st == St::Start && c == d.quote {
                    st = St::Quoted;
                } else if Some(c) == d.escape {
                    if let Some(n) = chars.next() {
                        field.push(n);
                    }
                    st = St::Unquoted;
                } else if c == d.delimiter {
                    row.push(std::mem::take(&mut field));
                    st = St::Start;
                } else if c == '\n' || c == '\r' {
                    if c == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    if !(row.is_empty() && field.is_empty() && st == St::Start) {
                        row.push(std::mem::take(&mut field));
                        rows.push(std::mem::take(&mut row));
                    }
                    st = St::Start;
                    any = false;
                } else {
                    field.push(c);
                    st = St::Unquoted;
                }
            }
            St::Quoted => {
                if Some(c) == d.escape {
                    if let Some(n) = chars.next() {
                        field.push(n);
                    }
                } else if c == d.quote {
                    if d.escape.is_none() && chars.peek() == Some(&d.quote) {
                        chars.next();
                        field.push(c);
                    } else {
                        st = St::AfterQuote;
                    }
                } else {
                    field.push(c);
                }
            }
            St::AfterQuote => {
                if c == d.delimiter {
                    row.push(std::mem::take(&mut field));
                    st = St::Start;
                } else if c == '\n' || c == '\r' {
                    if c == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                    st = St::Start;
                    any = false;
                } else {
                    field.push(c);
                    st = St::Unquoted;
                }
            }
        }
    }
    if any {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// A parsed COPY.
#[derive(Debug, PartialEq, Eq)]
pub struct Copy {
    pub ks: Option<String>,
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub from: bool,
    /// `None`: STDIN / STDOUT.
    pub file: Option<String>,
    pub options: Vec<(String, String)>,
}

pub fn parse(src: &str, toks: &[Token]) -> Option<Copy> {
    let mut p = P::new(src, toks);
    p.i = 1;
    let (ks, table) = p.qualified()?;
    let mut columns = None;
    if p.op("(") {
        let mut cols = vec![lex::unprotect(p.name()?)];
        while p.op(",") {
            cols.push(lex::unprotect(p.name()?));
        }
        if !p.op(")") {
            return None;
        }
        columns = Some(cols);
    }
    let from = if p.word("from") {
        true
    } else if p.word("to") {
        false
    } else {
        return None;
    };
    let file = if let Some(s) = p.kind(Kind::QuotedString).or_else(|| p.kind(Kind::PgString)) {
        Some(lex::dequote_value(s))
    } else if (from && p.word("stdin")) || (!from && p.word("stdout")) {
        None
    } else {
        return None;
    };
    let mut options = vec![];
    if p.word("with") {
        loop {
            let name = p.kind(Kind::Identifier)?.to_lowercase();
            if !p.op("=") {
                return None;
            }
            let t = p.peek()?;
            p.i += 1;
            let mut v = t.text(src).to_string();
            if t.kind == Kind::Op && v == "-" {
                v.push_str(p.kind(Kind::WholeNumber).or_else(|| p.kind(Kind::Float))?);
            }
            options.push((name, lex::dequote_value(&v)));
            if !p.word("and") {
                break;
            }
        }
    }
    p.done().then(|| Copy { ks: ks.map(lex::unprotect), table: lex::unprotect(table), columns, from, file, options })
}

fn num_processes() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    cores.saturating_sub(1).clamp(1, 16)
}

/// `describe_interval`.
pub fn describe_interval(mut seconds: f64) -> String {
    let mut desc = vec![];
    for (len, unit) in [(86400.0, "day"), (3600.0, "hour"), (60.0, "minute")] {
        let n = (seconds.trunc() / len).floor() as i64;
        if n > 0 {
            desc.push(format!("{n} {unit}{}", if n > 1 { "s" } else { "" }));
        }
        seconds %= len;
    }
    let words = format!("{seconds:.3} seconds");
    match desc.len() {
        0 => words,
        1 => format!("{} and {words}", desc[0]),
        _ => format!("{}, and {words}", desc.join(", ")),
    }
}

struct Options {
    dialect: Dialect,
    nullval: String,
    header: bool,
    pagesize: i32,
    float_precision: usize,
    double_precision: usize,
    decimal_sep: String,
    thousands_sep: String,
    bool_style: (String, String),
    max_rows: i64,
    skip_rows: usize,
    skip_cols: Vec<String>,
    max_batch: usize,
    ttl: i64,
    begin_token: Option<String>,
    end_token: Option<String>,
    max_output: i64,
    unrecognized: Vec<String>,
}

fn parse_options(opts: &[(String, String)], from: bool) -> Result<Options, String> {
    let get = |k: &str| opts.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let common = [
        "delimiter", "quote", "escape", "header", "null", "datetimeformat", "maxattempts", "reportfrequency", "decimalsep", "thousandssep",
        "boolstyle", "numprocesses", "configfile", "ratefile",
    ];
    let to_only = ["encoding", "pagesize", "pagetimeout", "begintoken", "endtoken", "maxoutputsize", "maxrequests", "floatprecision", "doubleprecision"];
    let from_only = [
        "chunksize", "ingestrate", "maxbatchsize", "minbatchsize", "maxrows", "skiprows", "skipcols", "maxparseerrors", "maxinserterrors", "errfile",
        "preparedstatements", "ttl",
    ];
    let unrecognized: Vec<String> = opts
        .iter()
        .map(|(k, _)| k.clone())
        .filter(|k| !common.contains(&k.as_str()) && !(if from { from_only.contains(&k.as_str()) } else { to_only.contains(&k.as_str()) }))
        .collect();
    let quote = get("quote").and_then(|q| q.chars().next()).unwrap_or('"');
    let escape = get("escape").and_then(|q| q.chars().next()).unwrap_or('\\');
    let int = |k: &str, d: i64| -> Result<i64, String> {
        match get(k) {
            Some(v) => v.trim().parse::<i64>().map_err(|_| format!("invalid literal for int() with base 10: {}", super::session::py_repr(&v))),
            None => Ok(d),
        }
    };
    let styles: Vec<String> = get("boolstyle").unwrap_or_else(|| "True, False".into()).split(',').map(|s| s.trim().to_string()).collect();
    if styles.len() != 2 || styles[0] == styles[1] {
        return Err(format!("Invalid boolean styles {}", super::session::py_repr(&format!("{styles:?}"))));
    }
    Ok(Options {
        dialect: Dialect {
            delimiter: get("delimiter").and_then(|q| q.chars().next()).unwrap_or(','),
            quote,
            escape: if quote == escape { None } else { Some(escape) },
        },
        nullval: get("null").unwrap_or_default(),
        header: get("header").is_some_and(|h| h.eq_ignore_ascii_case("true")),
        pagesize: int("pagesize", 1000)? as i32,
        float_precision: int("floatprecision", 5)? as usize,
        double_precision: int("doubleprecision", 12)? as usize,
        decimal_sep: get("decimalsep").unwrap_or_else(|| ".".into()),
        thousands_sep: get("thousandssep").unwrap_or_default(),
        bool_style: (styles[0].clone(), styles[1].clone()),
        max_rows: int("maxrows", -1)?,
        skip_rows: int("skiprows", 0)?.max(0) as usize,
        skip_cols: get("skipcols").map(|s| s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()).unwrap_or_default(),
        max_batch: int("maxbatchsize", 20)?.max(1) as usize,
        ttl: int("ttl", -1)?,
        begin_token: get("begintoken").filter(|t| !t.is_empty()),
        end_token: get("endtoken").filter(|t| !t.is_empty()),
        max_output: int("maxoutputsize", -1)?,
        unrecognized,
    })
}

/// The table's columns in the driver's order (partition key, clustering, the rest by name).
async fn table_columns(shell: &Shell, ks: &str, table: &str) -> Vec<(String, String, i64)> {
    let q = format!(
        "SELECT column_name, kind, position FROM system_schema.columns WHERE keyspace_name = '{}' AND table_name = '{}'",
        ks.replace('\'', "''"),
        table.replace('\'', "''")
    );
    let Ok(Some(rows)) = shell.conn.all_rows(&q, nosql_proto::Consistency::One).await else { return vec![] };
    let mut cols: Vec<(String, String, i64)> = rows
        .to_vec()
        .iter()
        .map(|r| {
            let t = |i: usize| r.get(i).cloned().flatten().map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            let pos = r.get(2).cloned().flatten().and_then(|b| <[u8; 4]>::try_from(&b[..]).ok()).map(i32::from_be_bytes).unwrap_or(-1);
            (t(0), t(1), pos as i64)
        })
        .collect();
    let rank = |k: &str| match k {
        "partition_key" => 0,
        "clustering" => 1,
        _ => 2,
    };
    cols.sort_by(|a, b| rank(&a.1).cmp(&rank(&b.1)).then(if rank(&a.1) < 2 { a.2.cmp(&b.2) } else { std::cmp::Ordering::Equal }));
    cols
}

pub async fn do_copy(shell: &mut Shell, src: &str, toks: &[Token], cmdword: &str) {
    let Some(c) = parse(src, toks) else {
        return shell.printerr(&format!("Improper {cmdword} command."));
    };
    let Some(ks) = c.ks.clone().or(shell.keyspace.clone()) else {
        return shell.printerr("Not in any keyspace.");
    };
    let keyspaces = super::describe::keyspace_names(shell).await;
    if !keyspaces.contains(&ks) {
        return shell.printerr(&format!("Keyspace {} not found.", super::session::py_repr(&ks)));
    }
    let cols = table_columns(shell, &ks, &c.table).await;
    if cols.is_empty() {
        return shell.printerr(&format!("Column family {} not found", c.table));
    }
    let opts = match parse_options(&c.options, c.from) {
        Ok(o) => o,
        Err(e) => return shell.printerr(&format!("Error parsing options: {e}")),
    };
    let columns = c.columns.clone().unwrap_or_else(|| cols.iter().map(|c| c.0.clone()).collect());
    let file = c.file.as_deref().map(expand_user);
    let verbose = file.is_some() || c.from;
    if !opts.unrecognized.is_empty() {
        let dir = if c.from { "FROM" } else { "TO" };
        return shell.printerr(&format!("Unrecognized COPY {dir} options: {}", opts.unrecognized.join(", ")));
    }
    if verbose && shell.flavor == super::format::Flavor::Scylla && !c.options.is_empty() {
        // ScyllaDB's cqlsh reports the options even without a configuration file.
        let opts: Vec<String> = c.options.iter().map(|(k, v)| format!("{}: {}", super::session::py_repr(k), super::session::py_repr(v))).collect();
        shell.print(&format!("Reading options from the command line: {{{}}}", opts.join(", ")));
    }
    if verbose {
        shell.print(&format!("Using {} child processes", num_processes()));
    }
    for col in &columns {
        if !cols.iter().any(|c| &c.0 == col) {
            return shell.printerr(&format!("Invalid column name {col}"));
        }
    }
    let start = Instant::now();
    if c.from {
        import(shell, &ks, &c.table, &columns, &cols, file, &opts, start).await;
    } else {
        export(shell, &ks, &c.table, &columns, &cols, file, &opts, start).await;
    }
}

fn names(cols: &[String]) -> String {
    format!("[{}]", cols.join(", "))
}

fn progress(n: usize, start: Instant) -> String {
    let rate = n as f64 / start.elapsed().as_secs_f64().max(1e-3);
    format!("Processed: {n} rows; Rate: {rate:7.0} rows/s; Avg. rate: {rate:7.0} rows/s\r\r")
}

#[allow(clippy::too_many_arguments)]
async fn export(
    shell: &mut Shell,
    ks: &str,
    table: &str,
    columns: &[String],
    all: &[(String, String, i64)],
    file: Option<String>,
    o: &Options,
    start: Instant,
) {
    let mut out: Box<dyn Write> = match &file {
        Some(f) => match std::fs::File::create(f) {
            Ok(f) => Box::new(std::io::BufWriter::new(f)),
            Err(e) => {
                let msg = format!("Can't open {} for writing: {}", super::session::py_repr(f), os_error(&e, f));
                return shell.printerr(&msg);
            }
        },
        None => Box::new(SharedStdout(shell.io.clone())),
    };
    let verbose = file.is_some();
    if verbose {
        shell.print(&format!("\nStarting copy of {ks}.{table} with columns {}.", names(columns)));
    }
    let mut fopts = Opts::new(shell.flavor);
    fopts.copy = true;
    fopts.nullval = o.nullval.clone();
    fopts.float_precision = o.float_precision;
    fopts.double_precision = o.double_precision;
    fopts.zone = shell.opts.zone;
    if o.decimal_sep != "." {
        fopts.decimal_sep = Some(o.decimal_sep.clone());
    }
    if !o.thousands_sep.is_empty() {
        fopts.thousands_sep = Some(o.thousands_sep.clone());
    }
    if o.bool_style != ("True".into(), "False".into()) {
        fopts.bool_styles = Some(o.bool_style.clone());
    }
    if o.header {
        let _ = out.write_all(write_row(columns, &o.dialect).as_bytes());
    }
    let pk: Vec<String> = all.iter().filter(|c| c.1 == "partition_key").map(|c| lex::protect_name(&c.0)).collect();
    let mut q = format!(
        "SELECT {} FROM {}.{}",
        columns.iter().map(|c| lex::protect_name(c)).collect::<Vec<_>>().join(", "),
        lex::protect_name(ks),
        lex::protect_name(table)
    );
    let mut wh = vec![];
    if let Some(b) = &o.begin_token {
        wh.push(format!("token({}) > {b}", pk.join(", ")));
    }
    if let Some(e) = &o.end_token {
        wh.push(format!("token({}) <= {e}", pk.join(", ")));
    }
    if !wh.is_empty() {
        q.push_str(&format!(" WHERE {}", wh.join(" AND ")));
    }
    let mut params = QueryParams::new(shell.consistency);
    params.page_size = Some(o.pagesize.max(1));
    let mut n = 0usize;
    loop {
        let r = match shell.conn.query(&q, params.clone(), false).await {
            Ok(r) => r,
            Err(f) => {
                shell.printerr(&format!("Error for (None, None): {}", f.line()));
                break;
            }
        };
        let ResultBody::Rows(rows) = r.body else { break };
        let types: Vec<CqlType> = rows.metadata.columns.iter().map(|c| format::cql_type(&c.ty)).collect();
        for row in rows.rows() {
            if o.max_output > 0 && n as i64 >= o.max_output {
                break;
            }
            let fields: Vec<String> = row.enumerate().map(|(i, v)| format::format(&types[i], v.as_deref(), &fopts).text).collect();
            let _ = out.write_all(write_row(&fields, &o.dialect).as_bytes());
            n += 1;
        }
        match rows.metadata.paging_state {
            Some(s) => params.paging_state = Some(s),
            None => break,
        }
    }
    let _ = out.flush();
    drop(out);
    if verbose {
        shell.print_raw(&progress(n, start));
        shell.print(&format!("\n{n} rows exported to 1 files in {}.", describe_interval(start.elapsed().as_secs_f64())));
    }
}

/// Python's `str.title()`-free check of a boolean style.
fn bool_literal(v: &str, o: &Options) -> Option<&'static str> {
    if v.eq_ignore_ascii_case(&o.bool_style.0) {
        Some("true")
    } else if v.eq_ignore_ascii_case(&o.bool_style.1) {
        Some("false")
    } else {
        None
    }
}

fn split_top(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    if chars.peek() == Some(&q) {
                        cur.push(chars.next().unwrap_or(q));
                    } else {
                        quote = None;
                    }
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '[' | '{' | '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ']' | '}' | ')' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => out.push(std::mem::take(&mut cur).trim().to_string()),
                c => cur.push(c),
            },
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn split_colon(s: &str) -> Option<(String, String)> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '[' | '{' | '(' => depth += 1,
                ']' | '}' | ')' => depth -= 1,
                ':' if depth == 0 => return Some((s[..i].trim().to_string(), s[i + 1..].trim().to_string())),
                _ => {}
            },
        }
    }
    None
}

fn unquote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') { s[1..s.len() - 1].replace("''", "'") } else { s.to_string() }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A CSV field (or a component of one, `nested`) as a CQL literal of type `ty`.
pub fn literal(ty: &CqlType, v: &str, nested: bool, o: &OptsLite) -> Result<String, String> {
    use CqlType as T;
    let v = if nested { v.trim() } else { v };
    if nested && (v == "None" || v == "null" || v.is_empty()) {
        return Ok("null".into());
    }
    Ok(match ty {
        T::Ascii | T::Text => {
            let s = if nested { unquote(v) } else { v.to_string() };
            quote(&s)
        }
        T::Timestamp | T::Date | T::Time | T::Inet => quote(&if nested { unquote(v) } else { v.to_string() }),
        T::Boolean => match (o.bool)(v) {
            Some(b) => b.into(),
            None => return Err(format!("Invalid boolean value {v}")),
        },
        T::Tinyint | T::Smallint | T::Int | T::Bigint | T::Varint | T::Counter => {
            let s = v.replace(&o.thousands, "");
            if s.trim_start_matches('-').chars().all(|c| c.is_ascii_digit()) && !s.is_empty() {
                s
            } else {
                return Err(format!("invalid literal for int() with base 10: {}", super::session::py_repr(v)));
            }
        }
        T::Float | T::Double | T::Decimal => {
            let s = v.replace(&o.thousands, "").replace(&o.decimal, ".");
            match s.as_str() {
                "NaN" | "Infinity" | "-Infinity" => s,
                _ if s.parse::<f64>().is_ok() => s,
                _ => return Err(format!("could not convert string to float: {}", super::session::py_repr(v))),
            }
        }
        T::List { elem, .. } | T::Set { elem, .. } | T::Vector { elem, .. } => {
            let (open, close) = if matches!(ty, T::Set { .. }) { ('{', '}') } else { ('[', ']') };
            let inner = v.trim().strip_prefix(['[', '{']).and_then(|s| s.strip_suffix([']', '}'])).ok_or_else(|| format!("Invalid collection {v}"))?;
            let parts: Result<Vec<String>, String> = split_top(inner).iter().map(|e| literal(elem, e, true, o)).collect();
            format!("{open}{}{close}", parts?.join(", "))
        }
        T::Map { key, value, .. } => {
            let inner = v.trim().strip_prefix('{').and_then(|s| s.strip_suffix('}')).ok_or_else(|| format!("Invalid map {v}"))?;
            let mut parts = vec![];
            for e in split_top(inner) {
                let (k, val) = split_colon(&e).ok_or_else(|| format!("Invalid map entry {e}"))?;
                parts.push(format!("{}: {}", literal(key, &k, true, o)?, literal(value, &val, true, o)?));
            }
            format!("{{{}}}", parts.join(", "))
        }
        T::Tuple(ts) => {
            let inner = v.trim().strip_prefix('(').and_then(|s| s.strip_suffix(')')).ok_or_else(|| format!("Invalid tuple {v}"))?;
            let es = split_top(inner);
            let parts: Result<Vec<String>, String> = ts.iter().enumerate().map(|(i, t)| literal(t, es.get(i).map(String::as_str).unwrap_or("None"), true, o)).collect();
            format!("({})", parts?.join(", "))
        }
        T::Udt(def, _) => {
            let inner = v.trim().strip_prefix('{').and_then(|s| s.strip_suffix('}')).ok_or_else(|| format!("Invalid UDT {v}"))?;
            let mut parts = vec![];
            for e in split_top(inner) {
                let (k, val) = split_colon(&e).ok_or_else(|| format!("Invalid UDT field {e}"))?;
                let name = lex::unprotect(&k);
                let t = def.fields.iter().find(|(n, _)| *n == name || n.eq_ignore_ascii_case(&k)).map(|(_, t)| t).ok_or_else(|| format!("Unknown field {k}"))?;
                parts.push(format!("{}: {}", lex::protect_name(&name), literal(t, &val, true, o)?));
            }
            format!("{{{}}}", parts.join(", "))
        }
        _ => v.to_string(),
    })
}

/// What [`literal`] needs from the options.
pub struct OptsLite<'a> {
    pub bool: &'a dyn Fn(&str) -> Option<&'static str>,
    pub thousands: String,
    pub decimal: String,
}

fn read_stdin_rows(shell: &mut Shell) -> String {
    shell.print("[Use . on a line by itself to end input]");
    let mut text = String::new();
    match &mut shell.input {
        Input::Lines(r) | Input::Prompted(r) => loop {
            let mut l = String::new();
            match r.read_line(&mut l) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    shell.lineno += 1;
                    if l.trim_end_matches(['\r', '\n']) == "\\." {
                        break;
                    }
                    text.push_str(&l);
                }
            }
        },
        Input::Tty(ed) => {
            while let Ok(l) = tokio::task::block_in_place(|| ed.readline("[copy] ")) {
                if l == "\\." {
                    break;
                }
                text.push_str(&l);
                text.push('\n');
            }
        }
        // With -e cqlsh reads the process's stdin to its end (a `\.` line is data).
        Input::Single(_) => {
            let _ = std::io::stdin().read_to_string(&mut text);
            shell.lineno += text.matches('\n').count();
        }
    }
    text
}

#[allow(clippy::too_many_arguments)]
async fn import(
    shell: &mut Shell,
    ks: &str,
    table: &str,
    columns: &[String],
    all: &[(String, String, i64)],
    file: Option<String>,
    o: &Options,
    start: Instant,
) {
    let valid: Vec<String> = columns.iter().filter(|c| !o.skip_cols.contains(c)).cloned().collect();
    if valid.is_empty() {
        return shell.printerr("No valid column specified");
    }
    for (name, kind, _) in all {
        if (kind == "partition_key" || kind == "clustering") && !valid.contains(name) {
            return shell.printerr(&format!("Primary key column '{name}' missing or skipped"));
        }
    }
    shell.print(&format!("\nStarting copy of {ks}.{table} with columns {}.", names(&valid)));
    let text = match &file {
        Some(pattern) => {
            let mut text = String::new();
            for f in pattern.split(',').map(str::trim) {
                match std::fs::read(f) {
                    Ok(b) => {
                        let b = b.strip_prefix(&[0xef, 0xbb, 0xbf][..]).unwrap_or(&b);
                        text.push_str(&String::from_utf8_lossy(b));
                    }
                    Err(e) => {
                        let msg = format!("Failed to import from file {}: {}", super::session::py_repr(f), os_error(&e, f));
                        shell.printerr(&msg);
                    }
                }
            }
            text
        }
        None => read_stdin_rows(shell),
    };
    let mut rows = read_rows(&text, &o.dialect);
    if o.header && !rows.is_empty() {
        rows.remove(0);
    }
    let rows: Vec<Vec<String>> = rows.into_iter().skip(o.skip_rows).take(if o.max_rows < 0 { usize::MAX } else { o.max_rows as usize }).collect();
    // Column types from a zero-row read.
    let probe = format!(
        "SELECT {} FROM {}.{} LIMIT 1",
        valid.iter().map(|c| lex::protect_name(c)).collect::<Vec<_>>().join(", "),
        lex::protect_name(ks),
        lex::protect_name(table)
    );
    let types: Vec<CqlType> = match shell.conn.query(&probe, QueryParams::new(shell.consistency), false).await {
        Ok(r) => match r.body {
            ResultBody::Rows(r) => r.metadata.columns.iter().map(|c| format::cql_type(&c.ty)).collect(),
            _ => vec![],
        },
        Err(f) => return shell.printerr(&f.line()),
    };
    let keep: Vec<usize> = columns.iter().enumerate().filter(|(_, c)| !o.skip_cols.contains(c)).map(|(i, _)| i).collect();
    let bool_fn = |v: &str| bool_literal(v, o);
    let lite = OptsLite { bool: &bool_fn, thousands: o.thousands_sep.clone(), decimal: o.decimal_sep.clone() };
    let head = format!(
        "INSERT INTO {}.{} ({}) VALUES",
        lex::protect_name(ks),
        lex::protect_name(table),
        valid.iter().map(|c| lex::protect_name(c)).collect::<Vec<_>>().join(", ")
    );
    let ttl = if o.ttl > 0 { format!(" USING TTL {}", o.ttl) } else { String::new() };
    let mut imported = 0usize;
    let mut stmts: Vec<String> = vec![];
    let mut parse_errors = 0usize;
    for r in &rows {
        if r.len() != columns.len() {
            parse_errors += 1;
            shell.printerr(&format!(
                "Failed to import 1 rows: ParseError - Invalid row length {} should be {},  given up without retries",
                r.len(),
                columns.len()
            ));
            continue;
        }
        let mut vals = vec![];
        let mut err = None;
        for (j, i) in keep.iter().enumerate() {
            let v = &r[*i];
            if *v == o.nullval {
                vals.push("null".to_string());
                continue;
            }
            match literal(types.get(j).unwrap_or(&CqlType::Text), v, false, &lite) {
                Ok(l) => vals.push(l),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = err {
            parse_errors += 1;
            shell.printerr(&format!("Failed to import 1 rows: ParseError - {e},  given up without retries"));
            continue;
        }
        stmts.push(format!("{head} ({}){ttl}", vals.join(", ")));
    }
    for chunk in stmts.chunks(o.max_batch) {
        let q = if chunk.len() == 1 {
            chunk[0].clone()
        } else {
            format!("BEGIN UNLOGGED BATCH {}; APPLY BATCH", chunk.join("; "))
        };
        match shell.conn.query(&q, QueryParams::new(shell.consistency), false).await {
            Ok(_) => imported += chunk.len(),
            Err(f) => {
                let (name, msg) = match &f {
                    super::session::Failure::Server(e) => super::session::error_message(e),
                    super::session::Failure::Client(s) => ("DriverException", s.clone()),
                };
                shell.printerr(&format!("Failed to import {} rows: {name} - {msg},  given up after 1 attempts", chunk.len()));
            }
        }
    }
    shell.print_raw(&progress(imported, start));
    shell.print(&format!(
        "\n{imported} rows imported from {} files in {} ({} skipped).",
        usize::from(file.is_some() || !rows.is_empty()).max(1),
        describe_interval(start.elapsed().as_secs_f64()),
        o.skip_rows
    ));
    let _ = parse_errors;
}

/// COPY TO STDOUT writes through the shell's stdout.
struct SharedStdout(super::shell::SharedIo);

impl Write for SharedStdout {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().stdout.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().stdout.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_like_python() {
        let d = Dialect::default();
        let row = |v: &[&str]| write_row(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &d);
        assert_eq!(row(&["1", "héllo\"", "[1, 2, 3]", ""]), "1,héllo\\\",\"[1, 2, 3]\",\r\n");
        assert_eq!(row(&["a\\\\b"]), "a\\\\\\\\b\r\n");
        assert_eq!(row(&[""]), "\"\"\r\n");
        assert_eq!(row(&["a\nb"]), "\"a\nb\"\r\n");
        let dq = Dialect { escape: None, ..Dialect::default() };
        assert_eq!(write_row(&["say \"hi\"".to_string()], &dq), "\"say \"\"hi\"\"\"\r\n");
        let pipe = Dialect { delimiter: '|', ..Dialect::default() };
        assert_eq!(write_row(&["k".to_string(), "a;b".to_string()], &pipe), "k|a;b\r\n");
        // Reading undoes it.
        assert_eq!(
            read_rows("1,héllo\\\",\"[1, 2, 3]\",\r\n2,x,y,z\n", &d),
            vec![vec!["1", "héllo\"", "[1, 2, 3]", ""], vec!["2", "x", "y", "z"]]
        );
        assert_eq!(read_rows("\"a\nb\",c\n", &d), vec![vec!["a\nb", "c"]]);
        assert_eq!(read_rows("\"say \"\"hi\"\"\"\r\n", &dq), vec![vec!["say \"hi\""]]);
        assert_eq!(read_rows("a\\\\\\\\b\r\n", &d), vec![vec!["a\\\\b"]]);
    }

    #[test]
    fn parses_copy() {
        let s = "COPY ks.t (k, \"V\") TO '/tmp/x.csv' WITH HEADER = true AND DELIMITER = '|' AND MAXOUTPUTSIZE = -1;";
        let toks = lex::lex(s).unwrap();
        let c = parse(s, &toks).unwrap();
        assert_eq!(c.ks.as_deref(), Some("ks"));
        assert_eq!(c.columns, Some(vec!["k".into(), "V".into()]));
        assert!(!c.from);
        assert_eq!(c.file.as_deref(), Some("/tmp/x.csv"));
        assert_eq!(c.options, vec![("header".into(), "true".into()), ("delimiter".into(), "|".into()), ("maxoutputsize".into(), "-1".into())]);
        let s = "COPY t FROM STDIN";
        let toks = lex::lex(s).unwrap();
        let c = parse(s, &toks).unwrap();
        assert!(c.from && c.file.is_none() && c.ks.is_none());
        let s = "COPY t SIDEWAYS STDIN";
        assert!(parse(s, &lex::lex(s).unwrap()).is_none());
    }

    #[test]
    fn literals_from_csv() {
        use CqlType as T;
        let b = |v: &str| match v.to_lowercase().as_str() {
            "true" => Some("true"),
            "false" => Some("false"),
            _ => None,
        };
        let o = OptsLite { bool: &b, thousands: String::new(), decimal: ".".into() };
        assert_eq!(literal(&T::Text, "it's", false, &o).unwrap(), "'it''s'");
        assert_eq!(literal(&T::list(T::Int, false), "[1, 2, 3]", false, &o).unwrap(), "[1, 2, 3]");
        assert_eq!(literal(&T::map(T::Text, T::Date, false), "{'a': 2026-10-20}", false, &o).unwrap(), "{'a': '2026-10-20'}");
        assert_eq!(literal(&T::Tuple(vec![T::Int, T::Text]), "(1, 'it''s')", false, &o).unwrap(), "(1, 'it''s')");
        assert_eq!(literal(&T::Tuple(vec![T::Int, T::Text]), "(None, None)", false, &o).unwrap(), "(null, null)");
        assert_eq!(literal(&T::Boolean, "True", false, &o).unwrap(), "true");
        assert!(literal(&T::Int, "x", false, &o).is_err());
        assert_eq!(describe_interval(0.0991), "0.099 seconds");
        assert_eq!(describe_interval(62.5), "1 minute and 2.500 seconds");
        assert_eq!(describe_interval(7322.0), "2 hours, 2 minutes, and 2.000 seconds");
    }
}
