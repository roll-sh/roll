//! The ES|QL lexer and parser: a query is a source command (`FROM`, `ROW`, `SHOW`) followed
//! by `|`-separated processing commands, parsed into [`Query`] with every expression keeping
//! its source span (unnamed `EVAL`/`STATS` outputs are named by their source text, as in
//! Elasticsearch) and every error located `line L:C` like Elasticsearch's.

use super::value::V;
use crate::error::{EsError, EsResult};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    /// A backquoted identifier.
    Quoted(String),
    Str(String),
    Int(i64),
    Dec(f64),
    Op(&'static str),
    Pipe,
    Param(String),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

const OPS: &[&str] = &["==", "!=", "<=", ">=", "=~", "::", "<", ">", "+", "-", "*", "/", "%", "(", ")", ",", "=", "[", "]", ".", ":", "{", "}", ";"];

/// What ANTLR expects where a processing command must start.
const PROCESSING: &str = "{'change_point', 'enrich', 'completion', 'dissect', 'eval', 'grok', 'limit', 'rerank', 'sample', 'sort', STATS, 'where', 'uri_parts', 'metrics_info', 'registered_domain', 'ts_info', 'user_agent', 'ts_collapse', 'ip_location', 'fork', 'fuse', 'inline', 'inlinestats', 'lookup', 'mmr', 'mv_expand', 'drop', 'keep', 'rename'}";
/// ... where a source command must start.
const SOURCE: &str = "{, 'row', 'from', 'ts', 'promql', 'set', 'show'}";
/// ... where an expression must start.
pub const EXPRESSION: &str = "{QUOTED_STRING, INTEGER_LITERAL, DECIMAL_LITERAL, 'false', 'first', 'last', 'not', 'null', '?', 'true', '+', '-', '??', NAMED_OR_POSITIONAL_PARAM, NAMED_OR_POSITIONAL_DOUBLE_PARAMS, '[', '(', UNQUOTED_IDENTIFIER, QUOTED_IDENTIFIER}";
/// ... where a name pattern must start (`KEEP`, `DROP`, `RENAME`).
const ID_PATTERN: &str = "{'?', '??', NAMED_OR_POSITIONAL_PARAM, NAMED_OR_POSITIONAL_DOUBLE_PARAMS, '[', ID_PATTERN}";
/// ... where a constant must start (`LIMIT`).
const CONSTANT: &str = "{QUOTED_STRING, INTEGER_LITERAL, DECIMAL_LITERAL, 'false', 'null', '?', 'true', '+', '-', NAMED_OR_POSITIONAL_PARAM, '['}";

pub fn lex(src: &str) -> EsResult<Vec<Token>> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    // `*` belongs to identifiers only in the commands taking name patterns (Elasticsearch's
    // lexer modes for KEEP / DROP / FROM / RENAME).
    let mut patterns = false;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        let start = i;
        if c == b'|' {
            out.push(Token { tok: Tok::Pipe, start, end: i + 1 });
            patterns = false;
            i += 1;
            continue;
        }
        if c == b'"' {
            if src[i..].starts_with("\"\"\"") {
                let rest = &src[i + 3..];
                let Some(end) = rest.find("\"\"\"") else { return Err(parse_error(src, start, "token recognition error at: '\"'")) };
                out.push(Token { tok: Tok::Str(rest[..end].to_string()), start, end: i + 3 + end + 3 });
                i += 3 + end + 3;
                continue;
            }
            let mut s = String::new();
            i += 1;
            loop {
                let Some(&ch) = b.get(i) else { return Err(parse_error(src, start, &format!("token recognition error at: '{}'", &src[start..]))) };
                if ch == b'"' {
                    i += 1;
                    break;
                }
                if ch == b'\\' {
                    let esc = b.get(i + 1).copied().unwrap_or(b'\\');
                    s.push(match esc {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'"' => '"',
                        b'\\' => '\\',
                        other => other as char,
                    });
                    i += 2;
                    continue;
                }
                let ch_len = src[i..].chars().next().map_or(1, char::len_utf8);
                s.push_str(&src[i..i + ch_len]);
                i += ch_len;
            }
            out.push(Token { tok: Tok::Str(s), start, end: i });
            continue;
        }
        if c == b'`' {
            let mut s = String::new();
            i += 1;
            loop {
                let Some(&ch) = b.get(i) else { return Err(parse_error(src, start, "token recognition error at: '`'")) };
                if ch == b'`' {
                    if b.get(i + 1) == Some(&b'`') {
                        s.push('`');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                let ch_len = src[i..].chars().next().map_or(1, char::len_utf8);
                s.push_str(&src[i..i + ch_len]);
                i += ch_len;
            }
            out.push(Token { tok: Tok::Quoted(s), start, end: i });
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit) && !prev_is_ident(&out)) {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let mut dec = false;
            if j < b.len() && b[j] == b'.' && b.get(j + 1).is_none_or(|d| d.is_ascii_digit() || !d.is_ascii_alphabetic()) {
                dec = true;
                j += 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
            }
            if j < b.len() && (b[j] == b'e' || b[j] == b'E') {
                let mut k = j + 1;
                if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
                    k += 1;
                }
                if k < b.len() && b[k].is_ascii_digit() {
                    dec = true;
                    j = k;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                }
            }
            let text = &src[i..j];
            let tok = if dec {
                Tok::Dec(text.parse().map_err(|_| parse_error(src, start, &format!("Cannot parse number [{text}]")))?)
            } else {
                match text.parse::<i64>() {
                    Ok(n) => Tok::Int(n),
                    Err(_) => Tok::Dec(text.parse().unwrap_or(f64::INFINITY)),
                }
            };
            out.push(Token { tok, start, end: j });
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' || c == b'@' || c >= 0x80 {
            let mut j = i;
            while j < b.len() {
                let d = b[j];
                let word = d.is_ascii_alphanumeric() || d == b'_' || d == b'@' || d >= 0x80;
                let dot = d == b'.' && b.get(j + 1).is_some_and(|n| n.is_ascii_alphanumeric() || *n == b'_' || *n == b'@' || *n == b'`');
                if word || dot || (d == b'*' && patterns) {
                    j += 1;
                } else {
                    break;
                }
            }
            let word = &src[i..j];
            if matches!(out.last().map(|t: &Token| &t.tok), None | Some(Tok::Pipe)) {
                patterns = ["KEEP", "DROP", "FROM", "RENAME"].iter().any(|k| word.eq_ignore_ascii_case(k));
            }
            out.push(Token { tok: Tok::Ident(word.to_string()), start, end: j });
            i = j;
            continue;
        }
        if c == b'?' {
            // `??name`: a parameter whose value is an identifier (kept with its `?`).
            let from = if b.get(i + 1) == Some(&b'?') { i + 1 } else { i };
            let mut j = from + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            out.push(Token { tok: Tok::Param(src[from + (from == i) as usize..j].to_string()), start, end: j });
            i = j;
            continue;
        }
        if c == b'*' {
            // `*` alone or a leading wildcard in an identifier pattern (`KEEP *name`).
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'.' || b[j] == b'*') {
                j += 1;
            }
            if j > i + 1 && patterns && prev_allows_pattern(&out) {
                out.push(Token { tok: Tok::Ident(src[i..j].to_string()), start, end: j });
                i = j;
                continue;
            }
        }
        let Some(op) = OPS.iter().find(|o| src[i..].starts_with(*o)) else {
            let ch = src[i..].chars().next().unwrap_or(' ');
            return Err(parse_error(src, start, &format!("token recognition error at: '{ch}'")));
        };
        out.push(Token { tok: Tok::Op(op), start, end: i + op.len() });
        i += op.len();
    }
    out.push(Token { tok: Tok::Eof, start: src.len(), end: src.len() });
    Ok(out)
}

fn prev_is_ident(out: &[Token]) -> bool {
    matches!(out.last().map(|t| &t.tok), Some(Tok::Ident(_) | Tok::Quoted(_)))
}

fn prev_allows_pattern(out: &[Token]) -> bool {
    match out.last().map(|t| &t.tok) {
        Some(Tok::Ident(k)) => matches!(k.to_ascii_uppercase().as_str(), "KEEP" | "DROP" | "FROM"),
        Some(Tok::Op(",")) => true,
        _ => false,
    }
}

/// `line L:C` of a byte offset (1-based, like ANTLR's positions).
pub fn location(src: &str, at: usize) -> (usize, usize) {
    let at = at.min(src.len());
    let before = &src[..at];
    let line = before.matches('\n').count() + 1;
    let col = before.rfind('\n').map_or(before.chars().count(), |p| before[p + 1..].chars().count()) + 1;
    (line, col)
}

pub fn parse_error(src: &str, at: usize, msg: &str) -> EsError {
    let (l, c) = location(src, at);
    let e = EsError::new(400, "parsing_exception", format!("line {l}:{c}: {msg}"));
    // ANTLR's recognition exception travels as the (reason-less) cause.
    if msg.starts_with("mismatched input '") {
        e.caused_by(EsError::java(400, "input_mismatch_exception", "").with_null_reason())
    } else if msg.starts_with("token recognition error") {
        e.caused_by(EsError::java(400, "lexer_no_viable_alt_exception", "").with_null_reason())
    } else if msg.starts_with("no viable alternative") {
        e.caused_by(EsError::java(400, "no_viable_alt_exception", "").with_null_reason())
    } else {
        e
    }
}

/// The type names (and aliases) a `::` cast may name: Elasticsearch's `DataType` names; the
/// ones without a conversion are refused as unsupported.
const CAST_TYPES: &[&str] = &[
    "boolean", "bool", "byte", "short", "integer", "int", "long", "unsigned_long", "float", "half_float", "scaled_float", "double", "keyword", "string", "text", "datetime", "date", "date_nanos", "ip", "version", "object", "date_period", "time_duration", "geo_point",
    "cartesian_point", "geo_shape", "cartesian_shape", "geohash", "geotile", "geohex", "counter_long", "counter_integer", "counter_double", "aggregate_metric_double", "dense_vector", "histogram", "exponential_histogram", "tdigest", "partial_agg", "flattened",
];

#[derive(Clone, Debug, PartialEq, Eq, Copy)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    And,
    Or,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Lit(V),
    Col(String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>, bool),
    Like(Box<Expr>, String, bool, bool),
    In(Box<Expr>, Vec<Expr>, bool),
    Call(String, Vec<Expr>),
    /// `COUNT(*)`'s argument.
    Star,
    /// A time span literal (`1 day`, `2 hours`): its normalized text.
    Span(String),
    Cast(Box<Expr>, String),
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub struct Named {
    pub name: String,
    pub expr: Expr,
}

#[derive(Clone, Debug)]
pub struct SortKey {
    pub expr: Expr,
    pub asc: bool,
    pub nulls_first: bool,
}

#[derive(Clone, Debug)]
pub enum Cmd {
    From { sources: Vec<String>, metadata: Vec<String>, at: usize },
    Row(Vec<Named>),
    Show(String),
    Where(Expr),
    Eval(Vec<Named>),
    Stats { aggs: Vec<Named>, by: Vec<Named> },
    Sort(Vec<SortKey>),
    Limit(usize),
    Keep(Vec<(String, usize)>),
    Drop(Vec<(String, usize)>),
    Rename(Vec<(String, String, usize)>),
    MvExpand(String, usize),
    /// `DISSECT field "pattern" [APPEND_SEPARATOR = "sep"]`.
    Dissect { field: Expr, pattern: String, append_separator: Option<String>, at: usize },
    /// `GROK field "pattern"`, compiled (and checked) by the parser like Elasticsearch's.
    Grok { field: Expr, grok: std::sync::Arc<search_painless::grok::Grok>, at: usize },
    /// `ENRICH policy [ON field] [WITH [name =] field, ...]` (`with`: output name, enrich field).
    Enrich { policy: String, policy_at: usize, on: Option<(String, usize)>, with: Option<Vec<(String, String, usize)>>, at: usize },
    /// `INLINE STATS`: `STATS` whose results are appended to every row of their group.
    InlineStats { aggs: Vec<Named>, by: Vec<Named> },
    /// `SAMPLE probability`.
    Sample(f64),
    /// `LOOKUP JOIN index ON field, ...`.
    LookupJoin { index: String, index_at: usize, on: Vec<(String, usize)> },
    /// `TS index`: a time series source; its first `STATS` aggregates per time series (and
    /// time bucket) first, then across series.
    Ts { sources: Vec<String>, metadata: Vec<String>, at: usize },
    /// `FORK (commands) (commands) ...`: each branch runs over the input, tagged `_fork`.
    Fork { branches: Vec<Vec<Cmd>>, at: usize },
    /// A source that is the union of whole queries: `FROM` with subqueries (`FROM a, (FROM b
    /// | ...)`) or over ES|QL views. `at`: the `FROM`.
    Union { branches: Vec<Branch>, at: usize },
}

/// One input of a [`Cmd::Union`].
#[derive(Clone, Debug)]
pub struct Branch {
    /// Its commands, a source command first; spans point into `src`.
    pub cmds: Vec<Cmd>,
    /// The text the spans point into (the query's, or a view's).
    pub src: std::sync::Arc<str>,
    /// The view it expands, if any.
    pub view: Option<String>,
    /// Where the subquery starts in `src` (`(` of a subquery).
    pub at: usize,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub src: String,
    pub cmds: Vec<Cmd>,
    /// Built by the SQL engine (its functions aren't checked with ES|QL's rules).
    pub sql: bool,
    /// The request's `locale` (texts of `DATE_FORMAT`; English when absent).
    pub locale: Option<String>,
    /// The `SET name = value;` statements before the source command.
    pub settings: Settings,
}

/// How references to fields no index maps resolve (`SET unmapped_fields`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Unmapped {
    /// `Unknown column` errors.
    #[default]
    Default,
    /// Null columns of type `null`.
    Nullify,
    /// Keyword columns read from `_source`.
    Load,
}

impl Unmapped {
    pub fn name(self) -> &'static str {
        match self {
            Unmapped::Default => "default",
            Unmapped::Nullify => "nullify",
            Unmapped::Load => "load",
        }
    }
}

/// The query settings (`QuerySettings`) a query sets with `SET`.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub unmapped: Unmapped,
    /// `time_zone`, validated (`Z`, offsets, region ids).
    pub time_zone: Option<String>,
    pub column_metadata: bool,
}

impl Settings {
    /// Applies `name = value` (Elasticsearch's `QuerySettings` validation and texts; `at`: the
    /// `SET` statement's offset).
    fn apply(&mut self, src: &str, at: usize, name: &str, value: &SetValue) -> EsResult<()> {
        let fail = |msg: String| Err(parse_error(src, at, &msg));
        let keyword = |what: &str| -> EsResult<String> {
            match value {
                SetValue::Lit(V::Str(s)) => Ok(s.clone()),
                _ => Err(parse_error(src, at, &format!("Setting [{what}] must be of type KEYWORD"))),
            }
        };
        match name {
            "unmapped_fields" => {
                let v = keyword(name)?;
                self.unmapped = match v.to_ascii_uppercase().as_str() {
                    "DEFAULT" => Unmapped::Default,
                    "NULLIFY" => Unmapped::Nullify,
                    "LOAD" => Unmapped::Load,
                    _ => return fail(format!("Error validating setting [unmapped_fields]: Invalid unmapped_fields resolution [{v}], must be one of [DEFAULT, NULLIFY, LOAD]")),
                };
            }
            "time_zone" => {
                let v = keyword(name)?;
                if crate::time::Zone::parse(&v).is_err() {
                    return fail(format!("Error validating setting [time_zone]: Invalid time zone [{v}]"));
                }
                self.time_zone = Some(v);
            }
            "column_metadata" => match value {
                SetValue::Lit(V::Bool(b)) => self.column_metadata = *b,
                _ => return fail(format!("Setting [{name}] must be of type BOOLEAN")),
            },
            // Query approximation is an optimization hint: exact results satisfy it.
            "approximation" => {}
            "project_routing" => {
                keyword(name)?;
                return fail("Error validating setting [project_routing]: cross-project search not enabled".into());
            }
            _ => return fail(format!("Unknown setting [{name}]")),
        }
        Ok(())
    }
}

/// A `SET` value: a constant, or a map literal (`approximation={"rows":10000}`).
enum SetValue {
    Lit(V),
    Map,
}

struct P<'a> {
    src: &'a str,
    toks: Vec<Token>,
    i: usize,
    params: &'a [V],
    named: &'a [(String, V)],
}

fn kw(t: &Tok, k: &str) -> bool {
    matches!(t, Tok::Ident(s) if s.eq_ignore_ascii_case(k))
}

impl<'a> P<'a> {
    fn peek(&self) -> &Token {
        &self.toks[self.i]
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.i].clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn is_kw(&self, k: &str) -> bool {
        kw(&self.peek().tok, k)
    }

    fn eat_kw(&mut self, k: &str) -> bool {
        if self.is_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek().tok, Tok::Op(o) if o == op)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn token_text(&self, t: &Token) -> String {
        match &t.tok {
            Tok::Eof => "<EOF>".into(),
            _ => self.src[t.start..t.end].to_string(),
        }
    }

    fn mismatch(&self, expecting: &str) -> EsError {
        let t = self.peek();
        parse_error(self.src, t.start, &format!("mismatched input '{}' expecting {expecting}", self.token_text(t)))
    }

    fn expect_op(&mut self, op: &str) -> EsResult<()> {
        if self.eat_op(op) { Ok(()) } else { Err(self.mismatch(&format!("'{op}'"))) }
    }

    fn ident(&mut self) -> EsResult<(String, usize)> {
        let t = self.peek().clone();
        match t.tok {
            Tok::Ident(s) => {
                self.bump();
                Ok((s, t.start))
            }
            Tok::Quoted(s) => {
                self.bump();
                Ok((s, t.start))
            }
            // A value parameter where a name goes names that column (null: the empty name).
            Tok::Param(ref p) if !p.is_empty() => {
                let v = match p.parse::<usize>() {
                    Ok(n) => n.checked_sub(1).and_then(|i| self.params.get(i)).cloned(),
                    Err(_) => self.named.iter().find(|(k, _)| k == p).map(|(_, v)| v.clone()),
                };
                match v {
                    Some(v) => {
                        self.bump();
                        Ok((if v.is_null() { String::new() } else { v.to_text() }, t.start))
                    }
                    None => Err(self.mismatch("{UNQUOTED_IDENTIFIER, QUOTED_IDENTIFIER}")),
                }
            }
            _ => Err(self.mismatch("{UNQUOTED_IDENTIFIER, QUOTED_IDENTIFIER}")),
        }
    }

    /// A field name: identifiers joined by `.` (quoted parts allowed).
    fn qualified(&mut self) -> EsResult<(String, usize)> {
        let (mut name, at) = self.ident()?;
        while self.is_op(".") {
            self.bump();
            let (n, _) = self.ident()?;
            name.push('.');
            name.push_str(&n);
        }
        Ok((name, at))
    }

    /// A processing command (one that may follow `|`).
    fn processing(&mut self) -> EsResult<Cmd> {
        let t = self.peek().clone();
        if !matches!(&t.tok, Tok::Ident(_)) || matches!(&t.tok, Tok::Ident(k) if matches!(k.to_ascii_uppercase().as_str(), "FROM" | "ROW" | "SHOW")) {
            return Err(parse_error(self.src, t.start, &format!("mismatched input '{}' expecting {PROCESSING}", self.token_text(&t))));
        }
        self.command()
    }

    /// The query inside a `FROM` subquery, after its `(`, up to and including its `)`.
    fn subquery(&mut self) -> EsResult<Vec<Cmd>> {
        let first = self.command()?;
        if !matches!(first, Cmd::From { .. } | Cmd::Ts { .. } | Cmd::Row(_) | Cmd::Show(_) | Cmd::Union { .. }) {
            return Err(parse_error(self.src, 0, "mismatched input expecting {'from', 'row', 'show'}"));
        }
        let mut cmds = vec![first];
        while matches!(self.peek().tok, Tok::Pipe) {
            self.bump();
            cmds.push(self.processing()?);
        }
        self.expect_op(")")?;
        Ok(cmds)
    }

    fn command(&mut self) -> EsResult<Cmd> {
        let t = self.peek().clone();
        let Tok::Ident(name) = &t.tok else {
            return Err(parse_error(self.src, t.start, &format!("mismatched input '{}' expecting {SOURCE}", self.token_text(&t))));
        };
        let up = name.to_ascii_uppercase();
        self.bump();
        Ok(match up.as_str() {
            "FROM" | "TS" => {
                let mut sources = Vec::new();
                let mut subqueries: Vec<(Vec<Cmd>, usize)> = Vec::new();
                loop {
                    if self.is_op("(") {
                        let open = self.bump().start;
                        subqueries.push((self.subquery()?, open));
                        if !self.eat_op(",") {
                            break;
                        }
                        continue;
                    }
                    let t = self.bump();
                    match t.tok {
                        Tok::Ident(s) | Tok::Quoted(s) | Tok::Str(s) => {
                            // Index patterns may contain `-`, `:` and `*`; glue adjacent pieces.
                            let mut s = s;
                            while matches!(self.peek().tok, Tok::Op("-") | Tok::Op(":") | Tok::Op("*")) && self.peek().start == self.toks[self.i - 1].end {
                                let n = self.bump();
                                s.push_str(&self.src[n.start..n.end]);
                                if let Tok::Ident(_) | Tok::Int(_) = self.peek().tok
                                    && self.peek().start == n.end
                                {
                                    let m = self.bump();
                                    s.push_str(&self.src[m.start..m.end]);
                                }
                            }
                            sources.push(s);
                        }
                        Tok::Op("*") => sources.push("*".into()),
                        _ => return Err(parse_error(self.src, t.start, &format!("mismatched input '{}' expecting {{QUOTED_STRING, '(', UNQUOTED_SOURCE}}", self.token_text(&t)))),
                    }
                    if !self.eat_op(",") {
                        break;
                    }
                }
                let mut metadata = Vec::new();
                if self.eat_kw("METADATA") {
                    loop {
                        metadata.push(self.ident()?.0);
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                }
                if up == "TS" {
                    Cmd::Ts { sources, metadata, at: t.start }
                } else if subqueries.is_empty() {
                    Cmd::From { sources, metadata, at: t.start }
                } else {
                    let text: std::sync::Arc<str> = self.src.into();
                    let mut branches = Vec::new();
                    if !sources.is_empty() {
                        branches.push(Branch { cmds: vec![Cmd::From { sources, metadata, at: t.start }], src: text.clone(), view: None, at: t.start });
                    }
                    for (cmds, at) in subqueries {
                        branches.push(Branch { cmds, src: text.clone(), view: None, at });
                    }
                    Cmd::Union { branches, at: t.start }
                }
            }
            "FORK" => {
                let mut branches = Vec::new();
                while self.is_op("(") {
                    self.bump();
                    let mut cmds = vec![self.processing()?];
                    while matches!(self.peek().tok, Tok::Pipe) {
                        self.bump();
                        cmds.push(self.processing()?);
                    }
                    self.expect_op(")")?;
                    branches.push(cmds);
                }
                if branches.is_empty() {
                    return Err(self.mismatch("'('"));
                }
                if branches.len() > 8 {
                    return Err(parse_error(self.src, t.start, "Fork supports up to 8 branches"));
                }
                Cmd::Fork { branches, at: t.start }
            }
            "ROW" => Cmd::Row(self.named_list()?),
            "SHOW" => Cmd::Show(self.ident()?.0.to_ascii_uppercase()),
            "WHERE" => Cmd::Where(self.expr()?),
            "EVAL" => Cmd::Eval(self.named_list()?),
            "STATS" | "INLINESTATS" | "INLINE" => {
                let inline = up != "STATS";
                if up == "INLINE" && !self.eat_kw("STATS") {
                    return Err(self.mismatch("STATS"));
                }
                let aggs = if self.is_kw("BY") || matches!(self.peek().tok, Tok::Pipe | Tok::Eof) { Vec::new() } else { self.agg_list()? };
                let by = if self.eat_kw("BY") { self.named_list()? } else { Vec::new() };
                if aggs.is_empty() && by.is_empty() {
                    return Err(parse_error(self.src, t.start, &format!("At least one aggregation or grouping expression required in [{}]", &self.src[t.start..self.toks[self.i - 1].end])));
                }
                if inline { Cmd::InlineStats { aggs, by } } else { Cmd::Stats { aggs, by } }
            }
            "DISSECT" | "GROK" => {
                let field = self.primary()?;
                let pt = self.bump();
                let Tok::Str(pattern) = pt.tok else { return Err(parse_error(self.src, pt.start, &format!("mismatched input '{}' expecting QUOTED_STRING", self.token_text(&pt)))) };
                if up == "GROK" {
                    Cmd::Grok { field, grok: std::sync::Arc::new(grok(self.src, &pattern, t.start)?), at: t.start }
                } else {
                    let mut append_separator = None;
                    if let Tok::Ident(o) = &self.peek().tok {
                        let (o, oat) = (o.clone(), self.peek().start);
                        self.bump();
                        self.expect_op("=")?;
                        let v = self.bump();
                        let Tok::Str(sep) = v.tok else { return Err(parse_error(self.src, v.start, &format!("mismatched input '{}' expecting QUOTED_STRING", self.token_text(&v)))) };
                        if !o.eq_ignore_ascii_case("append_separator") {
                            return Err(parse_error(self.src, oat, &format!("Invalid option for dissect: [{o}]")));
                        }
                        append_separator = Some(sep);
                    }
                    Cmd::Dissect { field, pattern, append_separator, at: t.start }
                }
            }
            "ENRICH" => {
                let pt = self.peek().clone();
                let mut policy = match &pt.tok {
                    Tok::Ident(n) | Tok::Quoted(n) | Tok::Str(n) => n.clone(),
                    _ => return Err(self.mismatch("{QUOTED_STRING, ENRICH_POLICY_NAME}")),
                };
                self.bump();
                // Policy names may hold `-` and `.`; `_any:` style modes prefix them.
                while matches!(self.peek().tok, Tok::Op("-") | Tok::Op(".") | Tok::Op(":")) && self.peek().start == self.toks[self.i - 1].end {
                    let n = self.bump();
                    policy.push_str(&self.src[n.start..n.end]);
                    if matches!(self.peek().tok, Tok::Ident(_) | Tok::Int(_)) && self.peek().start == n.end {
                        let m = self.bump();
                        policy.push_str(&self.src[m.start..m.end]);
                    }
                }
                let on = if self.eat_kw("ON") { Some(self.qualified()?) } else { None };
                let with = if self.eat_kw("WITH") {
                    let mut list = Vec::new();
                    loop {
                        let (a, at) = self.qualified()?;
                        if self.eat_op("=") {
                            let (b, _) = self.qualified()?;
                            list.push((a, b, at));
                        } else {
                            list.push((a.clone(), a, at));
                        }
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    Some(list)
                } else {
                    None
                };
                Cmd::Enrich { policy, policy_at: pt.start, on, with, at: t.start }
            }
            "SAMPLE" => {
                let e = self.unary()?;
                let p = match &e.kind {
                    ExprKind::Lit(v) => v.as_f64(),
                    _ => None,
                };
                match p {
                    Some(p) if p > 0.0 && p < 1.0 => Cmd::Sample(p),
                    _ => return Err(parse_error(self.src, e.start, &format!("invalid value for SAMPLE probability [{}], expecting a number between 0 and 1, exclusive", &self.src[e.start..e.end]))),
                }
            }
            "LOOKUP" => {
                if !self.eat_kw("JOIN") {
                    return Err(self.mismatch("'join'"));
                }
                let it = self.bump();
                let mut index = match &it.tok {
                    Tok::Ident(n) | Tok::Quoted(n) | Tok::Str(n) => n.clone(),
                    _ => return Err(parse_error(self.src, it.start, &format!("mismatched input '{}' expecting {{QUOTED_STRING, UNQUOTED_SOURCE}}", self.token_text(&it)))),
                };
                while matches!(self.peek().tok, Tok::Op("-") | Tok::Op(".")) && self.peek().start == self.toks[self.i - 1].end {
                    let n = self.bump();
                    index.push_str(&self.src[n.start..n.end]);
                    if matches!(self.peek().tok, Tok::Ident(_) | Tok::Int(_)) && self.peek().start == n.end {
                        let m = self.bump();
                        index.push_str(&self.src[m.start..m.end]);
                    }
                }
                // Patterns name no single lookup index.
                if index.contains('*') || (matches!(self.peek().tok, Tok::Op("*")) && self.peek().start == self.toks[self.i - 1].end) {
                    while !matches!(self.peek().tok, Tok::Eof) && self.peek().start == self.toks[self.i - 1].end {
                        let n = self.bump();
                        index.push_str(&self.src[n.start..n.end]);
                    }
                    return Err(parse_error(self.src, it.start, &format!("invalid index pattern [{index}], * is not allowed in LOOKUP JOIN")));
                }
                if !self.eat_kw("ON") {
                    return Err(self.mismatch("'on'"));
                }
                let mut on = Vec::new();
                loop {
                    on.push(self.qualified()?);
                    if !self.eat_op(",") {
                        break;
                    }
                }
                Cmd::LookupJoin { index, index_at: it.start, on }
            }
            "SORT" => {
                let mut keys = Vec::new();
                loop {
                    let expr = self.expr()?;
                    let asc = if self.eat_kw("DESC") {
                        false
                    } else {
                        self.eat_kw("ASC");
                        true
                    };
                    let nulls_first = if self.eat_kw("NULLS") {
                        if self.eat_kw("FIRST") {
                            true
                        } else if self.eat_kw("LAST") {
                            false
                        } else {
                            return Err(self.mismatch("{'first', 'last'}"));
                        }
                    } else {
                        !asc
                    };
                    keys.push(SortKey { expr, asc, nulls_first });
                    if !self.eat_op(",") {
                        break;
                    }
                }
                Cmd::Sort(keys)
            }
            "LIMIT" => {
                let n = self.peek().clone();
                if !matches!(n.tok, Tok::Int(_) | Tok::Dec(_) | Tok::Str(_) | Tok::Param(_) | Tok::Op("-" | "+" | "["))
                    && !matches!(&n.tok, Tok::Ident(w) if ["true", "false", "null"].iter().any(|k| w.eq_ignore_ascii_case(k)))
                {
                    return Err(self.mismatch(CONSTANT));
                }
                let e = self.unary()?;
                let ExprKind::Lit(v) = &e.kind else { return Err(parse_error(self.src, e.start, &format!("mismatched input '{}' expecting {CONSTANT}", &self.src[e.start..e.end]))) };
                match v {
                    V::Int(i) if *i >= 0 => Cmd::Limit(*i as usize),
                    other => {
                        let ty = match other {
                            V::Int(_) => "integer",
                            V::Long(_) => "long",
                            V::Double(_) => "double",
                            V::Bool(_) => "boolean",
                            V::Null => "null",
                            _ => "keyword",
                        };
                        return Err(parse_error(
                            self.src,
                            t.start,
                            &format!(
                                "value of [{}] must be a non negative integer, found value [{}] type [{ty}]",
                                &self.src[t.start..e.end],
                                if matches!(n.tok, Tok::Param(_)) { self.src[e.start..e.end].to_string() } else { other.to_text() }
                            ),
                        ));
                    }
                }
            }
            "KEEP" | "DROP" => {
                let mut pats = Vec::new();
                loop {
                    let t = self.peek().clone();
                    if self.eat_op("*") {
                        pats.push(("*".to_string(), t.start));
                    } else if matches!(t.tok, Tok::Ident(_) | Tok::Quoted(_) | Tok::Param(_)) {
                        pats.push(self.qualified()?);
                    } else {
                        return Err(self.mismatch(ID_PATTERN));
                    }
                    if !self.eat_op(",") {
                        break;
                    }
                }
                if up == "KEEP" { Cmd::Keep(pats) } else { Cmd::Drop(pats) }
            }
            "RENAME" => {
                let mut list = Vec::new();
                loop {
                    if !matches!(self.peek().tok, Tok::Ident(_) | Tok::Quoted(_) | Tok::Param(_)) {
                        return Err(self.mismatch(ID_PATTERN));
                    }
                    let (a, at) = self.qualified()?;
                    let (old, new) = if self.eat_kw("AS") {
                        (a, self.qualified()?.0)
                    } else if self.eat_op("=") {
                        let (b, _) = self.qualified()?;
                        (b, a)
                    } else {
                        return Err(self.mismatch("'as'"));
                    };
                    list.push((old, new, at));
                    if !self.eat_op(",") {
                        break;
                    }
                }
                Cmd::Rename(list)
            }
            "MV_EXPAND" => {
                if !matches!(self.peek().tok, Tok::Ident(_) | Tok::Quoted(_)) {
                    return Err(self.mismatch("{'?', '??', NAMED_OR_POSITIONAL_PARAM, NAMED_OR_POSITIONAL_DOUBLE_PARAMS, '[', UNQUOTED_IDENTIFIER, QUOTED_IDENTIFIER}"));
                }
                let (c, at) = self.qualified()?;
                Cmd::MvExpand(c, at)
            }
            _ => {
                return Err(parse_error(
                    self.src,
                    t.start,
                    &format!("mismatched input '{name}' expecting {PROCESSING}"),
                ));
            }
        })
    }

    /// `STATS`' aggregations: named expressions, each optionally filtered (`agg WHERE cond`,
    /// kept as a `$where` call of the aggregation and the condition).
    fn agg_list(&mut self) -> EsResult<Vec<Named>> {
        let mut out = Vec::new();
        loop {
            let save = self.i;
            let named = if let Tok::Ident(_) | Tok::Quoted(_) = self.peek().tok {
                let (n, _) = self.qualified()?;
                if self.eat_op("=") { Some(n) } else { None }
            } else {
                None
            };
            if named.is_none() {
                self.i = save;
            }
            let mut expr = self.expr()?;
            let name = named.unwrap_or_else(|| expr_name(self.src, &expr));
            if self.eat_kw("WHERE") {
                let cond = self.expr()?;
                let (start, end) = (expr.start, cond.end);
                expr = Expr { kind: ExprKind::Call("$where".into(), vec![expr, cond]), start, end };
            }
            out.push(Named { name, expr });
            if !self.eat_op(",") {
                break;
            }
        }
        Ok(out)
    }

    fn named_list(&mut self) -> EsResult<Vec<Named>> {
        let mut out = Vec::new();
        loop {
            // `name = expr` or `expr` (named by its text).
            let save = self.i;
            let named = if let Tok::Ident(_) | Tok::Quoted(_) = self.peek().tok {
                let (n, _) = self.qualified()?;
                if self.eat_op("=") { Some(n) } else { None }
            } else {
                None
            };
            if named.is_none() {
                self.i = save;
            }
            let expr = self.expr()?;
            let name = named.unwrap_or_else(|| expr_name(self.src, &expr));
            out.push(Named { name, expr });
            if !self.eat_op(",") {
                break;
            }
        }
        Ok(out)
    }

    fn expr(&mut self) -> EsResult<Expr> {
        self.or()
    }

    /// The right operand of a binary operator whose left operand starts at `left`: a token that
    /// can't start an operand is ANTLR's "no viable alternative" over the whole expression so far.
    fn operand(&mut self, left: usize, f: fn(&mut Self) -> EsResult<Expr>) -> EsResult<Expr> {
        let before = self.i;
        let r = f(self);
        if let Err(e) = &r
            && self.i == before
            && e.reason().contains("mismatched input '")
        {
            let t = self.peek().clone();
            let end = if matches!(t.tok, Tok::Eof) { self.src.len() } else { t.end };
            return Err(parse_error(self.src, t.start, &format!("no viable alternative at input '{}'", &self.src[left..end])));
        }
        r
    }

    fn or(&mut self) -> EsResult<Expr> {
        let mut l = self.and()?;
        while self.eat_kw("OR") {
            let r = self.and()?;
            l = bin(BinOp::Or, l, r);
        }
        Ok(l)
    }

    fn and(&mut self) -> EsResult<Expr> {
        let mut l = self.not()?;
        while self.eat_kw("AND") {
            let r = self.not()?;
            l = bin(BinOp::And, l, r);
        }
        Ok(l)
    }

    fn not(&mut self) -> EsResult<Expr> {
        let start = self.peek().start;
        if self.eat_kw("NOT") {
            let e = self.not()?;
            let end = e.end;
            return Ok(Expr { kind: ExprKind::Not(Box::new(e)), start, end });
        }
        self.predicate()
    }

    fn predicate(&mut self) -> EsResult<Expr> {
        let l = self.comparison()?;
        let start = l.start;
        if self.is_kw("IS") {
            self.bump();
            let neg = self.eat_kw("NOT");
            if !self.eat_kw("NULL") {
                return Err(self.mismatch("{'not', 'null'}"));
            }
            let end = self.toks[self.i - 1].end;
            return Ok(Expr { kind: ExprKind::IsNull(Box::new(l), neg), start, end });
        }
        let neg = if self.is_kw("NOT") && (kw(&self.toks[self.i + 1].tok, "LIKE") || kw(&self.toks[self.i + 1].tok, "RLIKE") || kw(&self.toks[self.i + 1].tok, "IN")) {
            self.bump();
            true
        } else {
            false
        };
        if self.is_kw("LIKE") || self.is_kw("RLIKE") {
            let rlike = self.is_kw("RLIKE");
            let op = if rlike { "RLIKE" } else { "LIKE" };
            self.bump();
            // One pattern, or a list of them (any may match).
            let list = self.eat_op("(");
            let mut pats = Vec::new();
            loop {
                let t = self.bump();
                let p = match t.tok {
                    Tok::Str(p) => p,
                    Tok::Param(ref name) if !name.is_empty() => {
                        let v = match name.parse::<usize>() {
                            Ok(n) => n.checked_sub(1).and_then(|i| self.params.get(i)).cloned(),
                            Err(_) => self.named.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()),
                        };
                        match v {
                            Some(V::Str(p)) => p,
                            Some(other) => {
                                let ty = match other {
                                    V::Null => "null",
                                    V::Int(_) => "integer",
                                    V::Long(_) => "long",
                                    V::Double(_) => "double",
                                    V::Bool(_) => "boolean",
                                    _ => "keyword",
                                };
                                return Err(parse_error(self.src, start, &format!("Invalid pattern parameter type for {op} [{}]: expected string, found {ty}", &self.src[t.start..t.end])));
                            }
                            None => return Err(parse_error(self.src, t.start, &format!("Unknown query parameter [{name}]"))),
                        }
                    }
                    _ => return Err(parse_error(self.src, t.start, &format!("mismatched input '{}' expecting QUOTED_STRING", self.token_text(&t)))),
                };
                pats.push((p, t.end));
                if !list || !self.eat_op(",") {
                    break;
                }
            }
            let mut end = pats.last().map_or(start, |(_, e)| *e);
            if list {
                end = self.peek().end;
                self.expect_op(")")?;
            }
            let mut it = pats.into_iter();
            let (p, _) = it.next().expect("one pattern");
            let mut e = Expr { kind: ExprKind::Like(Box::new(l.clone()), p, rlike, false), start, end };
            for (p, _) in it {
                let next = Expr { kind: ExprKind::Like(Box::new(l.clone()), p, rlike, false), start, end };
                e = Expr { kind: ExprKind::Bin(BinOp::Or, Box::new(e), Box::new(next)), start, end };
            }
            if neg {
                if let ExprKind::Like(_, _, _, n) = &mut e.kind {
                    *n = true;
                } else {
                    e = Expr { kind: ExprKind::Not(Box::new(e)), start, end };
                }
            }
            return Ok(e);
        }
        if self.eat_kw("IN") {
            self.expect_op("(")?;
            let mut list = Vec::new();
            loop {
                list.push(self.expr()?);
                if !self.eat_op(",") {
                    break;
                }
            }
            let end = self.peek().end;
            self.expect_op(")")?;
            return Ok(Expr { kind: ExprKind::In(Box::new(l), list, neg), start, end });
        }
        Ok(l)
    }

    fn comparison(&mut self) -> EsResult<Expr> {
        let l = self.additive()?;
        let op = match self.peek().tok {
            Tok::Op("==") => BinOp::Eq,
            Tok::Op("!=") => BinOp::Neq,
            Tok::Op("<") => BinOp::Lt,
            Tok::Op("<=") => BinOp::Lte,
            Tok::Op(">") => BinOp::Gt,
            Tok::Op(">=") => BinOp::Gte,
            // `field : query` is `MATCH(field, query)`.
            Tok::Op(":") => {
                self.bump();
                let r = self.additive()?;
                let (start, end) = (l.start, r.end);
                return Ok(Expr { kind: ExprKind::Call("match_operator".into(), vec![l, r]), start, end });
            }
            _ => return Ok(l),
        };
        self.bump();
        let r = self.operand(l.start, Self::additive)?;
        Ok(bin(op, l, r))
    }

    fn additive(&mut self) -> EsResult<Expr> {
        let mut l = self.multiplicative()?;
        loop {
            let op = match self.peek().tok {
                Tok::Op("+") => BinOp::Add,
                Tok::Op("-") => BinOp::Sub,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.operand(l.start, Self::multiplicative)?;
            l = bin(op, l, r);
        }
    }

    fn multiplicative(&mut self) -> EsResult<Expr> {
        let mut l = self.unary()?;
        loop {
            let op = match self.peek().tok {
                Tok::Op("*") => BinOp::Mul,
                Tok::Op("/") => BinOp::Div,
                Tok::Op("%") => BinOp::Mod,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.operand(l.start, Self::unary)?;
            l = bin(op, l, r);
        }
    }

    fn unary(&mut self) -> EsResult<Expr> {
        let start = self.peek().start;
        if self.eat_op("-") {
            let e = self.unary()?;
            let end = e.end;
            if let ExprKind::Lit(V::Long(n)) = e.kind {
                return Ok(Expr { kind: ExprKind::Lit(V::Long(-n)), start, end });
            }
            if let ExprKind::Lit(V::Int(n)) = e.kind {
                return Ok(Expr { kind: ExprKind::Lit(V::Int(-n)), start, end });
            }
            if let ExprKind::Lit(V::Double(n)) = e.kind {
                return Ok(Expr { kind: ExprKind::Lit(V::Double(-n)), start, end });
            }
            return Ok(Expr { kind: ExprKind::Neg(Box::new(e)), start, end });
        }
        if self.eat_op("+") {
            return self.unary();
        }
        let mut e = self.primary()?;
        while self.eat_op("::") {
            let (ty, ty_at) = self.ident()?;
            let end = self.toks[self.i - 1].end;
            let ty = ty.to_ascii_lowercase();
            if !CAST_TYPES.contains(&ty.as_str()) {
                return Err(parse_error(self.src, ty_at, &format!("Unknown data type named [{ty}]")));
            }
            // A type without a conversion function.
            if ty == "text" || super::exec::cast_function(&ty).is_none() && super::value::DataType::parse_name(&ty).is_none() && !matches!(ty.as_str(), "date_period" | "time_duration") {
                return Err(parse_error(self.src, start, &format!("Unsupported conversion to type [{}]", ty.to_ascii_uppercase())));
            }
            // A string cast to a temporal amount is that span (`"3 days"::date_period`).
            if let (ExprKind::Lit(V::Str(text)), "date_period" | "time_duration") = (&e.kind, ty.as_str())
                && let Some(span) = span_text(text)
            {
                e = Expr { kind: ExprKind::Span(span), start, end };
                continue;
            }
            e = Expr { kind: ExprKind::Cast(Box::new(e), ty), start, end };
        }
        Ok(e)
    }

    /// A function's options: `{"key": constant, ...}`, kept as a `$map` call of key/value
    /// argument pairs.
    fn map_literal(&mut self) -> EsResult<Expr> {
        let start = self.bump().start;
        let mut pairs = Vec::new();
        if !self.is_op("}") {
            loop {
                let t = self.bump();
                let Tok::Str(k) = t.tok else { return Err(parse_error(self.src, t.start, &format!("mismatched input '{}' expecting QUOTED_STRING", self.token_text(&t)))) };
                pairs.push(Expr { kind: ExprKind::Lit(V::Str(k)), start: t.start, end: t.end });
                self.expect_op(":")?;
                pairs.push(self.unary()?);
                if !self.eat_op(",") {
                    break;
                }
            }
        }
        let end = self.peek().end;
        self.expect_op("}")?;
        Ok(Expr { kind: ExprKind::Call("$map".into(), pairs), start, end })
    }

    fn primary(&mut self) -> EsResult<Expr> {
        let t = self.peek().clone();
        let lit = |v: V| Expr { kind: ExprKind::Lit(v), start: t.start, end: t.end };
        match &t.tok {
            Tok::Int(n) => {
                self.bump();
                // `1 hour`, `2 days`: a time span literal.
                if let Tok::Ident(u) = &self.peek().tok
                    && super::value::time_unit(u).is_some()
                {
                    let u = self.bump();
                    let text = format!("{n} {}", self.src[u.start..u.end].to_ascii_lowercase());
                    return Ok(Expr { kind: ExprKind::Span(text), start: t.start, end: u.end });
                }
                Ok(lit(if let Ok(i) = i32::try_from(*n) { V::Int(i) } else { V::Long(*n) }))
            }
            Tok::Op("[") => {
                self.bump();
                let mut vals = Vec::new();
                if !self.is_op("]") {
                    loop {
                        let e = self.unary()?;
                        let ExprKind::Lit(v) = e.kind else {
                            return Err(parse_error(self.src, e.start, "mismatched input expecting a constant"));
                        };
                        vals.push(v);
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                }
                let end = self.peek().end;
                self.expect_op("]")?;
                Ok(Expr { kind: ExprKind::Lit(if vals.is_empty() { V::Null } else { V::Multi(vals) }), start: t.start, end })
            }
            Tok::Dec(d) => {
                self.bump();
                Ok(lit(V::Double(*d)))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(lit(V::Str(s.clone())))
            }
            Tok::Param(name) => {
                self.bump();
                // `parse_with` numbered every anonymous `?` it could resolve.
                let v = if name.is_empty() {
                    return Err(parse_error(self.src, t.start, &format!("Not enough actual parameters {}", self.params.len())));
                } else if let Ok(n) = name.parse::<usize>() {
                    match n.checked_sub(1).and_then(|i| self.params.get(i)) {
                        Some(v) => v.clone(),
                        None => {
                            let hint = match self.params.len() {
                                0 => String::new(),
                                1 => ", did you mean position 1?".into(),
                                k => format!(", did you mean any position between 1 and {k}?"),
                            };
                            return Err(parse_error(self.src, t.start, &format!("No parameter is defined for position {n}{hint}")));
                        }
                    }
                } else {
                    match self.named.iter().find(|(k, _)| k == name) {
                        Some((_, v)) => v.clone(),
                        None => return Err(parse_error(self.src, t.start, &format!("Unknown query parameter [{name}]"))),
                    }
                };
                Ok(lit(v))
            }
            Tok::Op("(") => {
                self.bump();
                let mut e = self.expr()?;
                let end = self.peek().end;
                if !self.is_op(")") {
                    let n = self.peek().clone();
                    let stop = if matches!(n.tok, Tok::Eof) { self.src.len() } else { n.end };
                    return Err(parse_error(self.src, n.start, &format!("no viable alternative at input '{}'", &self.src[t.start..stop])));
                }
                self.bump();
                e.start = t.start;
                e.end = end;
                Ok(e)
            }
            Tok::Ident(s) if s.eq_ignore_ascii_case("null") => {
                self.bump();
                Ok(lit(V::Null))
            }
            Tok::Ident(s) if s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false") => {
                self.bump();
                Ok(lit(V::Bool(s.eq_ignore_ascii_case("true"))))
            }
            Tok::Ident(_) | Tok::Quoted(_) => {
                // A function call or a column.
                if matches!(t.tok, Tok::Ident(_)) && matches!(self.toks[self.i + 1].tok, Tok::Op("(")) {
                    let Tok::Ident(name) = self.bump().tok else { unreachable!() };
                    self.bump();
                    let mut args = Vec::new();
                    if self.is_op("*") {
                        let s = self.bump();
                        args.push(Expr { kind: ExprKind::Star, start: s.start, end: s.end });
                    } else if !self.is_op(")") {
                        loop {
                            if self.is_op("{") {
                                args.push(self.map_literal()?);
                            } else {
                                args.push(self.expr()?);
                            }
                            if !self.eat_op(",") {
                                break;
                            }
                        }
                    }
                    let end = self.peek().end;
                    self.expect_op(")")?;
                    return Ok(Expr { kind: ExprKind::Call(name.to_ascii_lowercase(), args), start: t.start, end });
                }
                let (name, _) = self.qualified()?;
                let end = self.toks[self.i - 1].end;
                Ok(Expr { kind: ExprKind::Col(name), start: t.start, end })
            }
            _ => Err(self.mismatch(EXPRESSION)),
        }
    }
}

/// A `GROK` pattern compiled with Elasticsearch's legacy bank; a name captured by different
/// groups (`%{NUMBER:x} %{WORD:x}`, or one typed `:int`) is refused.
fn grok(src: &str, pattern: &str, at: usize) -> EsResult<search_painless::grok::Grok> {
    let g = search_painless::grok::Grok::new(search_painless::grok::PatternBank::legacy(), pattern).map_err(|e| parse_error(src, at, &format!("Invalid pattern [{pattern}] for grok: {}", e.message)))?;
    let caps = g.captures_config();
    if let Some(dup) = caps.iter().enumerate().find_map(|(i, c)| caps[..i].iter().any(|p| p.name == c.name).then_some(&c.name)) {
        return Err(parse_error(src, at, &format!("Invalid GROK pattern [{pattern}]: the attribute [{dup}] is defined multiple times with different types")));
    }
    Ok(g)
}

fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
    let (start, end) = (l.start, r.end);
    Expr { kind: ExprKind::Bin(op, Box::new(l), Box::new(r)), start, end }
}

/// The name of an unnamed output: its source text (columns by their name).
fn expr_name(src: &str, e: &Expr) -> String {
    if let ExprKind::Col(c) = &e.kind {
        return c.clone();
    }
    src[e.start..e.end].to_string()
}

/// A request parameter: a value, or (`{"identifier": ...}`, `{"pattern": ...}`, `??name`) a
/// name standing in for a column or a name pattern.
#[derive(Clone, Debug)]
pub enum Param {
    Value(V),
    Identifier(String),
    Pattern(String),
}

/// The normalized text of a time span written as a string (`"3 days"`), if it is one.
pub fn span_text(s: &str) -> Option<String> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = t.split_at(split);
    let unit = unit.trim();
    let n: i64 = n.parse().ok()?;
    super::value::time_unit(unit)?;
    Some(format!("{n} {}", unit.to_ascii_lowercase()))
}

/// Parses a query (`params`: positional `?` / `?1` values, `named`: `?name` values).
pub fn parse(src: &str, params: &[V], named: &[(String, V)]) -> EsResult<Query> {
    let all: Vec<(Option<String>, Param)> = params.iter().map(|v| (None, Param::Value(v.clone()))).chain(named.iter().map(|(k, v)| (Some(k.clone()), Param::Value(v.clone())))).collect();
    parse_with(src, &all)
}

/// Parses a query whose parameters are in request order, each optionally named: `?` takes
/// the next one, `?N` the N-th, `?name` the one of that name (Elasticsearch numbers named
/// parameters too). Identifier and pattern parameters become the names they hold.
pub fn parse_with(src: &str, all: &[(Option<String>, Param)]) -> EsResult<Query> {
    let mut toks = lex(src)?;
    let mut next = 0usize;
    let mut positional: Vec<V> = Vec::with_capacity(all.len());
    for (_, p) in all {
        positional.push(match p {
            Param::Value(v) => v.clone(),
            _ => V::Null,
        });
    }
    let named_values: Vec<(String, V)> = all.iter().filter_map(|(k, p)| Some((k.clone()?, if let Param::Value(v) = p { v.clone() } else { V::Null }))).collect();
    for t in toks.iter_mut() {
        let Tok::Param(name) = &t.tok else { continue };
        let double = src.as_bytes().get(t.start + 1) == Some(&b'?');
        let (idx, label) = if name.is_empty() {
            next += 1;
            (Some(next - 1), (next).to_string())
        } else if let Ok(n) = name.parse::<usize>() {
            (n.checked_sub(1), name.clone())
        } else {
            (all.iter().position(|(k, _)| k.as_deref() == Some(name.as_str())), name.clone())
        };
        let Some(entry) = idx.and_then(|i| all.get(i)) else {
            // Left for the parser, which reports the missing parameter.
            if name.is_empty() {
                t.tok = Tok::Param(String::new());
            }
            continue;
        };
        match &entry.1 {
            Param::Identifier(id) => t.tok = Tok::Quoted(id.clone()),
            Param::Pattern(pat) => t.tok = Tok::Ident(pat.clone()),
            Param::Value(V::Str(id)) if double => t.tok = Tok::Quoted(id.clone()),
            Param::Value(_) => t.tok = Tok::Param(label),
        }
    }
    let named = named_values;
    let params = &positional[..];
    let named = &named[..];
    let mut p = P { src, toks, i: 0, params, named };
    let mut settings = Settings::default();
    // Settings are validated once the whole query parses (syntax errors come first).
    let mut sets: Vec<(usize, String, SetValue)> = Vec::new();
    while p.is_kw("SET") {
        let at = p.bump().start;
        let name = match p.bump() {
            Token { tok: Tok::Ident(n) | Tok::Quoted(n), .. } => n,
            t => return Err(parse_error(src, t.start, &format!("mismatched input '{}' expecting {{UNQUOTED_IDENTIFIER, QUOTED_IDENTIFIER}}", p.token_text(&t)))),
        };
        p.expect_op("=")?;
        let value = if p.is_op("{") {
            // Map literals: skipped (balanced braces).
            let mut depth = 0usize;
            loop {
                match p.peek().tok {
                    Tok::Op("{") => depth += 1,
                    Tok::Op("}") => depth -= 1,
                    Tok::Eof => return Err(p.mismatch("'}'")),
                    _ => {}
                }
                p.bump();
                if depth == 0 {
                    break;
                }
            }
            SetValue::Map
        } else {
            let e = p.primary()?;
            match e.kind {
                ExprKind::Lit(v) => SetValue::Lit(v),
                _ => return Err(parse_error(src, e.start, &format!("mismatched input '{}' expecting {CONSTANT}", &src[e.start..e.end]))),
            }
        };
        if !p.is_op(";") {
            return Err(p.mismatch("';'"));
        }
        p.bump();
        sets.push((at, name, value));
    }
    if settings_seen(&p) && matches!(p.peek().tok, Tok::Eof) {
        let t = p.peek().clone();
        return Err(parse_error(src, t.start, &format!("extraneous input '<EOF>' expecting {SOURCE}")));
    }
    let mut cmds = vec![p.command()?];
    if !matches!(cmds[0], Cmd::From { .. } | Cmd::Ts { .. } | Cmd::Row(_) | Cmd::Show(_) | Cmd::Union { .. }) {
        return Err(parse_error(src, 0, "mismatched input expecting {'from', 'row', 'show'}"));
    }
    loop {
        match p.peek().tok {
            Tok::Eof => break,
            Tok::Pipe => {
                p.bump();
                cmds.push(p.processing()?);
            }
            // A `;` after an expression: ANTLR's expectations there.
            Tok::Op(";") if matches!(cmds.last(), Some(Cmd::Row(_) | Cmd::Eval(_))) => {
                return Err(p.mismatch("{<EOF>, '|', 'and', '::', ',', 'or', '+', '-', '*', '/', '%'}"));
            }
            _ => {
                let t = p.peek().clone();
                return Err(parse_error(src, t.start, &format!("extraneous input '{}' expecting <EOF>", p.token_text(&t))));
            }
        }
    }
    for (at, name, value) in &sets {
        settings.apply(src, *at, name, value)?;
    }
    Ok(Query { src: src.to_string(), cmds, sql: false, locale: None, settings })
}

/// Whether a `SET` statement came before the current token.
fn settings_seen(p: &P<'_>) -> bool {
    p.i > 0 && matches!(p.toks[p.i - 1].tok, Tok::Op(";"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pipelines() {
        let q = parse("FROM esql-test | EVAL duration_ms = ROUND(event.duration / 1000000.0, 1) | STATS COUNT(*), m = MAX(x) BY message | SORT m DESC NULLS LAST | LIMIT 5", &[], &[]).unwrap();
        assert_eq!(q.cmds.len(), 5);
        let Cmd::From { sources, .. } = &q.cmds[0] else { panic!() };
        assert_eq!(sources, &["esql-test"]);
        let Cmd::Stats { aggs, by } = &q.cmds[2] else { panic!() };
        assert_eq!(aggs[0].name, "COUNT(*)");
        assert_eq!(aggs[1].name, "m");
        assert_eq!(by[0].name, "message");
        let Cmd::Sort(keys) = &q.cmds[3] else { panic!() };
        assert!(!keys[0].asc && !keys[0].nulls_first);
    }

    #[test]
    fn set_statements() {
        let q = parse(r#"SET unmapped_fields="Load"; set time_zone = "+04:00"; SET approximation={"rows":{"x":1}}; SET column_metadata=true; ROW x = 1"#, &[], &[]).unwrap();
        assert_eq!(q.settings.unmapped, Unmapped::Load);
        assert_eq!(q.settings.time_zone.as_deref(), Some("+04:00"));
        assert!(q.settings.column_metadata);
        assert!(matches!(q.cmds[0], Cmd::Row(_)));
        let err = |s: &str| parse(s, &[], &[]).unwrap_err().reason().to_string();
        assert_eq!(err(r#"SET no_such_setting="x"; ROW x = 1"#), "line 1:1: Unknown setting [no_such_setting]");
        assert_eq!(err("SET unmapped_fields=1; ROW x = 1"), "line 1:1: Setting [unmapped_fields] must be of type KEYWORD");
        assert_eq!(err(r#"SET column_metadata="x"; ROW x = 1"#), "line 1:1: Setting [column_metadata] must be of type BOOLEAN");
        assert_eq!(
            err(r#"SET unmapped_fields = "fail"; ROW x = 1"#),
            "line 1:1: Error validating setting [unmapped_fields]: Invalid unmapped_fields resolution [fail], must be one of [DEFAULT, NULLIFY, LOAD]"
        );
        // Each statement is located at its own `SET`.
        assert_eq!(err(r#"SET time_zone="Z"; SET b = 1; ROW x = 1"#), "line 1:20: Unknown setting [b]");
        assert_eq!(err(r#"SET time_zone="+04:00" ROW x = 1"#), "line 1:24: mismatched input 'ROW' expecting ';'");
        // Syntax errors come before setting errors.
        assert_eq!(err("SET a = 1;"), "line 1:11: extraneous input '<EOF>' expecting {, 'row', 'from', 'ts', 'promql', 'set', 'show'}");
        assert_eq!(err(r#"SET time_zone="Foo/Bar"; ROW x = 1"#), "line 1:1: Error validating setting [time_zone]: Invalid time zone [Foo/Bar]");
        assert!(err("ROW x = 1; SET a=1").starts_with("line 1:10: mismatched input ';' expecting {<EOF>, '|'"));
    }

    #[test]
    fn errors_are_located() {
        let e = parse("FROM esql-test | EVAL x = ", &[], &[]).unwrap_err();
        assert!(e.reason().starts_with("line 1:27: mismatched input '<EOF>' expecting {QUOTED_STRING"), "{}", e.reason());
        let e = parse("FROM a | FOO", &[], &[]).unwrap_err();
        assert!(e.reason().starts_with("line 1:10: mismatched input 'FOO'"), "{}", e.reason());
    }

    #[test]
    fn lookup_join_rejects_index_patterns() {
        let e = parse("FROM test | SORT key | LOOKUP JOIN test-lookup-1* ON key | LIMIT 3", &[], &[]).unwrap_err();
        assert!(e.reason().contains("line 1:36: invalid index pattern [test-lookup-1*], * is not allowed in LOOKUP JOIN"), "{}", e.reason());
        let e = parse("FROM test-lookup-1 | LOOKUP JOIN test-lookup-* ON key", &[], &[]).unwrap_err();
        assert!(e.reason().contains("line 1:34: invalid index pattern [test-lookup-*], * is not allowed in LOOKUP JOIN"), "{}", e.reason());
        assert!(parse("FROM t | LOOKUP JOIN look-up ON key", &[], &[]).is_ok());
    }

    #[test]
    fn literals_and_strings() {
        let q = parse(r#"ROW a = 1, b = "x\"y", c = """raw "q" """, d = -2.5, e = null, f = 3000000000"#, &[], &[]).unwrap();
        let Cmd::Row(r) = &q.cmds[0] else { panic!() };
        assert!(matches!(r[1].expr.kind, ExprKind::Lit(V::Str(ref s)) if s == "x\"y"));
        assert!(matches!(r[2].expr.kind, ExprKind::Lit(V::Str(ref s)) if s == "raw \"q\" "));
        assert!(matches!(r[3].expr.kind, ExprKind::Lit(V::Double(d)) if d == -2.5));
        assert!(matches!(r[5].expr.kind, ExprKind::Lit(V::Long(3_000_000_000))));
        assert_eq!(location("a\nbc", 3), (2, 2));
    }
}
