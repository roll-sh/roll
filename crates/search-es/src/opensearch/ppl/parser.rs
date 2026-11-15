//! PPL's lexer and parser: a hand-written recursive descent over the grammar of the SQL
//! plugin's `OpenSearchPPLParser.g4` (3.9.0), for the commands roll runs.
//!
//! Keywords are case-insensitive and most of them can also be field names
//! (`keywordsCanBeId`), so the lexer only produces identifiers and the parser decides by
//! position. Every expression keeps its source text the way ANTLR's `getText()` builds it —
//! its tokens concatenated without the whitespace between them — because that is the column
//! name PPL gives an unaliased expression (`count()`, `span(`@timestamp`,1h)`).

use super::value::{Ty, Val};
use crate::error::EsError;

// ------------------------------------------------------------------------------ tokens

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    /// An identifier; `quoted` when written in backticks.
    Ident { name: String, quoted: bool },
    Str(String),
    Int(String),
    /// A decimal literal (`1.5`, `.5`); `suffix` is `d`/`f` for typed literals.
    Dec(String),
    /// A number directly followed by letters (`5m`, `10min`, `1d`): span lengths.
    Span(String),
    Op(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

const OPS: &[&str] = &["==", "!=", "<>", "<=", ">=", "->", "=", "<", ">", "+", "-", "*", "/", "%", "(", ")", "[", "]", ",", ".", "|", "!", ":", "@"];

fn ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '@' || !c.is_ascii() && c.is_alphanumeric()
}

fn ident_part(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '*' || c == '@' || !c.is_ascii() && c.is_alphanumeric()
}

pub fn lex(src: &str) -> Result<Vec<Token>, EsError> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = src[i..].chars().next().expect("char");
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if src[i..].starts_with("//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if src[i..].starts_with("/*") {
            i = src[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
            continue;
        }
        let start = i;
        let tok = if c == '\'' || c == '"' || c == '`' {
            let (s, end) = quoted(src, i, c).ok_or_else(|| syntax(src, start, &src[start..], "token recognition error"))?;
            i = end;
            if c == '`' { Tok::Ident { name: s, quoted: true } } else { Tok::Str(s) }
        } else if c.is_ascii_digit() || (c == '.' && b.get(i + 1).is_some_and(u8::is_ascii_digit) && !prev_is_value(&out)) {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let mut dec = false;
            if j < b.len() && b[j] == b'.' && b.get(j + 1).is_some_and(u8::is_ascii_digit) {
                dec = true;
                j += 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
            }
            if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
                let mut k = j;
                while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'_' || b[k] == b'-' || b[k] == b'*') {
                    k += 1;
                }
                i = k;
                Tok::Span(src[start..k].to_string())
            } else {
                i = j;
                if dec { Tok::Dec(src[start..j].to_string()) } else { Tok::Int(src[start..j].to_string()) }
            }
        } else if ident_start(c) || (c == '*' && src[i + 1..].chars().next().is_some_and(|n| ident_part(n) && n != '-')) {
            let mut j = i + c.len_utf8();
            while let Some(n) = src[j..].chars().next() {
                if ident_part(n) {
                    j += n.len_utf8();
                } else {
                    break;
                }
            }
            // `ID_DATE_SUFFIX`: `logs-2024.01.01` is one name.
            while j < b.len() && b[j] == b'.' && b.get(j + 1).is_some_and(|d| d.is_ascii_digit() || *d == b'*') && src[start..j].contains('-') {
                j += 1;
                while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'*' || b[j] == b'-') {
                    j += 1;
                }
            }
            i = j;
            Tok::Ident { name: src[start..j].to_string(), quoted: false }
        } else if let Some(op) = OPS.iter().find(|o| src[i..].starts_with(**o)) {
            i += op.len();
            Tok::Op(op)
        } else {
            return Err(syntax(src, start, &c.to_string(), "token recognition error"));
        };
        out.push(Token { tok, start, end: i });
    }
    out.push(Token { tok: Tok::Eof, start: b.len(), end: b.len() });
    Ok(out)
}

fn prev_is_value(toks: &[Token]) -> bool {
    matches!(toks.last().map(|t| &t.tok), Some(Tok::Ident { .. } | Tok::Int(_) | Tok::Op(")")))
}

/// A quoted string or identifier starting at `i`: its value (backslash escapes and doubled
/// quotes resolved) and the offset after it.
fn quoted(src: &str, i: usize, q: char) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut it = src[i + 1..].char_indices().peekable();
    while let Some((k, c)) = it.next() {
        if c == '\\' {
            // `StringUtils.unquoteText`: only `\\` and an escaped quote are escapes.
            let (_, n) = it.next()?;
            if n == q || n == '\\' {
                out.push(n);
            } else {
                out.push('\\');
                out.push(n);
            }
        } else if c == q {
            if it.peek().is_some_and(|(_, n)| *n == q) {
                it.next();
                out.push(q);
            } else {
                return Some((out, i + 1 + k + 1));
            }
        } else {
            out.push(c);
        }
    }
    None
}

/// The plugin's `SyntaxCheckException` text for an unexpected token at `at`.
pub fn syntax(src: &str, at: usize, token: &str, detail: &str) -> EsError {
    let end = (at + token.len()).min(src.len());
    let shown = &src[..end];
    let shown = shown.trim_end();
    super::syntax_error(format!("[{token}] is not a valid term at this part of the query: '{shown}' <-- HERE. {detail}"))
}

// ------------------------------------------------------------------------------ AST

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: E,
    /// The expression's `getText()`: its tokens without whitespace.
    pub text: String,
    pub pos: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Xor,
}

impl Op {
    pub fn is_cmp(self) -> bool {
        matches!(self, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge)
    }
}

#[derive(Clone, Debug)]
pub enum E {
    Lit(Val, Ty),
    /// A field path (backticks removed, parts joined with `.`).
    Field(String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
    In(Box<Expr>, Vec<Expr>, bool),
    Between(Box<Expr>, Box<Expr>, Box<Expr>, bool),
    IsNull(Box<Expr>, bool),
    /// A function call, name lower-cased.
    Call(String, Vec<Expr>),
    /// A named argument of a relevance function (`slop=2`).
    Named(String, Box<Expr>),
    Cast(Box<Expr>, String),
    Interval(Box<Expr>, String),
    /// `case(cond, value, ..., else value)`.
    Case(Vec<(Expr, Expr)>, Option<Box<Expr>>),
    /// `span(field, length)` in a `BY` clause: the length and its unit (empty: numeric).
    Span(Box<Expr>, f64, String),
}

#[derive(Clone, Debug)]
pub struct Agg {
    /// Lower-cased function name (`count`, `avg`, `dc`, `percentile`, ...).
    pub func: String,
    pub args: Vec<Expr>,
    /// `count(eval(cond))`.
    pub cond: Option<Expr>,
    /// The output name: the alias, else the call's text.
    pub name: String,
    pub pos: usize,
}

#[derive(Clone, Debug)]
pub struct SortKey {
    pub expr: Expr,
    pub desc: bool,
    /// `str(x)`, `num(x)`, `ip(x)`, `auto(x)`.
    pub cast: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub expr: Expr,
    pub name: String,
}

#[derive(Clone, Debug, Default)]
pub struct BinOpts {
    pub span: Option<(f64, String)>,
    pub bins: Option<i64>,
    pub minspan: Option<(f64, String)>,
    pub start: Option<f64>,
    pub end: Option<f64>,
    pub aligntime: Option<String>,
}

/// A search-command term (`field=value`, a bare literal, time modifiers).
#[derive(Clone, Debug)]
pub enum Search {
    Cmp(String, Op, Val),
    In(String, Vec<Val>),
    Text(String),
    Earliest(String),
    Latest(String),
    Not(Box<Search>),
    And(Box<Search>, Box<Search>),
    Or(Box<Search>, Box<Search>),
}

#[derive(Clone, Debug)]
pub struct Source {
    pub indices: Vec<String>,
    pub filter: Option<Search>,
    pub pos: usize,
    pub alias: Option<String>,
}

// Built once per query and walked once: variant sizes don't matter here.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Cmd {
    Where(Expr),
    Fields { exclude: bool, fields: Vec<(String, usize)> },
    Eval(Vec<(String, Expr)>),
    Stats { aggs: Vec<Agg>, by: Vec<Group>, bucket_nullable: bool },
    EventStats { aggs: Vec<Agg>, by: Vec<Group>, bucket_nullable: bool },
    StreamStats { aggs: Vec<Agg>, by: Vec<Group>, window: usize, current: bool, global: bool },
    Sort { keys: Vec<SortKey>, count: Option<usize> },
    Head { n: usize, from: usize },
    Rename(Vec<(String, String)>),
    Dedup { n: usize, fields: Vec<(String, usize)>, keepempty: bool, consecutive: bool },
    Top { rare: bool, n: usize, fields: Vec<(String, usize)>, by: Vec<(String, usize)>, countfield: String, showcount: bool },
    Bin { field: Expr, opts: BinOpts, alias: Option<String> },
    Timechart { agg: Agg, by: Option<Expr>, span: Option<(f64, String)>, limit: usize, useother: bool, timefield: String },
    Chart { agg: Agg, row: Option<(Expr, BinOpts)>, col: Option<(Expr, BinOpts)>, limit: usize, top: bool, useother: bool, otherstr: String, usenull: bool, nullstr: String },
    Rex { field: String, pattern: String, max_match: usize, sed: bool, offset_field: Option<String> },
    Parse { field: Expr, pattern: String },
    Spath { input: String, output: Option<String>, path: Option<String> },
    Patterns { field: Expr, by: Vec<Group>, method: String, mode: String, max_sample_count: Option<usize>, show_numbered_token: Option<bool>, new_field: String, pattern: Option<String>, variable_count_threshold: Option<usize>, frequency_threshold: Option<f64> },
    Lookup { index: String, pos: usize, keys: Vec<(String, String)>, replace: bool, outputs: Vec<(String, String)> },
    Join { kind: String, left: Option<String>, right: Option<String>, on: Option<Expr>, fields: Vec<String>, sub: Box<Query>, alias: Option<String>, overwrite: bool },
    Append(Box<Query>),
    AppendPipe(Vec<Cmd>),
    Transpose { n: usize, column_name: String },
    FillNull { with: Expr, fields: Vec<String> },
    Reverse,
    Ad,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub explain: bool,
    pub source: Source,
    pub cmds: Vec<Cmd>,
    /// The whole query text (error positions point into it).
    pub text: std::sync::Arc<str>,
}

// ------------------------------------------------------------------------------ parser

pub struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    i: usize,
}

pub fn parse(src: &str) -> Result<Query, EsError> {
    let mut p = Parser { src, toks: lex(src)?, i: 0 };
    let explain = p.kw("explain");
    let q = p.query()?;
    if !matches!(p.peek(), Tok::Eof) {
        let t = &p.toks[p.i];
        let text = src[t.start..t.end].to_string();
        return Err(p.unexpected(&format!("extraneous input '{text}' expecting <EOF>")));
    }
    Ok(Query { explain, ..q })
}

/// Commands that start a pipe segment (anything else there is a syntax error).
const COMMANDS: &[&str] = &[
    "where", "fields", "table", "eval", "stats", "eventstats", "streamstats", "sort", "head", "rename", "dedup", "top", "rare", "bin", "timechart", "chart", "rex", "parse", "spath", "patterns", "lookup", "join", "append", "appendpipe", "transpose", "fillnull", "reverse", "ad", "left", "inner", "right", "full", "outer", "semi", "anti", "cross",
];

impl Parser<'_> {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].tok
    }

    fn peek_at(&self, k: usize) -> &Tok {
        &self.toks[(self.i + k).min(self.toks.len() - 1)].tok
    }

    fn pos(&self) -> usize {
        self.toks[self.i].start
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.i].clone();
        if self.i < self.toks.len() - 1 {
            self.i += 1;
        }
        t
    }

    /// The `getText()` of tokens `from..self.i`.
    fn text_from(&self, from: usize) -> String {
        self.toks[from..self.i].iter().map(|t| &self.src[t.start..t.end]).collect()
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident { name, quoted: false } if name.eq_ignore_ascii_case(kw))
    }

    fn is_kw_at(&self, k: usize, kw: &str) -> bool {
        matches!(self.peek_at(k), Tok::Ident { name, quoted: false } if name.eq_ignore_ascii_case(kw))
    }

    fn kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Tok::Op(o) if *o == op)
    }

    fn op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> Result<(), EsError> {
        if self.op(op) { Ok(()) } else { Err(self.unexpected(&format!("expecting '{op}'"))) }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<(), EsError> {
        if self.kw(kw) { Ok(()) } else { Err(self.unexpected(&format!("expecting '{}'", kw.to_ascii_uppercase()))) }
    }

    fn unexpected(&self, detail: &str) -> EsError {
        let t = &self.toks[self.i];
        let text = if matches!(t.tok, Tok::Eof) { "<EOF>".to_string() } else { self.src[t.start..t.end].to_string() };
        let at = if matches!(t.tok, Tok::Eof) { self.src.len() } else { t.start };
        syntax(self.src, at, &text, detail)
    }

    fn query(&mut self) -> Result<Query, EsError> {
        self.op("|");
        let source = self.search()?;
        let mut cmds = Vec::new();
        while self.op("|") {
            if matches!(self.peek(), Tok::Eof | Tok::Op("]")) {
                break;
            }
            cmds.push(self.command()?);
        }
        Ok(Query { explain: false, source, cmds, text: std::sync::Arc::from(self.src) })
    }

    /// `search? terms* source=... terms*`.
    fn search(&mut self) -> Result<Source, EsError> {
        self.kw("search");
        let mut terms: Vec<Search> = Vec::new();
        let mut source = None;
        while !matches!(self.peek(), Tok::Eof | Tok::Op("|") | Tok::Op("]")) {
            if (self.is_kw("source") || self.is_kw("index")) && matches!(self.peek_at(1), Tok::Op("=")) && source.is_none() {
                self.bump();
                self.bump();
                source = Some(self.table_sources()?);
                continue;
            }
            terms.push(self.search_or()?);
        }
        let (pos, indices, alias) = source.ok_or_else(|| self.unexpected("expecting 'source'"))?;
        let filter = terms.into_iter().reduce(|a, b| Search::And(Box::new(a), Box::new(b)));
        Ok(Source { indices, filter, pos, alias })
    }

    fn table_sources(&mut self) -> Result<(usize, Vec<String>, Option<String>), EsError> {
        let pos = self.pos();
        let mut names = vec![self.table_name()?];
        while self.op(",") {
            names.push(self.table_name()?);
        }
        let alias = if self.kw("as") { Some(self.qualified()?.0) } else { None };
        Ok((pos, names, alias))
    }

    /// An index name or pattern: identifier tokens and dots, as written.
    fn table_name(&mut self) -> Result<String, EsError> {
        let mut s = String::new();
        let mut last_end = None;
        loop {
            let t = &self.toks[self.i];
            if last_end.is_some_and(|e| e != t.start) {
                break;
            }
            match &t.tok {
                Tok::Ident { name, .. } => s.push_str(name),
                Tok::Op(o @ ("." | "*" | "-" | ":")) => s.push_str(o),
                Tok::Int(n) | Tok::Span(n) | Tok::Dec(n) if last_end.is_some() => s.push_str(n),
                Tok::Str(v) if last_end.is_none() => {
                    s.push_str(v);
                    self.bump();
                    break;
                }
                _ => break,
            }
            last_end = Some(t.end);
            self.bump();
        }
        if s.is_empty() { Err(self.unexpected("expecting an index name")) } else { Ok(s) }
    }

    fn search_or(&mut self) -> Result<Search, EsError> {
        let mut l = self.search_and()?;
        while self.kw("or") {
            let r = self.search_and()?;
            l = Search::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn search_and(&mut self) -> Result<Search, EsError> {
        let mut l = self.search_not()?;
        while self.kw("and") {
            let r = self.search_not()?;
            l = Search::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn search_not(&mut self) -> Result<Search, EsError> {
        if self.kw("not") {
            return Ok(Search::Not(Box::new(self.search_not()?)));
        }
        if self.op("(") {
            let s = self.search_or()?;
            self.expect_op(")")?;
            return Ok(s);
        }
        if (self.is_kw("earliest") || self.is_kw("latest")) && matches!(self.peek_at(1), Tok::Op("=")) {
            let latest = self.is_kw("latest");
            self.bump();
            self.bump();
            let v = self.time_modifier()?;
            return Ok(if latest { Search::Latest(v) } else { Search::Earliest(v) });
        }
        // `field op literal`, `field IN (...)`, or a bare literal.
        if matches!(self.peek(), Tok::Ident { .. }) {
            let save = self.i;
            let (field, _) = self.qualified()?;
            let op = match self.peek() {
                Tok::Op("=") | Tok::Op("==") => Some(Op::Eq),
                Tok::Op("!=") | Tok::Op("<>") => Some(Op::Ne),
                Tok::Op("<") => Some(Op::Lt),
                Tok::Op("<=") => Some(Op::Le),
                Tok::Op(">") => Some(Op::Gt),
                Tok::Op(">=") => Some(Op::Ge),
                _ => None,
            };
            if let Some(op) = op {
                self.bump();
                let v = self.search_literal()?;
                return Ok(Search::Cmp(field, op, v));
            }
            if self.is_kw("in") && matches!(self.peek_at(1), Tok::Op("(")) {
                self.bump();
                self.bump();
                let mut vs = vec![self.search_literal()?];
                while self.op(",") {
                    vs.push(self.search_literal()?);
                }
                self.expect_op(")")?;
                return Ok(Search::In(field, vs));
            }
            self.i = save;
        }
        let v = self.search_literal()?;
        Ok(Search::Text(v.to_text(Ty::Str).unwrap_or_default()))
    }

    fn search_literal(&mut self) -> Result<Val, EsError> {
        let neg = self.op("-");
        let t = self.bump();
        Ok(match t.tok {
            Tok::Int(n) => Val::Int(n.parse::<i64>().map(|v| if neg { -v } else { v }).unwrap_or(0)),
            Tok::Dec(n) => Val::Float(n.parse::<f64>().map(|v| if neg { -v } else { v }).unwrap_or(0.0)),
            Tok::Str(s) => Val::Str(s),
            Tok::Ident { name, quoted: false } if name.eq_ignore_ascii_case("true") => Val::Bool(true),
            Tok::Ident { name, quoted: false } if name.eq_ignore_ascii_case("false") => Val::Bool(false),
            Tok::Ident { name, .. } | Tok::Span(name) => Val::Str(name),
            _ => {
                self.i -= 1;
                return Err(self.unexpected("expecting a search term"));
            }
        })
    }

    fn time_modifier(&mut self) -> Result<String, EsError> {
        let start = self.i;
        let t = self.bump();
        match t.tok {
            Tok::Str(s) => Ok(s),
            Tok::Int(n) | Tok::Dec(n) => Ok(n),
            Tok::Ident { name, .. } if name.eq_ignore_ascii_case("now") => {
                if self.op("(") {
                    self.expect_op(")")?;
                }
                Ok("now".into())
            }
            Tok::Op("+") | Tok::Op("-") | Tok::Op("@") => {
                while !matches!(self.peek(), Tok::Eof | Tok::Op("|")) && self.toks[self.i].start == self.toks[self.i - 1].end {
                    self.bump();
                }
                Ok(self.text_from(start))
            }
            _ => {
                self.i = start;
                Err(self.unexpected("expecting a time modifier"))
            }
        }
    }

    /// `ident (. ident)*`: the path and its text.
    fn qualified(&mut self) -> Result<(String, String), EsError> {
        let from = self.i;
        let mut parts = vec![self.ident()?];
        while self.is_op(".") && matches!(self.peek_at(1), Tok::Ident { .. }) {
            self.bump();
            parts.push(self.ident()?);
        }
        Ok((parts.join("."), self.text_from(from)))
    }

    fn ident(&mut self) -> Result<String, EsError> {
        match self.peek().clone() {
            Tok::Ident { name, .. } => {
                self.bump();
                Ok(name)
            }
            _ => Err(self.unexpected("expecting an identifier")),
        }
    }

    fn field_list(&mut self) -> Result<Vec<(String, usize)>, EsError> {
        let mut out = Vec::new();
        loop {
            let pos = self.pos();
            if self.op("*") {
                out.push(("*".to_string(), pos));
            } else {
                out.push((self.qualified()?.0, pos));
            }
            if self.op(",") {
                continue;
            }
            if matches!(self.peek(), Tok::Ident { .. }) && !self.is_kw("by") && !self.is_kw("keepempty") && !self.is_kw("consecutive") && !self.is_kw("as") {
                continue;
            }
            if self.is_op("*") {
                continue;
            }
            return Ok(out);
        }
    }

    fn int_lit(&mut self) -> Result<i64, EsError> {
        let neg = self.op("-");
        if !neg {
            self.op("+");
        }
        match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                let v: i64 = n.parse().map_err(|_| self.unexpected("integer"))?;
                Ok(if neg { -v } else { v })
            }
            _ => Err(self.unexpected("expecting an integer")),
        }
    }

    fn num_lit(&mut self) -> Result<f64, EsError> {
        let neg = self.op("-");
        if !neg {
            self.op("+");
        }
        match self.peek().clone() {
            Tok::Int(n) | Tok::Dec(n) => {
                self.bump();
                let v: f64 = n.parse().unwrap_or(0.0);
                Ok(if neg { -v } else { v })
            }
            _ => Err(self.unexpected("expecting a number")),
        }
    }

    fn bool_lit(&mut self) -> Result<bool, EsError> {
        if self.kw("true") {
            Ok(true)
        } else if self.kw("false") {
            Ok(false)
        } else {
            Err(self.unexpected("expecting a boolean"))
        }
    }

    fn string_lit(&mut self) -> Result<String, EsError> {
        match self.peek().clone() {
            Tok::Str(s) => {
                self.bump();
                Ok(s)
            }
            _ => Err(self.unexpected("expecting a string")),
        }
    }

    /// A span literal: `5m`, `1000`, `1.5h`.
    fn span_lit(&mut self) -> Result<(f64, String), EsError> {
        match self.peek().clone() {
            Tok::Span(s) => {
                self.bump();
                let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
                Ok((s[..split].parse().unwrap_or(0.0), s[split..].to_string()))
            }
            Tok::Int(_) | Tok::Dec(_) => {
                let v = self.num_lit()?;
                // `1.5 d`? no: units are glued to the number.
                Ok((v, String::new()))
            }
            _ => Err(self.unexpected("expecting a span")),
        }
    }

    fn command(&mut self) -> Result<Cmd, EsError> {
        let name = match self.peek() {
            Tok::Ident { name, quoted: false } => name.to_ascii_lowercase(),
            _ => String::new(),
        };
        if !COMMANDS.contains(&name.as_str()) {
            let t = &self.toks[self.i];
            let text = if matches!(t.tok, Tok::Eof) { "<EOF>".to_string() } else { format!("'{}'", &self.src[t.start..t.end]) };
            return Err(self.unexpected(&format!("extraneous input {text} expecting <EOF>")));
        }
        if matches!(name.as_str(), "left" | "inner" | "right" | "full" | "outer" | "semi" | "anti" | "cross") {
            let kind = self.ident()?.to_ascii_lowercase();
            self.expect_kw("join")?;
            return self.join(Some(kind));
        }
        self.bump();
        Ok(match name.as_str() {
            "where" => Cmd::Where(self.logical()?),
            "fields" | "table" => {
                let exclude = self.op("-");
                if !exclude {
                    self.op("+");
                }
                Cmd::Fields { exclude, fields: self.field_list()? }
            }
            "eval" => {
                let mut out = Vec::new();
                loop {
                    let (f, _) = self.qualified()?;
                    self.expect_op("=")?;
                    out.push((f, self.logical()?));
                    if !self.op(",") {
                        break;
                    }
                }
                Cmd::Eval(out)
            }
            "stats" => {
                let mut bucket_nullable = true;
                while self.is_kw("bucket_nullable") || self.is_kw("partitions") || self.is_kw("allnum") || self.is_kw("delim") {
                    let k = self.ident()?.to_ascii_lowercase();
                    self.expect_op("=")?;
                    if k == "bucket_nullable" {
                        bucket_nullable = self.bool_lit()?;
                    } else {
                        self.bump();
                    }
                }
                let aggs = self.aggs()?;
                let by = self.by_clause()?;
                if self.kw("dedup_splitvalues") {
                    self.expect_op("=")?;
                    self.bool_lit()?;
                }
                Cmd::Stats { aggs, by, bucket_nullable }
            }
            "eventstats" => {
                let mut bucket_nullable = true;
                if self.kw("bucket_nullable") {
                    self.expect_op("=")?;
                    bucket_nullable = self.bool_lit()?;
                }
                let aggs = self.aggs()?;
                let by = self.by_clause()?;
                Cmd::EventStats { aggs, by, bucket_nullable }
            }
            "streamstats" => {
                let (mut window, mut current, mut global) = (0, true, true);
                loop {
                    if self.kw("window") {
                        self.expect_op("=")?;
                        window = self.int_lit()?.max(0) as usize;
                    } else if self.kw("current") {
                        self.expect_op("=")?;
                        current = self.bool_lit()?;
                    } else if self.kw("global") {
                        self.expect_op("=")?;
                        global = self.bool_lit()?;
                    } else if self.kw("bucket_nullable") {
                        self.expect_op("=")?;
                        self.bool_lit()?;
                    } else {
                        break;
                    }
                }
                let aggs = self.aggs()?;
                let by = self.by_clause()?;
                Cmd::StreamStats { aggs, by, window, current, global }
            }
            "sort" => {
                let count = if matches!(self.peek(), Tok::Int(_)) { Some(self.int_lit()?.max(0) as usize) } else { None };
                let mut keys = Vec::new();
                loop {
                    let mut desc = if self.op("-") {
                        true
                    } else {
                        self.op("+");
                        false
                    };
                    let cast = ["auto", "str", "ip", "num"].iter().find(|k| self.is_kw(k) && matches!(self.peek_at(1), Tok::Op("("))).map(|k| k.to_string());
                    let expr = if cast.is_some() {
                        self.bump();
                        self.bump();
                        let e = self.field_expr()?;
                        self.expect_op(")")?;
                        e
                    } else {
                        self.field_expr()?
                    };
                    if self.kw("desc") || self.kw("d") {
                        desc = true;
                    } else if self.kw("asc") || self.kw("a") {
                        desc = false;
                    }
                    keys.push(SortKey { expr, desc, cast });
                    if !self.op(",") {
                        break;
                    }
                }
                Cmd::Sort { keys, count }
            }
            "head" => {
                if self.kw("limit") {
                    self.expect_op("=")?;
                }
                let n = if matches!(self.peek(), Tok::Int(_) | Tok::Op("-") | Tok::Op("+")) { self.int_lit()? } else { 10 };
                let from = if self.kw("from") { self.int_lit()? } else { 0 };
                Cmd::Head { n: n.max(0) as usize, from: from.max(0) as usize }
            }
            "rename" => {
                let mut out = Vec::new();
                loop {
                    let a = self.wc_name()?;
                    self.expect_kw("as")?;
                    let b = self.wc_name()?;
                    out.push((a, b));
                    if !self.op(",") && !matches!(self.peek(), Tok::Ident { .. }) {
                        break;
                    }
                }
                Cmd::Rename(out)
            }
            "dedup" => {
                let n = if matches!(self.peek(), Tok::Int(_)) { self.int_lit()?.max(1) as usize } else { 1 };
                let fields = self.field_list()?;
                let (mut keepempty, mut consecutive) = (false, false);
                loop {
                    if self.kw("keepempty") {
                        self.expect_op("=")?;
                        keepempty = self.bool_lit()?;
                    } else if self.kw("consecutive") {
                        self.expect_op("=")?;
                        consecutive = self.bool_lit()?;
                    } else {
                        break;
                    }
                }
                Cmd::Dedup { n, fields, keepempty, consecutive }
            }
            "top" | "rare" => {
                if self.kw("limit") {
                    self.expect_op("=")?;
                }
                let n = if matches!(self.peek(), Tok::Int(_)) { self.int_lit()?.max(0) as usize } else { 10 };
                let (mut countfield, mut showcount) = ("count".to_string(), true);
                loop {
                    if self.kw("countfield") {
                        self.expect_op("=")?;
                        countfield = self.string_lit()?;
                    } else if self.kw("showcount") {
                        self.expect_op("=")?;
                        showcount = self.bool_lit()?;
                    } else if self.is_kw("percentfield") || self.is_kw("showperc") || self.is_kw("usenull") {
                        self.bump();
                        self.expect_op("=")?;
                        self.bump();
                    } else {
                        break;
                    }
                }
                let fields = self.field_list()?;
                let by = if self.kw("by") { self.field_list()? } else { Vec::new() };
                Cmd::Top { rare: name == "rare", n, fields, by, countfield, showcount }
            }
            "bin" => {
                let field = self.field_expr()?;
                let opts = self.bin_opts()?;
                let alias = if self.kw("as") { Some(self.qualified()?.0) } else { None };
                Cmd::Bin { field, opts, alias }
            }
            "timechart" => {
                let (mut span, mut limit, mut useother, mut timefield) = (None, 10usize, true, "@timestamp".to_string());
                let mut params = |p: &mut Self| -> Result<(), EsError> {
                    loop {
                        if p.kw("span") {
                            p.expect_op("=")?;
                            span = Some(p.span_lit()?);
                        } else if p.kw("limit") {
                            p.expect_op("=")?;
                            limit = p.int_lit()?.max(0) as usize;
                        } else if p.kw("useother") {
                            p.expect_op("=")?;
                            useother = match p.bump().tok {
                                Tok::Ident { name, .. } => matches!(name.to_ascii_lowercase().as_str(), "true" | "t"),
                                _ => true,
                            };
                        } else if p.kw("timefield") {
                            p.expect_op("=")?;
                            timefield = match p.bump().tok {
                                Tok::Ident { name, .. } | Tok::Str(name) => name,
                                _ => timefield.clone(),
                            };
                        } else {
                            return Ok(());
                        }
                    }
                };
                params(self)?;
                let agg = self.agg_term()?;
                let by = if self.kw("by") { Some(self.field_expr()?) } else { None };
                params(self)?;
                Cmd::Timechart { agg, by, span, limit, useother, timefield }
            }
            "chart" => {
                let mut c = (10usize, true, true, "OTHER".to_string(), true, "NULL".to_string());
                let mut opts = |p: &mut Self| -> Result<(), EsError> {
                    loop {
                        if p.kw("limit") {
                            p.expect_op("=")?;
                            if p.is_kw("top") || p.is_kw("bottom") {
                                c.1 = p.is_kw("top");
                                p.bump();
                            }
                            c.0 = p.int_lit()?.max(0) as usize;
                        } else if p.kw("useother") {
                            p.expect_op("=")?;
                            c.2 = p.bool_lit()?;
                        } else if p.kw("otherstr") {
                            p.expect_op("=")?;
                            c.3 = p.string_lit()?;
                        } else if p.kw("usenull") {
                            p.expect_op("=")?;
                            c.4 = p.bool_lit()?;
                        } else if p.kw("nullstr") {
                            p.expect_op("=")?;
                            c.5 = p.string_lit()?;
                        } else {
                            return Ok(());
                        }
                    }
                };
                opts(self)?;
                let agg = self.agg_term()?;
                let (mut row, mut col) = (None, None);
                if self.kw("over") {
                    let f = self.field_expr()?;
                    row = Some((f, self.bin_opts()?));
                    if self.kw("by") {
                        let f = self.field_expr()?;
                        col = Some((f, self.bin_opts()?));
                    }
                } else if self.kw("by") {
                    let f = self.field_expr()?;
                    row = Some((f, self.bin_opts()?));
                    self.op(",");
                    if matches!(self.peek(), Tok::Ident { .. }) && !self.is_kw("limit") && !self.is_kw("useother") && !self.is_kw("otherstr") && !self.is_kw("usenull") && !self.is_kw("nullstr") {
                        let f = self.field_expr()?;
                        col = Some((f, self.bin_opts()?));
                    }
                }
                opts(self)?;
                Cmd::Chart { agg, row, col, limit: c.0, top: c.1, useother: c.2, otherstr: c.3, usenull: c.4, nullstr: c.5 }
            }
            "rex" => {
                self.expect_kw("field")?;
                self.expect_op("=")?;
                let field = self.qualified()?.0;
                let (mut max_match, mut sed, mut offset_field, mut pattern) = (1usize, false, None, None);
                loop {
                    if self.kw("max_match") {
                        self.expect_op("=")?;
                        max_match = self.int_lit()?.max(0) as usize;
                    } else if self.kw("mode") {
                        self.expect_op("=")?;
                        sed = self.ident()?.eq_ignore_ascii_case("sed");
                    } else if self.kw("offset_field") {
                        self.expect_op("=")?;
                        offset_field = Some(self.qualified()?.0);
                    } else if matches!(self.peek(), Tok::Str(_)) && pattern.is_none() {
                        pattern = Some(self.string_lit()?);
                    } else {
                        break;
                    }
                }
                Cmd::Rex { field, pattern: pattern.ok_or_else(|| self.unexpected("expecting a pattern"))?, max_match, sed, offset_field }
            }
            "parse" => {
                let field = self.value_expr()?;
                let pattern = self.string_lit()?;
                Cmd::Parse { field, pattern }
            }
            "spath" => {
                let (mut input, mut output, mut path) = (None, None, None);
                while !matches!(self.peek(), Tok::Eof | Tok::Op("|") | Tok::Op("]")) {
                    if self.is_kw("input") && matches!(self.peek_at(1), Tok::Op("=")) {
                        self.bump();
                        self.bump();
                        input = Some(self.qualified()?.0);
                    } else if self.is_kw("output") && matches!(self.peek_at(1), Tok::Op("=")) {
                        self.bump();
                        self.bump();
                        output = Some(self.qualified()?.0);
                    } else {
                        if self.is_kw("path") && matches!(self.peek_at(1), Tok::Op("=")) {
                            self.bump();
                            self.bump();
                        }
                        path = Some(match self.peek().clone() {
                            Tok::Str(s) => {
                                self.bump();
                                s
                            }
                            _ => self.path_text()?,
                        });
                    }
                }
                Cmd::Spath { input: input.ok_or_else(|| self.unexpected("expecting input"))?, output, path }
            }
            "patterns" => {
                let field = self.value_expr()?;
                let by = self.by_clause()?;
                let mut c = Cmd::Patterns { field, by, method: String::new(), mode: String::new(), max_sample_count: None, show_numbered_token: None, new_field: "patterns_field".into(), pattern: None, variable_count_threshold: None, frequency_threshold: None };
                if let Cmd::Patterns { method, mode, max_sample_count, show_numbered_token, new_field, pattern, variable_count_threshold, frequency_threshold, .. } = &mut c {
                    loop {
                        let k = match self.peek() {
                            Tok::Ident { name, quoted: false } if matches!(self.peek_at(1), Tok::Op("=")) => name.to_ascii_lowercase(),
                            _ => break,
                        };
                        self.bump();
                        self.bump();
                        match k.as_str() {
                            "method" => *method = self.ident()?.to_ascii_lowercase(),
                            "mode" => *mode = self.ident()?.to_ascii_lowercase(),
                            "max_sample_count" => *max_sample_count = Some(self.int_lit()?.max(0) as usize),
                            "buffer_limit" => {
                                self.int_lit()?;
                            }
                            "show_numbered_token" => *show_numbered_token = Some(self.bool_lit()?),
                            "new_field" => *new_field = self.string_lit()?,
                            "pattern" => *pattern = Some(self.string_lit()?),
                            "variable_count_threshold" => *variable_count_threshold = Some(self.int_lit()?.max(0) as usize),
                            "frequency_threshold_percentage" => *frequency_threshold = Some(self.num_lit()?),
                            _ => return Err(self.unexpected("unknown patterns parameter")),
                        }
                    }
                }
                c
            }
            "lookup" => {
                let pos = self.pos();
                let index = self.table_name()?;
                let mut keys = Vec::new();
                loop {
                    let a = self.qualified()?.0;
                    let b = if self.kw("as") { self.qualified()?.0 } else { a.clone() };
                    keys.push((a, b));
                    if !self.op(",") {
                        break;
                    }
                }
                let mut replace = true;
                let mut outputs = Vec::new();
                if self.is_kw("append") || self.is_kw("replace") || self.is_kw("output") {
                    replace = !self.is_kw("append");
                    self.bump();
                    loop {
                        let a = self.qualified()?.0;
                        let b = if self.kw("as") { self.qualified()?.0 } else { a.clone() };
                        outputs.push((a, b));
                        if !self.op(",") {
                            break;
                        }
                    }
                }
                Cmd::Lookup { index, pos, keys, replace, outputs }
            }
            "join" => return self.join(None),
            "append" => {
                self.expect_op("[")?;
                let q = self.query()?;
                self.expect_op("]")?;
                Cmd::Append(Box::new(q))
            }
            "appendpipe" => {
                self.expect_op("[")?;
                self.op("|");
                let mut cmds = vec![self.command()?];
                while self.op("|") {
                    cmds.push(self.command()?);
                }
                self.expect_op("]")?;
                Cmd::AppendPipe(cmds)
            }
            "transpose" => {
                let (mut n, mut column_name) = (5usize, "column".to_string());
                loop {
                    if matches!(self.peek(), Tok::Int(_)) {
                        n = self.int_lit()?.max(0) as usize;
                    } else if self.kw("column_name") {
                        self.expect_op("=")?;
                        column_name = self.string_lit()?;
                    } else {
                        break;
                    }
                }
                Cmd::Transpose { n, column_name }
            }
            "fillnull" => {
                if self.kw("with") {
                    let with = self.value_expr()?;
                    let fields = if self.kw("in") { self.field_list()?.into_iter().map(|f| f.0).collect() } else { Vec::new() };
                    Cmd::FillNull { with, fields }
                } else {
                    self.expect_kw("value")?;
                    self.expect_op("=")?;
                    let with = self.value_expr()?;
                    let fields = if matches!(self.peek(), Tok::Ident { .. }) { self.field_list()?.into_iter().map(|f| f.0).collect() } else { Vec::new() };
                    Cmd::FillNull { with, fields }
                }
            }
            "reverse" => Cmd::Reverse,
            "ad" => {
                while !matches!(self.peek(), Tok::Eof | Tok::Op("|") | Tok::Op("]")) {
                    self.bump();
                }
                Cmd::Ad
            }
            _ => unreachable!("listed command"),
        })
    }

    fn join(&mut self, kind: Option<String>) -> Result<Cmd, EsError> {
        let mut kind = kind.unwrap_or_else(|| "inner".into());
        let (mut left, mut right, mut overwrite) = (None, None, true);
        loop {
            if self.is_kw("type") && matches!(self.peek_at(1), Tok::Op("=")) {
                self.bump();
                self.bump();
                kind = self.ident()?.to_ascii_lowercase();
            } else if self.is_kw("overwrite") && matches!(self.peek_at(1), Tok::Op("=")) {
                self.bump();
                self.bump();
                overwrite = self.bool_lit()?;
            } else if self.is_kw("max") && matches!(self.peek_at(1), Tok::Op("=")) {
                self.bump();
                self.bump();
                self.int_lit()?;
            } else if self.is_kw("left") && matches!(self.peek_at(1), Tok::Op("=")) {
                self.bump();
                self.bump();
                left = Some(self.qualified()?.0);
                self.op(",");
            } else if self.is_kw("right") && matches!(self.peek_at(1), Tok::Op("=")) {
                self.bump();
                self.bump();
                right = Some(self.qualified()?.0);
            } else {
                break;
            }
        }
        let mut on = None;
        let mut fields = Vec::new();
        if self.kw("on") || self.kw("where") {
            on = Some(self.logical()?);
        } else if !self.is_op("[") {
            // `join a, b index` (a field list then the right side).
            while matches!(self.peek(), Tok::Ident { .. }) && !matches!(self.peek_at(1), Tok::Eof | Tok::Op("|")) {
                fields.push(self.qualified()?.0);
                self.op(",");
            }
        }
        let (sub, alias) = if self.op("[") {
            let q = self.query()?;
            self.expect_op("]")?;
            let alias = if self.kw("as") { Some(self.qualified()?.0) } else { None };
            (q, alias)
        } else {
            let (pos, indices, alias) = self.table_sources()?;
            (Query { explain: false, source: Source { indices, filter: None, pos, alias: alias.clone() }, cmds: Vec::new(), text: std::sync::Arc::from(self.src) }, alias)
        };
        Ok(Cmd::Join { kind, left, right, on, fields, sub: Box::new(sub), alias, overwrite })
    }

    /// A rename / alias name, `*` wildcards allowed.
    fn wc_name(&mut self) -> Result<String, EsError> {
        let mut s = String::new();
        let mut last_end: Option<usize> = None;
        loop {
            let t = &self.toks[self.i];
            if last_end.is_some_and(|e| e != t.start) {
                break;
            }
            match &t.tok {
                Tok::Ident { name, quoted: false } if last_end.is_none() || !name.eq_ignore_ascii_case("as") => s.push_str(name),
                Tok::Ident { name, quoted: true } => s.push_str(name),
                Tok::Op("*") => s.push('*'),
                Tok::Op(".") => s.push('.'),
                Tok::Str(v) if last_end.is_none() => {
                    s.push_str(v);
                    self.bump();
                    break;
                }
                _ => break,
            }
            last_end = Some(t.end);
            self.bump();
        }
        if s.is_empty() { Err(self.unexpected("expecting a field name")) } else { Ok(s) }
    }

    fn path_text(&mut self) -> Result<String, EsError> {
        let from = self.i;
        let mut last_end: Option<usize> = None;
        while !matches!(self.peek(), Tok::Eof | Tok::Op("|")) {
            let t = &self.toks[self.i];
            if last_end.is_some_and(|e| e != t.start) {
                break;
            }
            last_end = Some(t.end);
            self.bump();
        }
        if self.i == from {
            return Err(self.unexpected("expecting a path"));
        }
        Ok(self.text_from(from))
    }

    fn bin_opts(&mut self) -> Result<BinOpts, EsError> {
        let mut o = BinOpts::default();
        loop {
            if self.is_kw("span") && matches!(self.peek_at(1), Tok::Op("=")) {
                self.bump();
                self.bump();
                o.span = Some(self.span_lit()?);
            } else if self.is_kw("bins") && matches!(self.peek_at(1), Tok::Op("=")) {
                self.bump();
                self.bump();
                o.bins = Some(self.int_lit()?);
            } else if self.is_kw("minspan") && matches!(self.peek_at(1), Tok::Op("=")) {
                self.bump();
                self.bump();
                o.minspan = Some(self.span_lit()?);
            } else if self.is_kw("start") && matches!(self.peek_at(1), Tok::Op("=")) {
                self.bump();
                self.bump();
                o.start = Some(self.num_lit()?);
            } else if self.is_kw("end") && matches!(self.peek_at(1), Tok::Op("=")) {
                self.bump();
                self.bump();
                o.end = Some(self.num_lit()?);
            } else if self.is_kw("aligntime") && matches!(self.peek_at(1), Tok::Op("=")) {
                self.bump();
                self.bump();
                let from = self.i;
                self.bump();
                o.aligntime = Some(self.text_from(from));
            } else {
                return Ok(o);
            }
        }
    }

    fn aggs(&mut self) -> Result<Vec<Agg>, EsError> {
        let mut out = vec![self.agg_term()?];
        while self.op(",") {
            out.push(self.agg_term()?);
        }
        Ok(out)
    }

    fn agg_term(&mut self) -> Result<Agg, EsError> {
        let from = self.i;
        let pos = self.pos();
        let fname = match self.peek().clone() {
            Tok::Ident { name, quoted: false } => name.to_ascii_lowercase(),
            _ => return Err(self.unexpected("expecting an aggregation")),
        };
        self.bump();
        let mut args = Vec::new();
        let mut cond = None;
        let func = match fname.as_str() {
            "c" | "count" if !self.is_op("(") => "count".to_string(),
            f => {
                self.expect_op("(")?;
                if (f == "count" || f == "c") && self.is_kw("eval") && matches!(self.peek_at(1), Tok::Op("(")) {
                    self.bump();
                    self.bump();
                    cond = Some(self.logical()?);
                    self.expect_op(")")?;
                } else if !self.is_op(")") {
                    args.push(self.logical()?);
                    while self.op(",") {
                        args.push(self.logical()?);
                    }
                }
                self.expect_op(")")?;
                match f {
                    "c" => "count".into(),
                    "distinct_count" | "distinct_count_approx" => "dc".into(),
                    "percentile_approx" => "percentile".into(),
                    "stddev" => "stddev_samp".into(),
                    "var" => "var_samp".into(),
                    _ if f.starts_with("perc") && f.len() > 4 && f[4..].chars().all(|c| c.is_ascii_digit() || c == '_') => {
                        args.push(Expr { kind: E::Lit(Val::Float(f[4..].replace('_', ".").parse().unwrap_or(50.0)), Ty::Double), text: String::new(), pos });
                        "percentile".into()
                    }
                    _ if f.starts_with('p') && f.len() > 1 && f[1..].chars().all(|c| c.is_ascii_digit() || c == '_') => {
                        args.push(Expr { kind: E::Lit(Val::Float(f[1..].replace('_', ".").parse().unwrap_or(50.0)), Ty::Double), text: String::new(), pos });
                        "percentile".into()
                    }
                    _ => f.to_string(),
                }
            }
        };
        // Unaliased, an aggregation is named by its call as written, arguments joined by ", "
        // (`percentile(a, 50)`, `COUNT()`), not by the text's own spacing.
        let text = match (&self.toks[from].tok, self.toks.get(from + 1).map(|t| &t.tok)) {
            (Tok::Ident { name, .. }, Some(Tok::Op("("))) => {
                let inner: Vec<String> = match &cond {
                    Some(c) => vec![format!("eval({})", c.text)],
                    None => args.iter().filter(|a| !a.text.is_empty()).map(|a| a.text.clone()).collect(),
                };
                format!("{name}({})", inner.join(", "))
            }
            _ => self.text_from(from),
        };
        let name = if self.kw("as") { self.wc_name()? } else { text };
        Ok(Agg { func, args, cond, name, pos })
    }

    /// `BY fields, span(...) [as x]` in either order.
    fn by_clause(&mut self) -> Result<Vec<Group>, EsError> {
        if !self.kw("by") {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        loop {
            let from = self.i;
            let pos = self.pos();
            if self.is_kw("span") && matches!(self.peek_at(1), Tok::Op("(")) {
                self.bump();
                self.bump();
                let field = if matches!(self.peek_at(1), Tok::Op(",") | Tok::Op(".")) || matches!(self.peek(), Tok::Ident { .. }) { Some(self.field_expr()?) } else { None };
                if field.is_some() {
                    self.expect_op(",")?;
                }
                let (len, unit) = self.span_lit()?;
                self.expect_op(")")?;
                let text = self.text_from(from);
                let field = field.unwrap_or_else(|| Expr { kind: E::Field("@timestamp".into()), text: "@timestamp".into(), pos });
                let expr = Expr { kind: E::Span(Box::new(field), len, unit), text: text.clone(), pos };
                let name = if self.kw("as") { self.qualified()?.0 } else { text };
                out.push(Group { expr, name });
            } else {
                let expr = self.field_expr()?;
                let name = match &expr.kind {
                    E::Field(f) => f.clone(),
                    _ => expr.text.clone(),
                };
                out.push(Group { expr, name });
            }
            if self.op(",") {
                continue;
            }
            if matches!(self.peek(), Tok::Ident { quoted: true, .. }) || matches!(self.peek(), Tok::Ident { name, .. } if !["dedup_splitvalues", "method", "mode", "max_sample_count", "buffer_limit", "show_numbered_token", "new_field", "pattern", "variable_count_threshold", "frequency_threshold_percentage"].contains(&name.to_ascii_lowercase().as_str())) {
                continue;
            }
            return Ok(out);
        }
    }

    fn field_expr(&mut self) -> Result<Expr, EsError> {
        let from = self.i;
        let pos = self.pos();
        let (path, _) = self.qualified()?;
        Ok(Expr { kind: E::Field(path), text: self.text_from(from), pos })
    }

    // ------------------------------------------------------------------ expressions

    pub fn logical(&mut self) -> Result<Expr, EsError> {
        let from = self.i;
        let mut l = self.xor_expr()?;
        while self.kw("or") {
            let r = self.xor_expr()?;
            l = self.mk(from, E::Bin(Op::Or, Box::new(l), Box::new(r)));
        }
        Ok(l)
    }

    fn xor_expr(&mut self) -> Result<Expr, EsError> {
        let from = self.i;
        let mut l = self.and_expr()?;
        while self.kw("xor") {
            let r = self.and_expr()?;
            l = self.mk(from, E::Bin(Op::Xor, Box::new(l), Box::new(r)));
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> Result<Expr, EsError> {
        let from = self.i;
        let mut l = self.not_expr()?;
        while self.kw("and") {
            let r = self.not_expr()?;
            l = self.mk(from, E::Bin(Op::And, Box::new(l), Box::new(r)));
        }
        Ok(l)
    }

    fn not_expr(&mut self) -> Result<Expr, EsError> {
        let from = self.i;
        if self.is_kw("not") && !matches!(self.peek_at(1), Tok::Op("(")) || self.is_kw("not") && matches!(self.peek_at(1), Tok::Op("(")) {
            self.bump();
            let e = self.not_expr()?;
            return Ok(self.mk(from, E::Not(Box::new(e))));
        }
        self.predicate()
    }

    fn mk(&self, from: usize, kind: E) -> Expr {
        Expr { kind, text: self.text_from(from), pos: self.toks[from].start }
    }

    fn predicate(&mut self) -> Result<Expr, EsError> {
        let from = self.i;
        let mut l = self.value_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Op("=") | Tok::Op("==") => Some(Op::Eq),
                Tok::Op("!=") | Tok::Op("<>") => Some(Op::Ne),
                Tok::Op("<") => Some(Op::Lt),
                Tok::Op("<=") => Some(Op::Le),
                Tok::Op(">") => Some(Op::Gt),
                Tok::Op(">=") => Some(Op::Ge),
                _ => None,
            };
            if let Some(op) = op {
                self.bump();
                let r = self.value_expr()?;
                l = self.mk(from, E::Bin(op, Box::new(l), Box::new(r)));
                continue;
            }
            let not = self.is_kw("not") && (self.is_kw_at(1, "in") || self.is_kw_at(1, "between") || self.is_kw_at(1, "like"));
            if not {
                self.bump();
            }
            if self.is_kw("in") && matches!(self.peek_at(1), Tok::Op("(")) {
                self.bump();
                self.bump();
                let mut vs = vec![self.value_expr()?];
                while self.op(",") {
                    vs.push(self.value_expr()?);
                }
                self.expect_op(")")?;
                l = self.mk(from, E::In(Box::new(l), vs, not));
                continue;
            }
            if self.kw("between") {
                let lo = self.value_expr()?;
                self.expect_kw("and")?;
                let hi = self.value_expr()?;
                l = self.mk(from, E::Between(Box::new(l), Box::new(lo), Box::new(hi), not));
                continue;
            }
            if self.kw("like") {
                let r = self.value_expr()?;
                let call = self.mk(from, E::Call("like".into(), vec![l, r]));
                l = if not { self.mk(from, E::Not(Box::new(call))) } else { call };
                continue;
            }
            if not {
                self.i -= 1;
            }
            if self.is_kw("is") {
                self.bump();
                let not = self.kw("not");
                self.expect_kw("null")?;
                l = self.mk(from, E::IsNull(Box::new(l), not));
                continue;
            }
            return Ok(l);
        }
    }

    pub fn value_expr(&mut self) -> Result<Expr, EsError> {
        let from = self.i;
        let mut l = self.term()?;
        loop {
            let op = match self.peek() {
                Tok::Op("+") => Op::Add,
                Tok::Op("-") => Op::Sub,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.term()?;
            l = self.mk(from, E::Bin(op, Box::new(l), Box::new(r)));
        }
    }

    fn term(&mut self) -> Result<Expr, EsError> {
        let from = self.i;
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Op("*") => Op::Mul,
                Tok::Op("/") => Op::Div,
                Tok::Op("%") => Op::Mod,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.unary()?;
            l = self.mk(from, E::Bin(op, Box::new(l), Box::new(r)));
        }
    }

    fn unary(&mut self) -> Result<Expr, EsError> {
        let from = self.i;
        if self.op("-") {
            let e = self.unary()?;
            return Ok(match e.kind {
                E::Lit(Val::Int(i), t) => self.mk(from, E::Lit(Val::Int(-i), t)),
                E::Lit(Val::Float(f), t) => self.mk(from, E::Lit(Val::Float(-f), t)),
                _ => self.mk(from, E::Neg(Box::new(e))),
            });
        }
        if self.op("+") {
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, EsError> {
        let from = self.i;
        let t = self.toks[self.i].clone();
        match t.tok {
            Tok::Op("(") => {
                self.bump();
                let e = self.logical()?;
                self.expect_op(")")?;
                Ok(Expr { text: self.text_from(from), ..e })
            }
            Tok::Str(s) => {
                self.bump();
                Ok(self.mk(from, E::Lit(Val::Str(s), Ty::Str)))
            }
            Tok::Int(n) => {
                self.bump();
                Ok(match n.parse::<i64>() {
                    Ok(v) if i32::try_from(v).is_ok() => self.mk(from, E::Lit(Val::Int(v), Ty::Int)),
                    Ok(v) => self.mk(from, E::Lit(Val::Int(v), Ty::Long)),
                    Err(_) => self.mk(from, E::Lit(Val::Float(n.parse().unwrap_or(f64::INFINITY)), Ty::Double)),
                })
            }
            Tok::Dec(n) => {
                self.bump();
                Ok(self.mk(from, E::Lit(Val::Float(n.parse().unwrap_or(0.0)), Ty::Double)))
            }
            Tok::Span(s) => {
                self.bump();
                let lower = s.to_ascii_lowercase();
                if let Some(num) = lower.strip_suffix('d').filter(|n| n.contains('.')) {
                    return Ok(self.mk(from, E::Lit(Val::Float(num.parse().unwrap_or(0.0)), Ty::Double)));
                }
                if let Some(num) = lower.strip_suffix('f').filter(|n| n.contains('.')) {
                    return Ok(self.mk(from, E::Lit(Val::Float(num.parse::<f32>().unwrap_or(0.0) as f64), Ty::Float)));
                }
                // A span length used as a value (`span(x, 5m)` is parsed elsewhere).
                Ok(self.mk(from, E::Lit(Val::Str(s), Ty::Str)))
            }
            Tok::Ident { ref name, quoted: false } => {
                let lower = name.to_ascii_lowercase();
                let call = matches!(self.peek_at(1), Tok::Op("("));
                match lower.as_str() {
                    "true" | "false" if !call => {
                        self.bump();
                        Ok(self.mk(from, E::Lit(Val::Bool(lower == "true"), Ty::Bool)))
                    }
                    "null" if !call => {
                        self.bump();
                        Ok(self.mk(from, E::Lit(Val::Null, Ty::Undefined)))
                    }
                    "interval" if !call => {
                        self.bump();
                        let v = self.value_expr()?;
                        let unit = self.ident()?.to_ascii_uppercase();
                        Ok(self.mk(from, E::Interval(Box::new(v), unit)))
                    }
                    "cast" if call => {
                        self.bump();
                        self.bump();
                        let e = self.logical()?;
                        self.expect_kw("as")?;
                        let ty = self.ident()?.to_ascii_uppercase();
                        self.expect_op(")")?;
                        Ok(self.mk(from, E::Cast(Box::new(e), ty)))
                    }
                    "case" if call => {
                        self.bump();
                        self.bump();
                        let mut arms = Vec::new();
                        let mut other = None;
                        loop {
                            if self.kw("else") {
                                other = Some(Box::new(self.value_expr()?));
                                break;
                            }
                            let c = self.logical()?;
                            self.expect_op(",")?;
                            let v = self.value_expr()?;
                            arms.push((c, v));
                            if !self.op(",") {
                                if self.kw("else") {
                                    other = Some(Box::new(self.value_expr()?));
                                }
                                break;
                            }
                        }
                        self.expect_op(")")?;
                        Ok(self.mk(from, E::Case(arms, other)))
                    }
                    _ if call => {
                        self.bump();
                        self.bump();
                        let mut args = Vec::new();
                        if !self.is_op(")") {
                            loop {
                                // Relevance functions take `name=value` options.
                                if let (Tok::Ident { name, quoted: false }, Tok::Op("=")) = (self.peek().clone(), self.peek_at(1).clone()) {
                                    let afrom = self.i;
                                    self.bump();
                                    self.bump();
                                    let v = self.value_expr()?;
                                    args.push(self.mk(afrom, E::Named(name.to_ascii_lowercase(), Box::new(v))));
                                } else if matches!(lower.as_str(), "timestampadd" | "timestampdiff" | "extract") && args.is_empty() && matches!(self.peek(), Tok::Ident { .. }) && matches!(self.peek_at(1), Tok::Op(",") | Tok::Ident { .. }) {
                                    let afrom = self.i;
                                    let unit = self.ident()?.to_ascii_uppercase();
                                    args.push(self.mk(afrom, E::Lit(Val::Str(unit), Ty::Str)));
                                    if lower == "extract" {
                                        self.expect_kw("from")?;
                                        args.push(self.logical()?);
                                        break;
                                    }
                                } else {
                                    args.push(self.logical()?);
                                }
                                if !self.op(",") {
                                    break;
                                }
                            }
                        }
                        self.expect_op(")")?;
                        Ok(self.mk(from, E::Call(lower, args)))
                    }
                    _ => self.field_expr(),
                }
            }
            Tok::Ident { .. } => self.field_expr(),
            _ => Err(self.unexpected("expecting an expression")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expression_text_drops_whitespace() {
        let q = parse("source=t | stats count(), avg(x) as a by span(`@timestamp`, 1h)").unwrap();
        let Cmd::Stats { aggs, by, .. } = &q.cmds[0] else { panic!() };
        assert_eq!(aggs[0].name, "count()");
        assert_eq!(aggs[1].name, "a");
        assert_eq!(by[0].name, "span(`@timestamp`,1h)");
    }

    #[test]
    fn index_patterns_and_search_terms() {
        let q = parse("search earliest='2024-01-15 10:30:05' source=log-* status=200 | head 5 from 1").unwrap();
        assert_eq!(q.source.indices, vec!["log-*"]);
        assert!(q.source.filter.is_some());
        assert!(matches!(q.cmds[0], Cmd::Head { n: 5, from: 1 }));
    }

    #[test]
    fn unknown_commands_are_syntax_errors() {
        let e = parse("source=t | invalid_command_xyz").unwrap_err();
        assert!(e.reason().contains("is not a valid term"), "{}", e.reason());
    }
}
