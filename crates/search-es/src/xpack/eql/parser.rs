//! The EQL parser: a hand-written lexer and recursive-descent parser for Elasticsearch's EQL
//! grammar (`EqlBase.g4`), producing [`Statement`]s.
//!
//! The grammar's shape is kept exactly, because it decides what a query means: keywords are
//! lowercase only (`WHERE` is an identifier, so `process WHERE true` fails like it does on
//! Elasticsearch), `not` binds tighter than `and`/`or`, comparisons don't chain, a predicate
//! (`in`, `:`, `like`, `regex`) attaches to the primary expression before it, and unary minus
//! binds tighter than `*`. Errors carry Elasticsearch's `line L:C:` prefix (columns are 1-based
//! character positions) and, for the checks the planner makes while building the plan
//! (sequence shape, `maxspan`, `runs`, pipes), Elasticsearch's texts.

use crate::error::EsError;

/// A literal value.
#[derive(Clone, Debug, PartialEq)]
pub enum Lit {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// The kinds of pattern predicates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchKind {
    /// `:` (case-insensitive equality, wildcards when the pattern has `*` or `?`).
    Seq,
    Like { insensitive: bool },
    Regex { insensitive: bool },
}

/// An expression with its source span (byte offsets into the query).
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Lit(Lit),
    Field { name: String, optional: bool },
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    In { expr: Box<Expr>, list: Vec<Expr>, insensitive: bool, negated: bool },
    Match { kind: MatchKind, expr: Box<Expr>, patterns: Vec<Expr> },
    Call { name: String, insensitive: bool, args: Vec<Expr> },
}

/// `category where condition` (`category: None` is `any`).
#[derive(Clone, Debug, PartialEq)]
pub struct EventFilter {
    pub category: Option<String>,
    pub condition: Expr,
}

/// A `[...]` term of a sequence or sample, with its own join keys.
#[derive(Clone, Debug, PartialEq)]
pub struct Term {
    pub filter: EventFilter,
    /// Join keys: the query-level `by` keys followed by the term's.
    pub keys: Vec<Expr>,
    /// `![...]`: an event that must not happen.
    pub missing: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    Event(EventFilter),
    /// Terms repeated `with runs=N` are already expanded.
    Sequence { terms: Vec<Term>, until: Option<Term>, maxspan_nanos: Option<i64> },
    Sample { terms: Vec<Term> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pipe {
    Head(i32),
    Tail(i32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Statement {
    pub query: Query,
    pub pipes: Vec<Pipe>,
}

// --- lexer -----------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Kw(&'static str),
    Ident(String),
    QuotedIdent(String),
    TildeIdent(String),
    Str(String),
    Int(String),
    Decimal(String),
    Op(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    start: usize,
    end: usize,
}

const KEYWORDS: &[&str] = &[
    "and", "any", "by", "false", "in", "join", "like", "maxspan", "not", "null", "of", "or", "regex", "sample", "sequence", "true", "until", "where", "with",
];

/// `line L:C: msg`, with Elasticsearch's 1-based columns.
pub fn error_at(src: &str, pos: usize, msg: impl AsRef<str>) -> EsError {
    let (line, col) = line_col(src, pos);
    let msg = msg.as_ref();
    let e = EsError::new(400, "parsing_exception", format!("line {line}:{col}: {msg}"));
    // ANTLR's recognition exception travels as the (reason-less) cause.
    if msg.starts_with("mismatched input '") {
        e.caused_by(EsError::java(400, "input_mismatch_exception", "").with_null_reason())
    } else if msg.starts_with("no viable alternative") {
        e.caused_by(EsError::java(400, "no_viable_alt_exception", "").with_null_reason())
    } else {
        e
    }
}

/// A `ParsingException` the AST builder throws: Elasticsearch prints its column one past the
/// offending token's start (syntax errors from the grammar are exact, see [`error_at`]).
pub fn semantic_at(src: &str, pos: usize, msg: impl AsRef<str>) -> EsError {
    error_at(src, pos + 1, msg)
}

/// 1-based line and column (in characters) of a byte offset.
pub fn line_col(src: &str, pos: usize) -> (usize, usize) {
    let pos = pos.min(src.len());
    let before = &src[..pos];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (line, before[line_start..].chars().count() + 1)
}

fn lex(src: &str) -> Result<Vec<Token>, EsError> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
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
            // Bracketed comments nest, as in the grammar.
            let start = i;
            let mut depth = 0usize;
            loop {
                if i + 1 >= b.len() {
                    return Err(error_at(src, start, format!("token recognition error at: '{}'", &src[start..])));
                }
                if b[i] == b'/' && b[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        let start = i;
        let tok = if c.is_ascii_alphabetic() || c == b'_' || c == b'@' {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let word = &src[i..j];
            let tilde = c.is_ascii_alphabetic() && b.get(j) == Some(&b'~');
            if tilde && matches!(word, "in" | "like" | "regex") {
                i = j + 1;
                Tok::Kw(match word {
                    "in" => "in~",
                    "like" => "like~",
                    _ => "regex~",
                })
            } else if let Some(k) = KEYWORDS.iter().find(|k| **k == word) {
                i = j;
                Tok::Kw(k)
            } else if tilde {
                i = j + 1;
                Tok::TildeIdent(word.to_string())
            } else {
                i = j;
                Tok::Ident(word.to_string())
            }
        } else if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let mut decimal = false;
            if j < b.len() && b[j] == b'.' {
                decimal = true;
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
                    while k < b.len() && b[k].is_ascii_digit() {
                        k += 1;
                    }
                    decimal = true;
                    j = k;
                }
            }
            i = j;
            if decimal { Tok::Decimal(src[start..j].to_string()) } else { Tok::Int(src[start..j].to_string()) }
        } else if c == b'"' || c == b'\'' || (c == b'?' && matches!(b.get(i + 1), Some(b'"') | Some(b'\''))) {
            let (s, j) = lex_string(src, i)?;
            i = j;
            Tok::Str(s)
        } else if c == b'`' {
            let mut j = i + 1;
            let mut name = String::new();
            loop {
                if j >= b.len() {
                    return Err(error_at(src, start, format!("token recognition error at: '{}'", &src[start..])));
                }
                if b[j] == b'`' {
                    if b.get(j + 1) == Some(&b'`') {
                        name.push('`');
                        j += 2;
                        continue;
                    }
                    j += 1;
                    break;
                }
                let ch = src[j..].chars().next().expect("in bounds");
                name.push(ch);
                j += ch.len_utf8();
            }
            i = j;
            Tok::QuotedIdent(name)
        } else {
            let two = src.get(i..i + 2).unwrap_or("");
            let op: &'static str = match two {
                "==" => "==",
                "!=" => "!=",
                "<=" => "<=",
                ">=" => ">=",
                "![" => "![",
                _ => match c {
                    b'<' => "<",
                    b'>' => ">",
                    b'=' => "=",
                    b':' => ":",
                    b'+' => "+",
                    b'-' => "-",
                    b'*' => "*",
                    b'/' => "/",
                    b'%' => "%",
                    b'.' => ".",
                    b',' => ",",
                    b'[' => "[",
                    b']' => "]",
                    b'(' => "(",
                    b')' => ")",
                    b'|' => "|",
                    b'?' => "?",
                    _ => {
                        let ch = src[i..].chars().next().expect("in bounds");
                        return Err(error_at(src, i, format!("token recognition error at: '{ch}'")));
                    }
                },
            };
            i += op.len();
            Tok::Op(op)
        };
        out.push(Token { tok, start, end: i });
    }
    out.push(Token { tok: Tok::Eof, start: src.len(), end: src.len() });
    Ok(out)
}

/// Lexes and unquotes a string literal starting at `i`; returns it and the end offset.
fn lex_string(src: &str, i: usize) -> Result<(String, usize), EsError> {
    let b = src.as_bytes();
    let unterminated = |at: usize| error_at(src, at, format!("token recognition error at: '{}'", &src[at..]));
    // `?"..."` and `?'...'`: the old raw strings, rejected after lexing like Elasticsearch.
    if b[i] == b'?' {
        let q = b[i + 1];
        let mut j = i + 2;
        while j < b.len() && b[j] != q {
            if b[j] == b'\n' || b[j] == b'\r' {
                return Err(unterminated(i));
            }
            if b[j] == b'\\' && b.get(j + 1) == Some(&q) {
                j += 1;
            }
            j += 1;
        }
        if j >= b.len() {
            return Err(unterminated(i));
        }
        return Err(semantic_at(src, i, "Use triple double quotes [\"\"\"] to define unescaped string literals, not [?\"]"));
    }
    if b[i] == b'\'' {
        let mut j = i + 1;
        while j < b.len() && b[j] != b'\'' {
            if b[j] == b'\n' || b[j] == b'\r' {
                return Err(unterminated(i));
            }
            if b[j] == b'\\' {
                j += 1;
            }
            j += 1;
        }
        if j >= b.len() {
            return Err(unterminated(i));
        }
        return Err(semantic_at(src, i, "Use double quotes [\"] to define string literals, not single quotes [']"));
    }
    // `"""raw"""` (up to two extra quotes belong to the literal's end, as in the grammar).
    if src[i..].starts_with("\"\"\"") {
        let body_start = i + 3;
        let Some(rel) = src[body_start..].find("\"\"\"") else { return Err(unterminated(i)) };
        let body = &src[body_start..body_start + rel];
        if body.contains('\n') || body.contains('\r') {
            return Err(unterminated(i));
        }
        let mut end = body_start + rel + 3;
        let mut extra = 0;
        while extra < 2 && b.get(end) == Some(&b'"') {
            end += 1;
            extra += 1;
        }
        let mut s = body.to_string();
        s.push_str(&"\"".repeat(extra));
        return Ok((s, end));
    }
    let mut j = i + 1;
    let mut out = String::new();
    loop {
        let Some(&c) = b.get(j) else { return Err(unterminated(i)) };
        match c {
            b'"' => return Ok((out, j + 1)),
            b'\n' | b'\r' => return Err(unterminated(i)),
            b'\\' => {
                let Some(&e) = b.get(j + 1) else { return Err(unterminated(i)) };
                match e {
                    b'b' => out.push('\u{8}'),
                    b't' => out.push('\t'),
                    b'n' => out.push('\n'),
                    b'f' => out.push('\u{c}'),
                    b'r' => out.push('\r'),
                    b'"' => out.push('"'),
                    b'\'' => out.push('\''),
                    b'\\' => out.push('\\'),
                    b'u' if b.get(j + 2) == Some(&b'{') => {
                        let close = src[j + 3..].find('}').map(|r| j + 3 + r).ok_or_else(|| unterminated(i))?;
                        let hex = &src[j + 3..close];
                        if hex.is_empty() || !hex.bytes().all(|h| h.is_ascii_hexdigit()) {
                            return Err(unterminated(i));
                        }
                        if !(2..=8).contains(&hex.len()) {
                            return Err(semantic_at(
                                src,
                                i,
                                format!("Unicode sequence should use [2-8] hex digits; [{hex}] has [{}]", hex.len()),
                            ));
                        }
                        let cp = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32).ok_or_else(|| {
                            semantic_at(src, i, format!("Invalid unicode character code [{hex}]"))
                        })?;
                        if (0xD800..=0xDFFF).contains(&(cp as u32)) {
                            return Err(semantic_at(src, i, format!("Invalid unicode character code, [{hex}] is a surrogate code")));
                        }
                        out.push(cp);
                        j = close + 1;
                        continue;
                    }
                    _ => return Err(error_at(src, j, format!("token recognition error at: '{}'", &src[i..(j + 2).min(src.len())]))),
                }
                j += 2;
            }
            _ => {
                let ch = src[j..].chars().next().expect("in bounds");
                out.push(ch);
                j += ch.len_utf8();
            }
        }
    }
}

// --- parser ----------------------------------------------------------------------------------

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    pos: usize,
}

/// How a token reads in `mismatched input '...'` messages.
fn tok_text(src: &str, t: &Token) -> String {
    match t.tok {
        Tok::Eof => "<EOF>".to_string(),
        _ => src[t.start..t.end].to_string(),
    }
}

/// The expected-token set ANTLR names where an expression must start.
const EXPR_START: &str = "{'false', 'null', 'true', '+', '-', '(', '?', STRING, INTEGER_VALUE, DECIMAL_VALUE, IDENTIFIER, QUOTED_IDENTIFIER, TILDE_IDENTIFIER}";
const EXPR_START_NOT: &str = "{'false', 'not', 'null', 'true', '+', '-', '(', '?', STRING, INTEGER_VALUE, DECIMAL_VALUE, IDENTIFIER, QUOTED_IDENTIFIER, TILDE_IDENTIFIER}";

impl<'a> Parser<'a> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }

    fn cur(&self) -> &Token {
        &self.toks[self.pos]
    }

    fn start(&self) -> usize {
        self.cur().start
    }

    fn prev_end(&self) -> usize {
        if self.pos == 0 { 0 } else { self.toks[self.pos - 1].end }
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn is_kw(&self, k: &str) -> bool {
        matches!(self.peek(), Tok::Kw(x) if *x == k)
    }

    fn is_op(&self, o: &str) -> bool {
        matches!(self.peek(), Tok::Op(x) if *x == o)
    }

    fn eat_kw(&mut self, k: &str) -> bool {
        if self.is_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_op(&mut self, o: &str) -> bool {
        if self.is_op(o) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn mismatched(&self, expecting: &str) -> EsError {
        let t = self.cur();
        error_at(self.src, t.start, format!("mismatched input '{}' expecting {expecting}", tok_text(self.src, t)))
    }

    fn expect_op(&mut self, o: &str) -> Result<(), EsError> {
        if self.eat_op(o) {
            return Ok(());
        }
        let t = self.cur();
        if matches!(t.tok, Tok::Eof) {
            return Err(self.mismatched(&format!("'{o}'")));
        }
        Err(error_at(self.src, t.start, format!("missing '{o}' at '{}'", tok_text(self.src, t))))
    }

    fn expect_kw(&mut self, k: &str) -> Result<(), EsError> {
        if self.eat_kw(k) {
            return Ok(());
        }
        let t = self.cur();
        if matches!(t.tok, Tok::Eof) {
            return Err(self.mismatched(&format!("'{k}'")));
        }
        Err(error_at(self.src, t.start, format!("missing '{k}' at '{}'", tok_text(self.src, t))))
    }

    fn statement(&mut self) -> Result<Statement, EsError> {
        let query = self.query()?;
        let mut pipes = Vec::new();
        let is_sample = matches!(query, Query::Sample { .. });
        while self.is_op("|") {
            let pipe_start = self.start();
            self.bump();
            let name_tok = self.cur().clone();
            let Tok::Ident(name) = name_tok.tok.clone() else { return Err(self.mismatched("IDENTIFIER")) };
            self.bump();
            let mut args = Vec::new();
            if !self.is_op("|") && !matches!(self.peek(), Tok::Eof) {
                args.push(self.boolean()?);
                while self.eat_op(",") {
                    args.push(self.boolean()?);
                }
            }
            if is_sample {
                return Err(semantic_at(self.src, pipe_start, "Samples do not support pipes yet"));
            }
            pipes.push(self.pipe(&name, &name_tok, pipe_start, args)?);
        }
        if !matches!(self.peek(), Tok::Eof) {
            return Err(self.mismatched("{<EOF>, '|'}"));
        }
        Ok(Statement { query, pipes })
    }

    fn pipe(&self, name: &str, name_tok: &Token, pipe_start: usize, args: Vec<Expr>) -> Result<Pipe, EsError> {
        const SUPPORTED: [&str; 7] = ["count", "filter", "head", "sort", "tail", "unique", "unique_count"];
        if !SUPPORTED.contains(&name) {
            let similar = find_similar(name, &SUPPORTED);
            let mut msg = format!("Unrecognized pipe [{name}]");
            if !similar.is_empty() {
                let list = format!("[{}]", similar.join(", "));
                msg.push_str(&format!(", did you mean {}?", if similar.len() == 1 { list } else { format!("any of {list}") }));
            }
            return Err(semantic_at(self.src, name_tok.start, msg));
        }
        if name != "head" && name != "tail" {
            return Err(semantic_at(self.src, pipe_start, format!("Pipe [{name}] is not supported")));
        }
        if args.len() != 1 {
            return Err(semantic_at(self.src, pipe_start, format!("Pipe [{name}] expects exactly one argument but found [{}]", args.len())));
        }
        let a = &args[0];
        let value = fold_int(a);
        match value {
            Some(v) if (0..=i32::MAX as i64).contains(&v) => Ok(if name == "head" { Pipe::Head(v as i32) } else { Pipe::Tail(v as i32) }),
            _ => Err(semantic_at(self.src, a.start, format!("Pipe [{name}] expects a positive integer but found [{}]", &self.src[a.start..a.end]))),
        }
    }

    fn query(&mut self) -> Result<Query, EsError> {
        if self.is_kw("sequence") {
            return self.sequence();
        }
        if self.is_kw("sample") {
            return self.sample();
        }
        if self.is_kw("join") {
            return self.join();
        }
        Ok(Query::Event(self.event_filter()?))
    }

    fn join_keys(&mut self) -> Result<Vec<Expr>, EsError> {
        // `by` already consumed.
        let start = self.start();
        let mut keys = vec![self.boolean()?];
        while self.eat_op(",") {
            keys.push(self.boolean()?);
        }
        if keys.iter().any(|k| !matches!(k.kind, ExprKind::Field { .. })) {
            return Err(semantic_at(self.src, start, "Unsupported join key "));
        }
        Ok(keys)
    }

    fn sequence_params(&mut self) -> Result<i64, EsError> {
        // `with` already consumed.
        self.expect_kw("maxspan")?;
        self.expect_op("=")?;
        let num_start = self.start();
        let num = self.bump();
        let unit = match self.peek() {
            Tok::Ident(u) => {
                let u = u.clone();
                let t = self.bump();
                Some((u, t))
            }
            _ => None,
        };
        let text_end = unit.as_ref().map_or(num.end, |(_, t)| t.end);
        let text = &self.src[num_start..text_end];
        let value = match &num.tok {
            Tok::Int(s) => s.parse::<i64>().map_err(|_| semantic_at(self.src, num.start, format!("Number [{s}] is too large")))?,
            Tok::Decimal(s) => {
                return Err(semantic_at(self.src, num.start, format!("Decimal time interval [{s}] not supported; please use an positive integer")));
            }
            _ => {
                self.pos -= 1 + usize::from(unit.is_some());
                return Err(self.mismatched("{INTEGER_VALUE, DECIMAL_VALUE}"));
            }
        };
        if value <= 0 {
            return Err(semantic_at(self.src, num.start, format!("A positive maxspan value is required; found [{value}]")));
        }
        let Some((unit, unit_tok)) = unit else {
            return Err(semantic_at(self.src, num_start, format!("No time unit specified, did you mean [s] as in [{text}s]?")));
        };
        let nanos_per: i64 = match unit.as_str() {
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            "d" => 86_400_000_000_000,
            _ => {
                return Err(semantic_at(
                    self.src,
                    unit_tok.start,
                    format!("Unrecognized time unit [{unit}] in [{text}], please specify one of [ms, s, m, h, d]"),
                ));
            }
        };
        Ok(value.saturating_mul(nanos_per))
    }

    fn sequence(&mut self) -> Result<Query, EsError> {
        let seq_start = self.start();
        self.bump();
        let mut parent_keys = Vec::new();
        let mut maxspan = None;
        if self.eat_kw("by") {
            parent_keys = self.join_keys()?;
            if self.eat_kw("with") {
                maxspan = Some(self.sequence_params()?);
            }
        } else if self.eat_kw("with") {
            maxspan = Some(self.sequence_params()?);
            if self.eat_kw("by") {
                self.join_keys()?;
                return Err(semantic_at(self.src, seq_start, "Please specify sequence [by] before [with] not after"));
            }
        }
        let mut terms: Vec<Term> = Vec::new();
        let mut number_of_keys: Option<usize> = None;
        let mut until = None;
        loop {
            let is_until = self.eat_kw("until");
            if !is_until && !self.is_op("[") && !self.is_op("![") {
                break;
            }
            let term_start = self.start();
            let (filter, missing) = self.subquery()?;
            let mut keys = parent_keys.clone();
            let mut by_start = None;
            if self.is_kw("by") {
                by_start = Some(self.toks[self.pos].start);
                self.bump();
                keys.extend(self.join_keys()?);
            }
            let mut runs = 1i64;
            if !is_until && self.eat_kw("with") {
                let key_tok = self.cur().clone();
                let key = match &key_tok.tok {
                    Tok::Ident(k) => k.clone(),
                    Tok::Kw(k) => {
                        // `with maxspan=...` after a term: the grammar expects an identifier.
                        let _ = k;
                        return Err(self.mismatched("IDENTIFIER"));
                    }
                    _ => return Err(self.mismatched("IDENTIFIER")),
                };
                self.bump();
                self.expect_op("=")?;
                let num = self.bump();
                if key != "runs" {
                    return Err(semantic_at(self.src, key_tok.start, format!("Unrecognized option [{key}], expecting [runs]")));
                }
                match &num.tok {
                    Tok::Int(s) => {
                        let v = s.parse::<i64>().unwrap_or(i64::MAX);
                        if v < 1 {
                            return Err(semantic_at(self.src, num.start, format!("A positive runs value is required; found [{v}]")));
                        }
                        if v > 100 {
                            return Err(semantic_at(self.src, num.start, format!("A query cannot be repeated more than 100 times; found [{v}]")));
                        }
                        runs = v;
                    }
                    Tok::Decimal(_) => {}
                    _ => {
                        self.pos -= 1;
                        return Err(self.mismatched("{INTEGER_VALUE, DECIMAL_VALUE}"));
                    }
                }
            }
            match number_of_keys {
                None => number_of_keys = Some(keys.len()),
                Some(n) if n != keys.len() => {
                    let at = by_start.unwrap_or(term_start);
                    return Err(semantic_at(
                        self.src,
                        at,
                        format!(
                            "Inconsistent number of join keys specified; expected [{}] but found [{}]",
                            n - parent_keys.len(),
                            keys.len() - parent_keys.len()
                        ),
                    ));
                }
                _ => {}
            }
            let term = Term { filter, keys, missing };
            if is_until {
                until = Some(term);
                break;
            }
            if terms.len() as i64 + runs > 256 {
                return Err(semantic_at(self.src, term_start, format!("Sequence cannot contain more than 256 queries; found [{}]", terms.len() as i64 + runs)));
            }
            for _ in 0..runs {
                terms.push(term.clone());
            }
        }
        if terms.is_empty() && until.is_none() {
            return Err(self.mismatched("{'[', '![', 'by', 'with'}"));
        }
        if terms.len() < 2 {
            return Err(semantic_at(self.src, seq_start, format!("A sequence requires a minimum of 2 queries, found [{}]", terms.len())));
        }
        if maxspan.is_none() && terms.iter().any(|t| t.missing) {
            return Err(semantic_at(self.src, seq_start, "[maxspan] is required for sequences with missing events queries; found none"));
        }
        if terms.iter().all(|t| t.missing) {
            return Err(semantic_at(self.src, seq_start, "A sequence requires at least one positive event query; found none"));
        }
        Ok(Query::Sequence { terms, until, maxspan_nanos: maxspan })
    }

    fn sample(&mut self) -> Result<Query, EsError> {
        let start = self.start();
        self.bump();
        let parent_keys = if self.eat_kw("by") { self.join_keys()? } else { Vec::new() };
        let mut terms: Vec<Term> = Vec::new();
        let mut number_of_keys: Option<usize> = None;
        let mut missing_keys_at = None;
        while self.is_op("[") || self.is_op("![") {
            let term_start = self.start();
            if self.is_op("![") {
                return Err(EsError::new(400, "parsing_exception", "Missing events are supported only for sequences"));
            }
            let (filter, _) = self.subquery()?;
            let mut keys = parent_keys.clone();
            let mut by_start = None;
            if self.is_kw("by") {
                by_start = Some(self.toks[self.pos].start);
                self.bump();
                keys.extend(self.join_keys()?);
            }
            match number_of_keys {
                None => number_of_keys = Some(keys.len()),
                Some(n) if n != keys.len() => {
                    return Err(semantic_at(
                        self.src,
                        by_start.unwrap_or(term_start),
                        format!(
                            "Inconsistent number of join keys specified; expected [{}] but found [{}]",
                            n - parent_keys.len(),
                            keys.len() - parent_keys.len()
                        ),
                    ));
                }
                _ => {}
            }
            if keys.is_empty() && missing_keys_at.is_none() {
                missing_keys_at = Some(term_start);
            }
            if terms.len() + 1 > 5 {
                return Err(semantic_at(self.src, term_start, format!("A sample cannot contain more than 5 queries, found [{}]", terms.len() + 1)));
            }
            let mut seen = std::collections::HashSet::new();
            let mut dups = Vec::new();
            for k in &keys {
                if let ExprKind::Field { name, .. } = &k.kind
                    && !seen.insert(name.clone())
                    && !dups.contains(name)
                {
                    dups.push(name.clone());
                }
            }
            if !dups.is_empty() {
                return Err(semantic_at(
                    self.src,
                    by_start.unwrap_or(term_start),
                    format!("Join keys must be used only once, found duplicates: [{}]", dups.join(",")),
                ));
            }
            terms.push(Term { filter, keys, missing: false });
        }
        if terms.is_empty() {
            return Err(self.mismatched("{'[', 'by'}"));
        }
        if terms.len() < 2 {
            return Err(semantic_at(self.src, start, format!("A sample requires a minimum of 2 queries, found [{}]", terms.len())));
        }
        if let Some(at) = missing_keys_at {
            return Err(semantic_at(self.src, at, "A sample must have at least one join key, found none"));
        }
        Ok(Query::Sample { terms })
    }

    /// `join` parses, then fails verification like Elasticsearch.
    fn join(&mut self) -> Result<Query, EsError> {
        let start = self.start();
        self.bump();
        if self.eat_kw("by") {
            self.join_keys()?;
        }
        let mut n = 0;
        while self.is_op("[") || self.is_op("![") {
            if self.is_op("![") {
                return Err(EsError::new(400, "parsing_exception", "Missing events are supported only for sequences"));
            }
            self.subquery()?;
            if self.eat_kw("by") {
                self.join_keys()?;
            }
            n += 1;
        }
        if n < 2 {
            return Err(self.mismatched("'['"));
        }
        if self.eat_kw("until") {
            self.subquery()?;
            if self.eat_kw("by") {
                self.join_keys()?;
            }
        }
        let (line, col) = line_col(self.src, start);
        Err(EsError::new(400, "verification_exception", format!("Found 1 problem\nline {line}:{col}: JOIN command is not supported")))
    }

    fn subquery(&mut self) -> Result<(EventFilter, bool), EsError> {
        let missing = if self.eat_op("![") {
            true
        } else if self.eat_op("[") {
            false
        } else {
            return Err(self.mismatched("{'[', '!['}"));
        };
        let f = self.event_filter()?;
        self.expect_op("]")?;
        Ok((f, missing))
    }

    fn event_filter(&mut self) -> Result<EventFilter, EsError> {
        let category = match self.peek().clone() {
            Tok::Kw("any") => {
                self.bump();
                None
            }
            Tok::Ident(s) | Tok::Str(s) => {
                self.bump();
                Some(s)
            }
            _ => return Err(self.mismatched("{'any', 'join', 'sample', 'sequence', STRING, IDENTIFIER}")),
        };
        self.expect_kw("where")?;
        let condition = self.boolean()?;
        Ok(EventFilter { category, condition })
    }

    // booleanExpression: OR < AND < NOT.
    fn boolean(&mut self) -> Result<Expr, EsError> {
        let mut left = self.and_expr()?;
        while self.is_kw("or") {
            self.bump();
            let right = self.and_expr()?;
            let (start, end) = (left.start, right.end);
            left = Expr { kind: ExprKind::Or(Box::new(left), Box::new(right)), start, end };
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr, EsError> {
        let mut left = self.not_expr()?;
        while self.is_kw("and") {
            self.bump();
            let right = self.not_expr()?;
            let (start, end) = (left.start, right.end);
            left = Expr { kind: ExprKind::And(Box::new(left), Box::new(right)), start, end };
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Expr, EsError> {
        if self.is_kw("not") {
            let start = self.start();
            self.bump();
            let inner = self.not_expr()?;
            let end = inner.end;
            return Ok(Expr { kind: ExprKind::Not(Box::new(inner)), start, end });
        }
        // `relationship of [subquery]`.
        if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Kw("of")) {
            let start = self.start();
            self.bump();
            self.bump();
            self.subquery()?;
            return Err(error_at(self.src, start, "Process relationships are not supported"));
        }
        self.value_expr()
    }

    fn value_expr(&mut self) -> Result<Expr, EsError> {
        let left = self.operator_expr()?;
        let op = match self.peek() {
            Tok::Op("==") => CmpOp::Eq,
            Tok::Op("!=") => CmpOp::Neq,
            Tok::Op("<") => CmpOp::Lt,
            Tok::Op("<=") => CmpOp::Lte,
            Tok::Op(">") => CmpOp::Gt,
            Tok::Op(">=") => CmpOp::Gte,
            Tok::Op("=") => {
                let t = self.cur();
                return Err(error_at(self.src, t.start, "mismatched input '=' expecting {<EOF>, 'and', 'or', '==', '!=', '<', '<=', '>', '>=', '+', '-', '*', '/', '%', '|'}"));
            }
            _ => return Ok(left),
        };
        self.bump();
        let right = self.operator_expr()?;
        let (start, end) = (left.start, right.end);
        Ok(Expr { kind: ExprKind::Cmp(op, Box::new(left), Box::new(right)), start, end })
    }

    // operatorExpression: + - < * / % < unary.
    fn operator_expr(&mut self) -> Result<Expr, EsError> {
        let mut left = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Op("+") => ArithOp::Add,
                Tok::Op("-") => ArithOp::Sub,
                _ => return Ok(left),
            };
            self.bump();
            let right = self.mul_expr()?;
            let (start, end) = (left.start, right.end);
            left = Expr { kind: ExprKind::Arith(op, Box::new(left), Box::new(right)), start, end };
        }
    }

    fn mul_expr(&mut self) -> Result<Expr, EsError> {
        let mut left = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Op("*") => ArithOp::Mul,
                Tok::Op("/") => ArithOp::Div,
                Tok::Op("%") => ArithOp::Mod,
                _ => return Ok(left),
            };
            self.bump();
            let right = self.unary_expr()?;
            let (start, end) = (left.start, right.end);
            left = Expr { kind: ExprKind::Arith(op, Box::new(left), Box::new(right)), start, end };
        }
    }

    fn unary_expr(&mut self) -> Result<Expr, EsError> {
        if self.is_op("-") || self.is_op("+") {
            let start = self.start();
            let neg = self.is_op("-");
            self.bump();
            let inner = self.unary_expr()?;
            let end = inner.end;
            if !neg {
                return Ok(Expr { kind: inner.kind, start, end });
            }
            // A negated number literal folds, so `-9223372036854775808` stays a long.
            if let ExprKind::Lit(l) = &inner.kind {
                match l {
                    Lit::Int(i) => return Ok(Expr { kind: ExprKind::Lit(Lit::Int(i.wrapping_neg())), start, end }),
                    Lit::Double(d) => return Ok(Expr { kind: ExprKind::Lit(Lit::Double(-d)), start, end }),
                    _ => {}
                }
            }
            return Ok(Expr { kind: ExprKind::Neg(Box::new(inner)), start, end });
        }
        let primary = self.primary()?;
        self.predicate(primary)
    }

    fn predicate(&mut self, expr: Expr) -> Result<Expr, EsError> {
        let start = expr.start;
        let negated = self.is_kw("not") && matches!(self.peek_at(1), Tok::Kw("in") | Tok::Kw("in~"));
        if negated {
            self.bump();
        }
        if self.is_kw("in") || self.is_kw("in~") {
            let insensitive = self.is_kw("in~");
            self.bump();
            self.expect_op("(")?;
            let mut list = vec![self.boolean()?];
            while self.eat_op(",") {
                list.push(self.boolean()?);
            }
            self.expect_op(")")?;
            let end = self.prev_end();
            return Ok(Expr { kind: ExprKind::In { expr: Box::new(expr), list, insensitive, negated }, start, end });
        }
        let kind = match self.peek() {
            Tok::Op(":") => MatchKind::Seq,
            Tok::Kw("like") => MatchKind::Like { insensitive: false },
            Tok::Kw("like~") => MatchKind::Like { insensitive: true },
            Tok::Kw("regex") => MatchKind::Regex { insensitive: false },
            Tok::Kw("regex~") => MatchKind::Regex { insensitive: true },
            _ => return Ok(expr),
        };
        self.bump();
        let mut patterns = Vec::new();
        if self.eat_op("(") {
            patterns.push(self.constant()?);
            while self.eat_op(",") {
                patterns.push(self.constant()?);
            }
            self.expect_op(")")?;
        } else {
            patterns.push(self.constant()?);
        }
        let end = self.prev_end();
        Ok(Expr { kind: ExprKind::Match { kind, expr: Box::new(expr), patterns }, start, end })
    }

    fn constant(&mut self) -> Result<Expr, EsError> {
        let t = self.cur().clone();
        let lit = match &t.tok {
            Tok::Kw("null") => Lit::Null,
            Tok::Kw("true") => Lit::Bool(true),
            Tok::Kw("false") => Lit::Bool(false),
            Tok::Str(s) => Lit::Str(s.clone()),
            Tok::Int(s) => int_literal(self.src, &t, s)?,
            Tok::Decimal(s) => Lit::Double(s.parse().map_err(|_| semantic_at(self.src, t.start, format!("Cannot parse number [{s}]")))?),
            _ => return Err(self.mismatched("{'false', 'null', 'true', STRING, INTEGER_VALUE, DECIMAL_VALUE}")),
        };
        self.bump();
        Ok(Expr { kind: ExprKind::Lit(lit), start: t.start, end: t.end })
    }

    fn primary(&mut self) -> Result<Expr, EsError> {
        let t = self.cur().clone();
        match &t.tok {
            Tok::Kw("null" | "true" | "false") | Tok::Str(_) | Tok::Int(_) | Tok::Decimal(_) => self.constant(),
            Tok::Op("(") => {
                self.bump();
                let mut inner = self.boolean()?;
                self.expect_op(")")?;
                inner.start = t.start;
                inner.end = self.prev_end();
                Ok(inner)
            }
            Tok::TildeIdent(name) => {
                self.bump();
                if !self.is_op("(") {
                    return Err(self.mismatched("'('"));
                }
                self.call(name.clone(), true, t.start)
            }
            Tok::Ident(name) if matches!(self.peek_at(1), Tok::Op("(")) => {
                self.bump();
                self.call(name.clone(), false, t.start)
            }
            Tok::Ident(_) | Tok::QuotedIdent(_) | Tok::Op("?") => self.qualified_name(),
            _ => Err(self.mismatched(if self.pos > 0 && matches!(self.toks[self.pos - 1].tok, Tok::Kw("where")) { EXPR_START_NOT } else { EXPR_START })),
        }
    }

    fn call(&mut self, name: String, insensitive: bool, start: usize) -> Result<Expr, EsError> {
        self.expect_op("(")?;
        let mut args = Vec::new();
        if !self.is_op(")") {
            args.push(self.boolean()?);
            while self.eat_op(",") {
                args.push(self.boolean()?);
            }
        }
        self.expect_op(")")?;
        Ok(Expr { kind: ExprKind::Call { name, insensitive, args }, start, end: self.prev_end() })
    }

    fn qualified_name(&mut self) -> Result<Expr, EsError> {
        let start = self.start();
        let optional = self.eat_op("?");
        let mut name = self.identifier()?;
        loop {
            if self.is_op(".") {
                self.bump();
                name.push('.');
                name.push_str(&self.identifier()?);
            } else if matches!(self.peek(), Tok::Decimal(d) if d.starts_with('.') && d[1..].bytes().all(|b| b.is_ascii_digit())) {
                // `a.1` lexes as IDENTIFIER DECIMAL; the grammar has no rule for it.
                return Err(self.mismatched("{<EOF>, 'and', 'or'}"));
            } else if self.is_op("[") && matches!(self.peek_at(1), Tok::Int(_)) {
                let at = self.start();
                return Err(semantic_at(self.src, at, "Array indexes are not supported"));
            } else {
                break;
            }
        }
        Ok(Expr { kind: ExprKind::Field { name, optional }, start, end: self.prev_end() })
    }

    fn identifier(&mut self) -> Result<String, EsError> {
        match self.peek().clone() {
            Tok::Ident(s) | Tok::QuotedIdent(s) => {
                self.bump();
                Ok(s)
            }
            _ => Err(self.mismatched("{IDENTIFIER, QUOTED_IDENTIFIER}")),
        }
    }
}

fn int_literal(src: &str, t: &Token, s: &str) -> Result<Lit, EsError> {
    match s.parse::<i64>() {
        Ok(v) => Ok(Lit::Int(v)),
        // Too large for a long: quietly a double, like Elasticsearch.
        Err(_) => s.parse::<f64>().map(Lit::Double).map_err(|_| semantic_at(src, t.start, format!("Number [{s}] is too large"))),
    }
}

/// Folds an integer-valued constant expression (pipe arguments).
fn fold_int(e: &Expr) -> Option<i64> {
    match &e.kind {
        ExprKind::Lit(Lit::Int(i)) => Some(*i),
        ExprKind::Neg(x) => fold_int(x).and_then(i64::checked_neg),
        ExprKind::Arith(op, a, b) => {
            let (a, b) = (fold_int(a)?, fold_int(b)?);
            match op {
                ArithOp::Add => a.checked_add(b),
                ArithOp::Sub => a.checked_sub(b),
                ArithOp::Mul => a.checked_mul(b),
                ArithOp::Div => a.checked_div(b),
                ArithOp::Mod => a.checked_rem(b),
            }
        }
        _ => None,
    }
}

/// Elasticsearch's `StringUtils.findSimilar`: candidates within a Levenshtein distance of 2
/// (closest first), for "did you mean" hints.
pub fn find_similar(name: &str, candidates: &[&str]) -> Vec<String> {
    let mut scored: Vec<(usize, &str)> = candidates
        .iter()
        .filter_map(|c| {
            let d = levenshtein(&name.to_lowercase(), &c.to_lowercase());
            (d <= 2 && d < c.len().max(name.len())).then_some((d, *c))
        })
        .collect();
    scored.sort_by_key(|(d, _)| *d);
    match scored.first() {
        Some((best, _)) => {
            let best = *best;
            scored.into_iter().filter(|(d, _)| *d == best).map(|(_, c)| c.to_string()).collect()
        }
        None => Vec::new(),
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Parses an EQL statement.
pub fn parse(src: &str) -> Result<Statement, EsError> {
    let toks = lex(src)?;
    let mut p = Parser { src, toks, pos: 0 };
    p.statement()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(q: &str) -> String {
        parse(q).unwrap_err().reason().to_string()
    }

    fn cond(q: &str) -> Expr {
        match parse(q).unwrap().query {
            Query::Event(f) => f.condition,
            other => panic!("not an event query: {other:?}"),
        }
    }

    #[test]
    fn event_queries() {
        let s = parse("process where user == \"SYSTEM\"").unwrap();
        let Query::Event(f) = s.query else { panic!() };
        assert_eq!(f.category.as_deref(), Some("process"));
        assert!(matches!(f.condition.kind, ExprKind::Cmp(CmpOp::Eq, _, _)));
        let s = parse("any where true | tail 2 | head 1").unwrap();
        assert_eq!(s.pipes, vec![Pipe::Tail(2), Pipe::Head(1)]);
        let s = parse("\"my event\" where true").unwrap();
        let Query::Event(f) = s.query else { panic!() };
        assert_eq!(f.category.as_deref(), Some("my event"));
    }

    #[test]
    fn precedence() {
        // `not` binds tighter than `and`.
        let c = cond("any where not a == 1 and b == 2");
        let ExprKind::And(l, _) = c.kind else { panic!("{c:?}") };
        assert!(matches!(l.kind, ExprKind::Not(_)));
        // `and` binds tighter than `or`.
        let c = cond("any where a == 1 or b == 2 and c == 3");
        assert!(matches!(c.kind, ExprKind::Or(_, _)));
        // `*` binds tighter than `+`, unary minus tighter than `*`.
        let c = cond("any where a + b * -c > 1");
        let ExprKind::Cmp(_, l, _) = c.kind else { panic!() };
        let ExprKind::Arith(ArithOp::Add, _, r) = l.kind else { panic!() };
        let ExprKind::Arith(ArithOp::Mul, _, n) = r.kind else { panic!() };
        assert!(matches!(n.kind, ExprKind::Neg(_)));
        // A predicate attaches to the primary before it.
        let c = cond("any where a + b in (1, 2)");
        let ExprKind::Arith(ArithOp::Add, _, r) = c.kind else { panic!("{c:?}") };
        assert!(matches!(r.kind, ExprKind::In { .. }));
    }

    #[test]
    fn predicates_and_functions() {
        let c = cond("any where user : (\"a*\", \"b\")");
        let ExprKind::Match { kind: MatchKind::Seq, patterns, .. } = c.kind else { panic!() };
        assert_eq!(patterns.len(), 2);
        let c = cond("any where user not in~ (\"a\", \"b\")");
        assert!(matches!(c.kind, ExprKind::In { insensitive: true, negated: true, .. }));
        let c = cond("any where startsWith~(user, \"ad\")");
        assert!(matches!(c.kind, ExprKind::Call { insensitive: true, .. }));
        let c = cond("any where ?`odd name`.x == null");
        let ExprKind::Cmp(_, l, _) = c.kind else { panic!() };
        assert_eq!(l.kind, ExprKind::Field { name: "odd name.x".into(), optional: true });
        let c = cond("any where x like~ \"A*\"");
        assert!(matches!(c.kind, ExprKind::Match { kind: MatchKind::Like { insensitive: true }, .. }));
    }

    #[test]
    fn strings() {
        let c = cond(r#"any where a == "x\"\n\u{41}""#);
        let ExprKind::Cmp(_, _, r) = c.kind else { panic!() };
        assert_eq!(r.kind, ExprKind::Lit(Lit::Str("x\"\nA".into())));
        let c = cond(r#"any where a == """C:\path""""#);
        let ExprKind::Cmp(_, _, r) = c.kind else { panic!() };
        assert_eq!(r.kind, ExprKind::Lit(Lit::Str("C:\\path".into())));
        let c = cond(r#"any where a == "\u{1F600}""#);
        let ExprKind::Cmp(_, _, r) = c.kind else { panic!() };
        assert_eq!(r.kind, ExprKind::Lit(Lit::Str("\u{1F600}".into())));
        assert_eq!(err("any where a == 'x'"), "line 1:17: Use double quotes [\"] to define string literals, not single quotes [']");
        assert_eq!(err("any where a == ?\"x\""), "line 1:17: Use triple double quotes [\"\"\"] to define unescaped string literals, not [?\"]");
    }

    #[test]
    fn sequences() {
        let s = parse("sequence by pid with maxspan=2d [process where true] by a [network where true] by b until [file where true] by c").unwrap();
        let Query::Sequence { terms, until, maxspan_nanos } = s.query else { panic!() };
        assert_eq!(terms.len(), 2);
        assert_eq!(terms[0].keys.len(), 2);
        assert!(until.is_some());
        assert_eq!(maxspan_nanos, Some(2 * 86_400_000_000_000));
        let s = parse("sequence [process where true] with runs=3 [network where true]").unwrap();
        let Query::Sequence { terms, .. } = s.query else { panic!() };
        assert_eq!(terms.len(), 4);
    }

    #[test]
    fn elasticsearch_errors() {
        assert_eq!(err("PROCESS WHERE true"), "line 1:9: missing 'where' at 'WHERE'");
        assert_eq!(
            err("process where pid >"),
            "line 1:20: mismatched input '<EOF>' expecting {'false', 'null', 'true', '+', '-', '(', '?', STRING, INTEGER_VALUE, DECIMAL_VALUE, IDENTIFIER, QUOTED_IDENTIFIER, TILDE_IDENTIFIER}"
        );
        assert_eq!(
            err("sequence by pid [process where true] ![file where true] [network where true]"),
            "line 1:2: [maxspan] is required for sequences with missing events queries; found none"
        );
        assert_eq!(err("sequence [process where true]"), "line 1:2: A sequence requires a minimum of 2 queries, found [1]");
        assert_eq!(err("sequence with maxspan=2 [a where true] [b where true]"), "line 1:24: No time unit specified, did you mean [s] as in [2s]?");
        assert_eq!(
            err("sequence with maxspan=2y [a where true] [b where true]"),
            "line 1:25: Unrecognized time unit [y] in [2y], please specify one of [ms, s, m, h, d]"
        );
        assert_eq!(err("sequence with maxspan=0s [a where true] [b where true]"), "line 1:24: A positive maxspan value is required; found [0]");
        assert_eq!(err("sequence [a where true] with runs=0 [b where true]"), "line 1:36: A positive runs value is required; found [0]");
        assert_eq!(err("any where true | hed 1"), "line 1:19: Unrecognized pipe [hed], did you mean [head]?");
        assert_eq!(err("any where true | sort a"), "line 1:17: Pipe [sort] is not supported");
        assert_eq!(err("any where true | head -1"), "line 1:24: Pipe [head] expects a positive integer but found [-1]");
        assert_eq!(err("any where true | head"), "line 1:17: Pipe [head] expects exactly one argument but found [0]");
        assert_eq!(err("sequence [a where true] by x [b where true]"), "line 1:31: Inconsistent number of join keys specified; expected [1] but found [0]");
        assert_eq!(err("sequence [a where true] by x [b where true]  by   x,y"), "line 1:47: Inconsistent number of join keys specified; expected [1] but found [2]");
        assert_eq!(err("sample [a where true] by x [b where true] by x,y"), "line 1:44: Inconsistent number of join keys specified; expected [1] but found [2]");
        assert_eq!(err("sequence [a where true] [b where true] by x"), "line 1:41: Inconsistent number of join keys specified; expected [0] but found [1]");
        assert_eq!(err("sample [a where true] by x [b where true]"), "line 1:29: Inconsistent number of join keys specified; expected [1] but found [0]");
        assert_eq!(err("sequence by x [a where true] by x [b where true]"), "line 1:36: Inconsistent number of join keys specified; expected [1] but found [0]");
        assert_eq!(err("sample by x [a where true] | head 1"), "line 1:2: A sample requires a minimum of 2 queries, found [1]");
        assert!(err("join [a where true] [b where true]").contains("JOIN command is not supported"));
    }

    #[test]
    fn similar() {
        assert_eq!(find_similar("tial", &["head", "tail"]), vec!["tail".to_string()]);
        assert!(find_similar("zzzzzz", &["head", "tail"]).is_empty());
    }
}
