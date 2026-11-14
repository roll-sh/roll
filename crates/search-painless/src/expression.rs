//! `lang: expression`: Lucene expressions as Elasticsearch's `lang-expression` module runs them.
//!
//! An expression is a JavaScript-like formula over doubles (`doc['popularity'] * 2 +
//! ln(_score)`), compiled once and evaluated per document. The pipeline follows Lucene's
//! `JavascriptCompiler` and Elasticsearch's `ExpressionScriptEngine`:
//!
//! * **Lexing and parsing** reproduce the ANTLR grammar (`Javascript.g4`): longest-match tokens
//!   (a variable such as `doc['f'].date.year` is one token), left-recursive operator precedence,
//!   and the parser's error messages (`unexpected end of expression`, `invalid sequence of
//!   tokens near …`, `unexpected token … was expecting one of …`, `unexpected character …`).
//! * **Compilation** turns the tree into closures over a typed value, as Lucene generates
//!   bytecode with a type stack: every node is compiled for the type its parent wants
//!   (`double`, `long` or `int`), so `!0.5` is `1` (the operand is truncated to an `int` first),
//!   `0.5 ? 1 : 2` is `2`, bitwise operators work on `long`s and shifts take an `int` count.
//!   Function calls (`max`, `haversin`, …) are checked for arity here.
//! * **Linking** happens on an instance's first document, when the host can tell what a field
//!   is: each variable becomes `_score`, `_value`, a numeric parameter, or a doc-values reader
//!   (`doc['f']`, `.min()`, `.length`, `.date.dayOfWeek`, `.lat`, …), with Elasticsearch's
//!   `link error` messages.
//! * **Evaluation** reads every variable once per document into a slot array and runs the
//!   closure tree: no allocation and no dynamic typing per document.
//!
//! Lucene caches an expression's value per document: Elasticsearch's aggregation value scripts
//! (`_value` over a multi-valued field) therefore see the first value's result for every value
//! of a document. [`Instance::set_document`] opts into that behaviour.

use std::sync::Arc;

use serde_json::{json, Value as Json};

use crate::contexts::Context;
use crate::error::ScriptError;
use crate::host::{DocKind, DocValues, Host};
use crate::value::Value;

/// Contexts the expression engine compiles for (`GET _script_language`).
pub const CONTEXTS: &[Context] = &[
    Context::AggregationSelector,
    Context::Aggs,
    Context::BucketAggregation,
    Context::DoubleValues,
    Context::Field,
    Context::Filter,
    Context::NumberSort,
    Context::Score,
    Context::TermsSet,
];

/// Deepest nesting (of parentheses, or of the operator tree: `1 + 1 + … + 1` is a deep left
/// spine) a source may have. Elasticsearch's node dies of a stack overflow on such sources
/// (2,000 nested parentheses take it down); here they fail to compile, which bounds the
/// recursion of compilation, evaluation and dropping the tree.
const MAX_DEPTH: u32 = 1000;

// ------------------------------------------------------------------------------------ lexing

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok {
    Lp,
    Rp,
    Comma,
    BoolNot,
    BwNot,
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Lsh,
    Rsh,
    Ush,
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
    Ne,
    BwAnd,
    BwXor,
    BwOr,
    BoolAnd,
    BoolOr,
    Cond,
    Colon,
    Variable,
    Octal,
    Hex,
    Decimal,
    Eof,
}

impl Tok {
    /// The grammar's symbolic name (expected-token lists).
    fn name(self) -> &'static str {
        match self {
            Tok::Lp => "LP",
            Tok::Rp => "RP",
            Tok::Comma => "COMMA",
            Tok::Colon => "COLON",
            Tok::Eof => "<EOF>",
            _ => "?",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Token {
    kind: Tok,
    /// Char (UTF-16 unit) offsets into the source.
    start: usize,
    end: usize,
    line: usize,
    col: usize,
}

/// A compile-time failure: a `java.text.ParseException` (`nested`: the ANTLR exception it
/// wraps) at a source offset.
#[derive(Debug, Clone)]
struct ParseError {
    message: String,
    offset: usize,
    nested: Option<&'static str>,
}

struct Lexer<'a> {
    c: &'a [u16],
}

fn is_id_start(c: u16) -> bool {
    matches!(c, 0x5F | 0x24) || (c as u8 as u16 == c && (c as u8).is_ascii_alphabetic())
}

fn is_id_part(c: u16) -> bool {
    is_id_start(c) || (c < 128 && (c as u8).is_ascii_digit())
}

fn is_digit(c: u16) -> bool {
    (b'0' as u16..=b'9' as u16).contains(&c)
}

impl Lexer<'_> {
    fn at(&self, i: usize) -> Option<u16> {
        self.c.get(i).copied()
    }

    /// `STRING` (inside an array index) from `i`: the end offset, or the furthest offset read.
    fn string(&self, i: usize) -> Result<usize, usize> {
        let q = self.at(i).ok_or(i)?;
        let mut j = i + 1;
        loop {
            match self.at(j) {
                None => return Err(j),
                Some(c) if c == q => return Ok(j + 1),
                Some(0x5C) => match self.at(j + 1) {
                    Some(n) if n == q || n == 0x5C => j += 2,
                    _ => return Err(j + 1),
                },
                Some(_) => j += 1,
            }
        }
    }

    /// `INTEGER` from `i`.
    fn integer(&self, i: usize) -> Option<usize> {
        match self.at(i)? {
            0x30 => Some(i + 1),
            c if is_digit(c) => {
                let mut j = i + 1;
                while self.at(j).is_some_and(is_digit) {
                    j += 1;
                }
                Some(j)
            }
            _ => None,
        }
    }

    /// `ARRAY` (`[` STRING-or-INTEGER `]`) at `i`.
    fn array(&self, i: usize) -> Result<usize, usize> {
        let j = match self.at(i + 1) {
            Some(0x27 | 0x22) => self.string(i + 1)?,
            _ => self.integer(i + 1).ok_or(i + 1)?,
        };
        if self.at(j) == Some(0x5D) { Ok(j + 1) } else { Err(j) }
    }

    /// `VARIABLE` at `i` (an `ID` start): its end and the furthest offset read.
    fn variable(&self, i: usize) -> (usize, usize) {
        let mut j = i + 1;
        while self.at(j).is_some_and(is_id_part) {
            j += 1;
        }
        let mut accept = j;
        let mut furthest = j;
        loop {
            match self.at(j) {
                Some(0x5B) => match self.array(j) {
                    Ok(e) => {
                        j = e;
                        accept = j;
                    }
                    Err(f) => {
                        furthest = furthest.max(f);
                        break;
                    }
                },
                Some(0x2E) if self.at(j + 1).is_some_and(is_id_start) => {
                    j += 2;
                    while self.at(j).is_some_and(is_id_part) {
                        j += 1;
                    }
                    accept = j;
                }
                Some(0x2E) => {
                    furthest = furthest.max(j + 1);
                    break;
                }
                _ => break,
            }
        }
        (accept, furthest.max(accept))
    }

    /// The longest number token at `i`: kind, end, furthest offset read.
    fn number(&self, i: usize) -> (Option<(Tok, usize)>, usize) {
        let mut best: Option<(Tok, usize)> = None;
        let mut furthest = i;
        let take = |t: Tok, e: usize, best: &mut Option<(Tok, usize)>| {
            if best.is_none_or(|(_, b)| e > b) {
                *best = Some((t, e));
            }
        };
        if self.at(i) == Some(0x30) {
            // OCTAL: 0 [0-7]+
            let mut j = i + 1;
            while self.at(j).is_some_and(|c| (0x30..=0x37).contains(&c)) {
                j += 1;
            }
            if j > i + 1 {
                take(Tok::Octal, j, &mut best);
            }
            furthest = furthest.max(j);
            // HEX: 0 [xX] [0-9a-fA-F]+
            if matches!(self.at(i + 1), Some(0x78 | 0x58)) {
                let mut j = i + 2;
                while self.at(j).is_some_and(|c| c < 128 && (c as u8).is_ascii_hexdigit()) {
                    j += 1;
                }
                if j > i + 2 {
                    take(Tok::Hex, j, &mut best);
                }
                furthest = furthest.max(j);
            }
        }
        // DECIMAL: (INTEGER ('.' [0-9]*)? | '.' [0-9]+) ([eE] [+-]? [0-9]+)?
        let mantissa = if let Some(mut j) = self.integer(i) {
            if self.at(j) == Some(0x2E) {
                j += 1;
                while self.at(j).is_some_and(is_digit) {
                    j += 1;
                }
            }
            Some(j)
        } else if self.at(i) == Some(0x2E) {
            let mut j = i + 1;
            while self.at(j).is_some_and(is_digit) {
                j += 1;
            }
            if j > i + 1 { Some(j) } else { None }.or_else(|| {
                furthest = furthest.max(i + 1);
                None
            })
        } else {
            None
        };
        if let Some(mut j) = mantissa {
            furthest = furthest.max(j);
            if matches!(self.at(j), Some(0x65 | 0x45)) {
                let mut k = j + 1;
                if matches!(self.at(k), Some(0x2B | 0x2D)) {
                    k += 1;
                }
                let d = k;
                while self.at(k).is_some_and(is_digit) {
                    k += 1;
                }
                if k > d {
                    j = k;
                } else {
                    furthest = furthest.max(k);
                }
            }
            take(Tok::Decimal, j, &mut best);
        }
        (best, furthest)
    }

    fn tokens(&self) -> Result<Vec<Token>, ParseError> {
        let mut out = Vec::new();
        let (mut i, mut line, mut line_start) = (0usize, 1usize, 0usize);
        loop {
            while let Some(c @ (0x20 | 0x09 | 0x0A | 0x0D)) = self.at(i) {
                i += 1;
                if c == 0x0A {
                    line += 1;
                    line_start = i;
                }
            }
            let col = i - line_start;
            let Some(c) = self.at(i) else {
                out.push(Token { kind: Tok::Eof, start: i, end: i, line, col });
                return Ok(out);
            };
            let two = |a: u16, t: Tok, single: Option<Tok>| -> Option<(Tok, usize)> {
                if self.at(i + 1) == Some(a) { Some((t, 2)) } else { single.map(|s| (s, 1)) }
            };
            let fixed: Option<(Tok, usize)> = match c {
                0x28 => Some((Tok::Lp, 1)),
                0x29 => Some((Tok::Rp, 1)),
                0x2C => Some((Tok::Comma, 1)),
                0x7E => Some((Tok::BwNot, 1)),
                0x2A => Some((Tok::Mul, 1)),
                0x2F => Some((Tok::Div, 1)),
                0x25 => Some((Tok::Rem, 1)),
                0x2B => Some((Tok::Add, 1)),
                0x2D => Some((Tok::Sub, 1)),
                0x5E => Some((Tok::BwXor, 1)),
                0x3F => Some((Tok::Cond, 1)),
                0x3A => Some((Tok::Colon, 1)),
                0x21 => two(0x3D, Tok::Ne, Some(Tok::BoolNot)),
                0x26 => two(0x26, Tok::BoolAnd, Some(Tok::BwAnd)),
                0x7C => two(0x7C, Tok::BoolOr, Some(Tok::BwOr)),
                0x3D => two(0x3D, Tok::Eq, None),
                0x3C => two(0x3C, Tok::Lsh, None).or(two(0x3D, Tok::Lte, Some(Tok::Lt))),
                0x3E => {
                    if self.at(i + 1) == Some(0x3E) {
                        if self.at(i + 2) == Some(0x3E) { Some((Tok::Ush, 3)) } else { Some((Tok::Rsh, 2)) }
                    } else {
                        two(0x3D, Tok::Gte, Some(Tok::Gt))
                    }
                }
                _ => None,
            };
            let (kind, end) = if let Some((t, n)) = fixed {
                (t, i + n)
            } else if is_id_start(c) {
                (Tok::Variable, self.variable(i).0)
            } else if is_digit(c) || c == 0x2E {
                match self.number(i) {
                    (Some((t, e)), _) => (t, e),
                    (None, furthest) => return Err(self.char_error(i, furthest, line, col)),
                }
            } else {
                // `=` alone reads one character further, as the lexer tried `==`
                let furthest = if c == 0x3D { i + 1 } else { i };
                return Err(self.char_error(i, furthest, line, col));
            };
            out.push(Token { kind, start: i, end, line, col });
            i = end;
        }
    }

    /// `unexpected character '…'`: the text from the token start through the character the
    /// lexer failed on.
    fn char_error(&self, start: usize, furthest: usize, line: usize, col: usize) -> ParseError {
        let end = (furthest + 1).min(self.c.len()).max(start + 1);
        let text = String::from_utf16_lossy(&self.c[start..end]);
        ParseError { message: format!("unexpected character '{}' on line ({line}) position ({col})", display(&text)), offset: start, nested: Some("lexer_no_viable_alt_exception") }
    }
}

/// ANTLR's `getErrorDisplay`: control characters escaped.
fn display(s: &str) -> String {
    s.replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t")
}

// ------------------------------------------------------------------------------------ parsing

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bin {
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Lsh,
    Rsh,
    Ush,
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
    Ne,
    BwAnd,
    BwXor,
    BwOr,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Un {
    Not,
    BwNot,
    Plus,
    Neg,
}

#[derive(Debug)]
enum Node {
    Decimal(f64),
    /// Hexadecimal and octal literals (`long`s).
    Long(i64),
    /// A variable reference or a call: the normalized text, the call's arguments (`None`
    /// without parentheses) and the offset of its token.
    External { text: String, args: Option<Vec<Node>>, start: usize },
    Unary(Un, Box<Node>),
    Binary(Bin, Box<Node>, Box<Node>),
    Cond(Box<Node>, Box<Node>, Box<Node>),
}

fn binary_of(t: Tok) -> Option<(Bin, u8)> {
    Some(match t {
        Tok::Mul => (Bin::Mul, 10),
        Tok::Div => (Bin::Div, 10),
        Tok::Rem => (Bin::Rem, 10),
        Tok::Add => (Bin::Add, 9),
        Tok::Sub => (Bin::Sub, 9),
        Tok::Lsh => (Bin::Lsh, 8),
        Tok::Rsh => (Bin::Rsh, 8),
        Tok::Ush => (Bin::Ush, 8),
        Tok::Lt => (Bin::Lt, 7),
        Tok::Lte => (Bin::Lte, 7),
        Tok::Gt => (Bin::Gt, 7),
        Tok::Gte => (Bin::Gte, 7),
        Tok::Eq => (Bin::Eq, 6),
        Tok::Ne => (Bin::Ne, 6),
        Tok::BwAnd => (Bin::BwAnd, 5),
        Tok::BwXor => (Bin::BwXor, 4),
        Tok::BwOr => (Bin::BwOr, 3),
        Tok::BoolAnd => (Bin::And, 2),
        Tok::BoolOr => (Bin::Or, 1),
        _ => return None,
    })
}

/// Precedence of the ternary (right associative) and of prefix operators.
const COND_PREC: u8 = 0;
const UNARY_PREC: u8 = 11;

struct Parser<'a> {
    src: &'a [u16],
    toks: Vec<Token>,
    pos: usize,
    depth: u32,
}

impl Parser<'_> {
    fn peek(&self) -> Token {
        self.toks[self.pos]
    }

    fn text(&self, t: Token) -> String {
        if t.kind == Tok::Eof {
            return "<EOF>".into();
        }
        String::from_utf16_lossy(&self.src[t.start..t.end])
    }

    /// `recoverInline`: the token is not the one the rule needs.
    fn mismatch(&self, want: Tok) -> ParseError {
        let t = self.peek();
        ParseError {
            message: format!("unexpected token '{}' on line ({}) position ({}) was expecting one of {}", display(&self.text(t)), t.line, t.col, want.name()),
            offset: t.start,
            nested: None,
        }
    }

    fn expect(&mut self, want: Tok) -> Result<Token, ParseError> {
        let t = self.peek();
        if t.kind != want {
            return Err(self.mismatch(want));
        }
        self.pos += 1;
        Ok(t)
    }

    /// Fails a tree (or a recursion) deeper than [`MAX_DEPTH`].
    fn deep(&self, d: u32) -> Result<(), ParseError> {
        if d > MAX_DEPTH {
            return Err(ParseError { message: String::new(), offset: self.peek().start, nested: Some("stack_overflow_error") });
        }
        Ok(())
    }

    /// An expression whose operators bind at least as tightly as `min`, with its tree depth.
    fn expr(&mut self, min: u8) -> Result<(Node, u32), ParseError> {
        self.depth += 1;
        self.deep(self.depth)?;
        let (mut left, mut ld) = self.primary()?;
        loop {
            let t = self.peek();
            if t.kind == Tok::Cond {
                if COND_PREC < min {
                    break;
                }
                self.pos += 1;
                let (mid, md) = self.expr(0)?;
                self.expect(Tok::Colon)?;
                let (right, rd) = self.expr(COND_PREC)?;
                ld = 1 + ld.max(md).max(rd);
                self.deep(ld)?;
                left = Node::Cond(Box::new(left), Box::new(mid), Box::new(right));
                continue;
            }
            let Some((op, prec)) = binary_of(t.kind) else { break };
            if prec < min {
                break;
            }
            self.pos += 1;
            let (right, rd) = self.expr(prec + 1)?;
            ld = 1 + ld.max(rd);
            self.deep(ld)?;
            left = Node::Binary(op, Box::new(left), Box::new(right));
        }
        self.depth -= 1;
        Ok((left, ld))
    }

    fn primary(&mut self) -> Result<(Node, u32), ParseError> {
        let t = self.peek();
        let text = self.text(t);
        match t.kind {
            Tok::Lp => {
                self.pos += 1;
                let e = self.expr(0)?;
                self.expect(Tok::Rp)?;
                Ok(e)
            }
            Tok::Decimal => {
                self.pos += 1;
                Ok((Node::Decimal(parse_decimal(&text)), 1))
            }
            Tok::Hex | Tok::Octal => {
                self.pos += 1;
                let (digits, radix) = if t.kind == Tok::Hex { (&text[2..], 16) } else { (&text[1..], 8) };
                match i64::from_str_radix(digits, radix) {
                    Ok(v) => Ok((Node::Long(v), 1)),
                    Err(_) => Err(ParseError { message: format!("For input string: \"{digits}\"{}", if radix == 16 { " under radix 16" } else { " under radix 8" }), offset: t.start, nested: Some("number_format_exception") }),
                }
            }
            Tok::Variable => {
                self.pos += 1;
                if self.peek().kind != Tok::Lp {
                    return Ok((Node::External { text: normalize_quotes(&text), args: None, start: t.start }, 1));
                }
                self.pos += 1;
                let mut args = Vec::new();
                let mut d = 0;
                if starts_expression(self.peek().kind) {
                    let (a, ad) = self.expr(0)?;
                    args.push(a);
                    d = d.max(ad);
                    while self.peek().kind == Tok::Comma {
                        self.pos += 1;
                        let (a, ad) = self.expr(0)?;
                        args.push(a);
                        d = d.max(ad);
                    }
                }
                self.expect(Tok::Rp)?;
                let d = d + 1;
                self.deep(d)?;
                Ok((Node::External { text, args: Some(args), start: t.start }, d))
            }
            Tok::BoolNot | Tok::BwNot | Tok::Add | Tok::Sub => {
                self.pos += 1;
                let op = match t.kind {
                    Tok::BoolNot => Un::Not,
                    Tok::BwNot => Un::BwNot,
                    Tok::Add => Un::Plus,
                    _ => Un::Neg,
                };
                let (x, d) = self.expr(UNARY_PREC)?;
                self.deep(d + 1)?;
                Ok((Node::Unary(op, Box::new(x)), d + 1))
            }
            Tok::Eof => Err(ParseError { message: "unexpected end of expression".into(), offset: t.start, nested: Some("no_viable_alt_exception") }),
            _ => Err(ParseError {
                message: format!("invalid sequence of tokens near '{}' on line ({}) position ({})", display(&text), t.line, t.col),
                offset: t.start,
                nested: Some("no_viable_alt_exception"),
            }),
        }
    }
}

fn starts_expression(t: Tok) -> bool {
    matches!(t, Tok::Lp | Tok::Decimal | Tok::Hex | Tok::Octal | Tok::Variable | Tok::BoolNot | Tok::BwNot | Tok::Add | Tok::Sub)
}

/// `Double.parseDouble` of a `DECIMAL` token (`2.`, `.5`, `1e3`).
fn parse_decimal(s: &str) -> f64 {
    let fixed: String = if s.ends_with('.') || s.contains(".e") || s.contains(".E") { s.replacen('.', ".0", 1) } else { s.to_string() };
    let fixed = if fixed.starts_with('.') { format!("0{fixed}") } else { fixed };
    fixed.parse().unwrap_or(f64::NAN)
}

/// `JavascriptCompiler.normalizeQuotes`: double-quoted strings of a variable become
/// single-quoted ones (escaping single quotes inside), so `doc["f"]` and `doc['f']` are the
/// same variable.
fn normalize_quotes(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_double = false;
    let mut i = 0;
    while i < c.len() {
        let mut ch = c[i];
        if ch == '\\' {
            i += 1;
            ch = c.get(i).copied().unwrap_or('\\');
            if ch == '\\' {
                out.push('\\');
            }
        } else if ch == '\'' {
            if in_double {
                out.push('\\');
            } else {
                // copy the single-quoted string up to its end quote
                let mut j = i + 1;
                while j < c.len() && c[j] != '\'' {
                    if c[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
                out.extend(&c[i..j.min(c.len())]);
                i = j;
                ch = '\'';
            }
        } else if ch == '"' {
            ch = '\'';
            in_double = !in_double;
        }
        out.push(ch);
        i += 1;
    }
    out
}

// ------------------------------------------------------------------------------- compilation

/// A compiled node for the type its parent requested.
enum Typed {
    D(Fd),
    L(Fl),
    I(Fi),
}

type Fd = Box<dyn Fn(&[f64]) -> f64 + Send + Sync>;
type Fl = Box<dyn Fn(&[f64]) -> i64 + Send + Sync>;
type Fi = Box<dyn Fn(&[f64]) -> i32 + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    D,
    L,
    I,
}

/// `GeneratorAdapter.cast` between the primitive types (Java's narrowing rules: `d2i`/`d2l`
/// saturate and map NaN to 0, `l2i` truncates).
fn cast(t: Typed, to: Want) -> Typed {
    match (t, to) {
        (t @ Typed::D(_), Want::D) | (t @ Typed::L(_), Want::L) | (t @ Typed::I(_), Want::I) => t,
        (Typed::D(f), Want::L) => Typed::L(Box::new(move |v| f(v) as i64)),
        (Typed::D(f), Want::I) => Typed::I(Box::new(move |v| f(v) as i32)),
        (Typed::L(f), Want::D) => Typed::D(Box::new(move |v| f(v) as f64)),
        (Typed::L(f), Want::I) => Typed::I(Box::new(move |v| f(v) as i32)),
        (Typed::I(f), Want::D) => Typed::D(Box::new(move |v| f(v) as f64)),
        (Typed::I(f), Want::L) => Typed::L(Box::new(move |v| f(v) as i64)),
    }
}

fn boolean(want: Want, f: impl Fn(&[f64]) -> bool + Send + Sync + 'static) -> Typed {
    match want {
        Want::D => Typed::D(Box::new(move |v| if f(v) { 1.0 } else { 0.0 })),
        Want::L => Typed::L(Box::new(move |v| f(v) as i64)),
        Want::I => Typed::I(Box::new(move |v| f(v) as i32)),
    }
}

fn as_d(t: Typed) -> Fd {
    match cast(t, Want::D) {
        Typed::D(f) => f,
        _ => unreachable!(),
    }
}
fn as_l(t: Typed) -> Fl {
    match cast(t, Want::L) {
        Typed::L(f) => f,
        _ => unreachable!(),
    }
}
fn as_i(t: Typed) -> Fi {
    match cast(t, Want::I) {
        Typed::I(f) => f,
        _ => unreachable!(),
    }
}

/// A function of Lucene's default set: arity and implementation.
#[derive(Clone, Copy)]
enum Func {
    Math(crate::lib_lang::MathFn),
    N(usize, fn(&[f64]) -> f64),
}

impl Func {
    fn arity(self) -> usize {
        match self {
            Func::Math(crate::lib_lang::MathFn::F1(_)) => 1,
            Func::Math(crate::lib_lang::MathFn::F2(_)) => 2,
            Func::N(n, _) => n,
        }
    }
}

/// Lucene's default functions (`JavascriptCompiler.properties`): `java.lang.Math` (HotSpot's
/// intrinsics where `Math` has them, exactly as Painless's `Math`), `MathUtil`'s inverse
/// hyperbolic functions and logarithm, and the haversine distances.
fn function(name: &str) -> Option<Func> {
    let math = |n: &str, arity: usize| crate::lib_lang::math_fn(n, arity, false).map(Func::Math);
    match name {
        "abs" | "acos" | "asin" | "atan" | "ceil" | "cos" | "cosh" | "exp" | "floor" | "log10" | "sin" | "sinh" | "sqrt" | "tan" | "tanh" => math(name, 1),
        "atan2" | "max" | "min" | "pow" => math(name, 2),
        "ln" => math("log", 1),
        // `MathUtil` over `Math.log` and `Math.sqrt`
        "acosh" => Some(Func::N(1, |a| ((a[0] * a[0] - 1.0).sqrt() + a[0]).ln())),
        "asinh" => Some(Func::N(1, |a| {
            let (x, sign) = if a[0].is_sign_negative() { (a[0].abs(), -1.0) } else { (a[0], 1.0) };
            sign * ((x * x + 1.0).sqrt() + x).ln()
        })),
        "atanh" => Some(Func::N(1, |a| {
            let (x, mult) = if a[0].is_sign_negative() { (a[0].abs(), -0.5) } else { (a[0], 0.5) };
            mult * ((1.0 + x) / (1.0 - x)).ln()
        })),
        "logn" => Some(Func::N(2, |a| a[1].ln() / a[0].ln())),
        "haversin" => Some(Func::N(4, |a| crate::es::score::haversin_meters(a[0], a[1], a[2], a[3]) * (1.0 / 1000.0))),
        "haversinMeters" => Some(Func::N(4, |a| crate::es::score::haversin_meters(a[0], a[1], a[2], a[3]))),
        _ => None,
    }
}

struct Codegen<'s> {
    source: &'s str,
    /// Normalized variable names in order of first use.
    variables: Vec<String>,
}

impl Codegen<'_> {
    fn var(&mut self, name: String) -> usize {
        match self.variables.iter().position(|v| *v == name) {
            Some(i) => i,
            None => {
                self.variables.push(name);
                self.variables.len() - 1
            }
        }
    }

    fn node(&mut self, n: &Node, want: Want) -> Result<Typed, ParseError> {
        Ok(match n {
            Node::Decimal(x) => {
                let x = *x;
                cast(Typed::D(Box::new(move |_| x)), want)
            }
            Node::Long(x) => {
                let x = *x;
                match want {
                    Want::I => {
                        let y = x as i32;
                        Typed::I(Box::new(move |_| y))
                    }
                    Want::L => Typed::L(Box::new(move |_| x)),
                    Want::D => {
                        let y = x as f64;
                        Typed::D(Box::new(move |_| y))
                    }
                }
            }
            Node::External { text, args, start } => {
                let func = if args.is_some() { function(text) } else { None };
                if let (Some(func), Some(args)) = (func, args) {
                    let arity = func.arity();
                    if args.len() != arity {
                        return Err(ParseError {
                            message: format!("Invalid expression '{}': Expected ({arity}) arguments for function call ({text}), but found ({}).", self.source, args.len()),
                            offset: *start,
                            nested: None,
                        });
                    }
                    let mut fs: Vec<Fd> = args.iter().map(|a| self.node(a, Want::D).map(as_d)).collect::<Result<_, _>>()?;
                    use crate::lib_lang::MathFn;
                    let call: Fd = match func {
                        Func::Math(MathFn::F1(f)) => {
                            let a = fs.remove(0);
                            Box::new(move |v| f(a(v)))
                        }
                        Func::Math(MathFn::F2(f)) => {
                            let (a, b) = (fs.remove(0), fs.remove(0));
                            Box::new(move |v| f(a(v), b(v)))
                        }
                        Func::N(_, f) => Box::new(move |v| {
                            let mut buf = [0f64; 4];
                            for (i, a) in fs.iter().enumerate() {
                                buf[i] = a(v);
                            }
                            f(&buf[..fs.len()])
                        }),
                    };
                    return Ok(cast(Typed::D(call), want));
                }
                let name = match args {
                    None => text.clone(),
                    Some(a) if a.is_empty() && text.contains('.') => normalize_quotes(&format!("{text}()")),
                    Some(_) => {
                        return Err(ParseError { message: format!("Invalid expression '{}': Unrecognized function call ({text}).", self.source), offset: *start, nested: None });
                    }
                };
                let i = self.var(name);
                cast(Typed::D(Box::new(move |v| v[i])), want)
            }
            Node::Unary(op, x) => match op {
                Un::Not => {
                    let f = as_i(self.node(x, Want::I)?);
                    boolean(want, move |v| f(v) == 0)
                }
                Un::BwNot => {
                    let f = as_l(self.node(x, Want::L)?);
                    cast(Typed::L(Box::new(move |v| f(v) ^ -1)), want)
                }
                Un::Plus => self.node(x, want)?,
                Un::Neg => {
                    let f = as_d(self.node(x, Want::D)?);
                    cast(Typed::D(Box::new(move |v| -f(v))), want)
                }
            },
            Node::Binary(op, l, r) => match op {
                Bin::Mul | Bin::Div | Bin::Rem | Bin::Add | Bin::Sub => {
                    let (a, b) = (as_d(self.node(l, Want::D)?), as_d(self.node(r, Want::D)?));
                    let f: Fd = match op {
                        Bin::Mul => Box::new(move |v| a(v) * b(v)),
                        Bin::Div => Box::new(move |v| a(v) / b(v)),
                        Bin::Rem => Box::new(move |v| a(v) % b(v)),
                        Bin::Add => Box::new(move |v| a(v) + b(v)),
                        _ => Box::new(move |v| a(v) - b(v)),
                    };
                    cast(Typed::D(f), want)
                }
                Bin::Lsh | Bin::Rsh | Bin::Ush => {
                    let (a, b) = (as_l(self.node(l, Want::L)?), as_i(self.node(r, Want::I)?));
                    let f: Fl = match op {
                        Bin::Lsh => Box::new(move |v| a(v).wrapping_shl(b(v) as u32)),
                        Bin::Rsh => Box::new(move |v| a(v).wrapping_shr(b(v) as u32)),
                        _ => Box::new(move |v| (a(v) as u64).wrapping_shr(b(v) as u32) as i64),
                    };
                    cast(Typed::L(f), want)
                }
                Bin::BwAnd | Bin::BwXor | Bin::BwOr => {
                    let (a, b) = (as_l(self.node(l, Want::L)?), as_l(self.node(r, Want::L)?));
                    let f: Fl = match op {
                        Bin::BwAnd => Box::new(move |v| a(v) & b(v)),
                        Bin::BwXor => Box::new(move |v| a(v) ^ b(v)),
                        _ => Box::new(move |v| a(v) | b(v)),
                    };
                    cast(Typed::L(f), want)
                }
                Bin::Lt | Bin::Lte | Bin::Gt | Bin::Gte | Bin::Eq | Bin::Ne => {
                    let (a, b) = (as_d(self.node(l, Want::D)?), as_d(self.node(r, Want::D)?));
                    match op {
                        Bin::Lt => boolean(want, move |v| a(v) < b(v)),
                        Bin::Lte => boolean(want, move |v| a(v) <= b(v)),
                        Bin::Gt => boolean(want, move |v| a(v) > b(v)),
                        Bin::Gte => boolean(want, move |v| a(v) >= b(v)),
                        Bin::Eq => boolean(want, move |v| a(v) == b(v)),
                        _ => boolean(want, move |v| a(v) != b(v)),
                    }
                }
                Bin::And => {
                    let (a, b) = (as_i(self.node(l, Want::I)?), as_i(self.node(r, Want::I)?));
                    boolean(want, move |v| a(v) != 0 && b(v) != 0)
                }
                Bin::Or => {
                    let (a, b) = (as_i(self.node(l, Want::I)?), as_i(self.node(r, Want::I)?));
                    boolean(want, move |v| a(v) != 0 || b(v) != 0)
                }
            },
            Node::Cond(c, t, f) => {
                let c = as_i(self.node(c, Want::I)?);
                match (self.node(t, want)?, self.node(f, want)?) {
                    (Typed::D(a), Typed::D(b)) => Typed::D(Box::new(move |v| if c(v) != 0 { a(v) } else { b(v) })),
                    (Typed::L(a), Typed::L(b)) => Typed::L(Box::new(move |v| if c(v) != 0 { a(v) } else { b(v) })),
                    (Typed::I(a), Typed::I(b)) => Typed::I(Box::new(move |v| if c(v) != 0 { a(v) } else { b(v) })),
                    _ => unreachable!("both branches are compiled for the same type"),
                }
            }
        })
    }
}

// ------------------------------------------------------------------------------- expression

/// A compiled expression (`Send + Sync`; share one per source, like a Painless `Script`).
pub struct Expression {
    source: Arc<str>,
    context: Context,
    eval: Fd,
    variables: Vec<String>,
}

impl std::fmt::Debug for Expression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Expression").field("source", &self.source).field("context", &self.context).field("variables", &self.variables).finish()
    }
}

/// A `script_exception` of the expression engine (`compile error`, `link error`): the portion
/// of the source it is about and a pointer, no position; `cause` is the exception it wraps.
fn script_error(reason: &str, source: &str, portion: &str, pointer: usize, cause_type: &str, cause_reason: Option<String>, nested: Option<&str>) -> ScriptError {
    let mut e = ScriptError::plain(cause_type, String::new());
    e.plain = false;
    e.reason = reason.into();
    e.script_stack = vec![portion.to_string(), format!("{}^---- HERE", " ".repeat(pointer))];
    e.script = source.to_string();
    e.lang = "expression";
    e.position = None;
    e.cause_type = cause_type.to_string();
    e.cause_reason = cause_reason;
    e.cause_is_root = false;
    e.cause_fields = nested.map(|n| vec![("caused_by".to_string(), json!({ "type": n, "reason": null }))]).unwrap_or_default();
    e
}

impl Expression {
    /// Compiles `source` for `context` (`JavascriptCompiler.compile`, then the engine's context
    /// check).
    pub fn compile(source: &str, context: Context) -> Result<Expression, ScriptError> {
        // the parser and the compiler recurse per nesting level: a thread with room for
        // `MAX_DEPTH` levels (the tree they build is shallow enough for any caller)
        std::thread::scope(|scope| {
            let worker = std::thread::Builder::new().name("expression-compile".into()).stack_size(crate::guard::COMPILE_THREAD_STACK).spawn_scoped(scope, || Self::compile_here(source, context));
            match worker {
                Ok(h) => h.join().unwrap_or_else(|panic| Err(ScriptError::internal(source, &*panic))),
                Err(_) => Self::compile_here(source, context),
            }
        })
    }

    fn compile_here(source: &str, context: Context) -> Result<Expression, ScriptError> {
        let units: Vec<u16> = source.encode_utf16().collect();
        let fail = |e: ParseError| {
            if e.nested == Some("stack_overflow_error") {
                // Elasticsearch's node overflows its stack on such sources; this one refuses them
                let mut g = script_error("compile error", source, source, 0, "stack_overflow_error", None, None);
                g.script_stack.clear();
                g.cause_is_root = true;
                return g;
            }
            if e.nested == Some("number_format_exception") {
                // not a ParseException: the script service wraps it as it is
                let mut g = ScriptError::plain("general_script_exception", format!("Failed to compile inline script [{source}] using lang [expression]"));
                g.cause_fields = vec![("caused_by".into(), json!({ "type": "number_format_exception", "reason": e.message }))];
                return g;
            }
            script_error("compile error", source, source, e.offset, "parse_exception", Some(e.message), e.nested)
        };
        let toks = Lexer { c: &units }.tokens().map_err(fail)?;
        let mut p = Parser { src: &units, toks, pos: 0, depth: 0 };
        let (tree, _) = p.expr(0).map_err(fail)?;
        p.expect(Tok::Eof).map_err(fail)?;
        let mut cg = Codegen { source, variables: Vec::new() };
        let eval = as_d(cg.node(&tree, Want::D).map_err(fail)?);
        if !CONTEXTS.contains(&context) {
            return Err(ScriptError::plain("illegal_argument_exception", format!("expression engine does not know how to handle script context [{}]", context.name())));
        }
        Ok(Expression { source: Arc::from(source), context, eval, variables: cg.variables })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn context(&self) -> Context {
        self.context
    }

    /// The variables the expression reads (normalized: `doc["f"]` is `doc['f']`), in order of
    /// first use: the `double_values` context supplies one value per variable.
    pub fn variables(&self) -> &[String] {
        &self.variables
    }

    /// Whether the expression reads `_score` in a context that provides it.
    pub fn needs_score(&self) -> bool {
        matches!(self.context, Context::Score | Context::Aggs | Context::NumberSort) && self.variables.iter().any(|v| v == "_score")
    }

    /// Evaluates over the given variable values (one per [`Expression::variables`]; the
    /// `double_values` context's `evaluate`).
    pub fn evaluate(&self, values: &[f64]) -> f64 {
        if values.len() >= self.variables.len() {
            (self.eval)(values)
        } else {
            let mut v = values.to_vec();
            v.resize(self.variables.len(), 0.0);
            (self.eval)(&v)
        }
    }

    /// An evaluator for one thread (one segment): reuse it across documents.
    pub fn instance(&self) -> Instance<'_> {
        Instance { expr: self, params: None, value: 0.0, bound: None, vals: vec![0.0; self.variables.len()], bufs: Vec::new(), doc: None, cached: None }
    }
}

// ----------------------------------------------------------------------------------- linking

/// How a variable gets its value for a document.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Source {
    Const(f64),
    Score,
    /// `_value` of an aggregation value script.
    AggValue,
    Doc { field: usize, read: Read },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Read {
    Min,
    Max,
    Avg,
    Median,
    Sum,
    Count,
    Empty,
    /// `Calendar.get(field)` of the minimum date (UTC Gregorian calendar with its Julian
    /// cutover, as `DateMethodValueSource`).
    Calendar(usize),
    /// A `ZonedDateTime` field of the minimum date (`DateObjectValueSource`).
    DateObject(DatePart),
    GeoLat,
    GeoLon,
    GeoEmpty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DatePart {
    CenturyOfEra,
    DayOfMonth,
    DayOfWeek,
    DayOfYear,
    Era,
    HourOfDay,
    MillisOfDay,
    MillisOfSecond,
    MinuteOfDay,
    MinuteOfHour,
    MonthOfYear,
    SecondOfDay,
    SecondOfMinute,
    WeekOfWeekyear,
    Weekyear,
    Year,
    YearOfCentury,
    YearOfEra,
}

impl DatePart {
    fn of(name: &str, method: bool) -> Option<DatePart> {
        let name = if method {
            let rest = name.strip_prefix("get")?;
            let mut c = rest.chars();
            let first = c.next()?.to_ascii_lowercase();
            format!("{first}{}", c.as_str())
        } else {
            name.to_string()
        };
        Some(match name.as_str() {
            "centuryOfEra" => DatePart::CenturyOfEra,
            "dayOfMonth" => DatePart::DayOfMonth,
            "dayOfWeek" => DatePart::DayOfWeek,
            "dayOfYear" => DatePart::DayOfYear,
            "era" => DatePart::Era,
            "hourOfDay" => DatePart::HourOfDay,
            "millisOfDay" => DatePart::MillisOfDay,
            "millisOfSecond" => DatePart::MillisOfSecond,
            "minuteOfDay" => DatePart::MinuteOfDay,
            "minuteOfHour" => DatePart::MinuteOfHour,
            "monthOfYear" => DatePart::MonthOfYear,
            "secondOfDay" => DatePart::SecondOfDay,
            "secondOfMinute" => DatePart::SecondOfMinute,
            "weekOfWeekyear" => DatePart::WeekOfWeekyear,
            "weekyear" => DatePart::Weekyear,
            "year" => DatePart::Year,
            "yearOfCentury" => DatePart::YearOfCentury,
            "yearOfEra" => DatePart::YearOfEra,
            _ => return None,
        })
    }

    /// The field of the UTC date-time at `ms` epoch milliseconds (proleptic ISO calendar).
    fn at(self, ms: i64) -> f64 {
        let day = ms.div_euclid(86_400_000);
        let mod_ms = ms.rem_euclid(86_400_000);
        let d = crate::time::Date::from_epoch_day(day);
        let y = d.y as i64;
        let yoe = if y >= 1 { y } else { 1 - y };
        (match self {
            DatePart::CenturyOfEra => yoe / 100,
            DatePart::DayOfMonth => d.d as i64,
            DatePart::DayOfWeek => d.day_of_week() as i64,
            DatePart::DayOfYear => d.day_of_year() as i64,
            DatePart::Era => i64::from(y >= 1),
            DatePart::HourOfDay => mod_ms / 3_600_000,
            DatePart::MillisOfDay => mod_ms,
            DatePart::MillisOfSecond => mod_ms % 1000,
            DatePart::MinuteOfDay => mod_ms / 60_000,
            DatePart::MinuteOfHour => mod_ms / 60_000 % 60,
            DatePart::MonthOfYear => d.m as i64,
            DatePart::SecondOfDay => mod_ms / 1000,
            DatePart::SecondOfMinute => mod_ms / 1000 % 60,
            DatePart::WeekOfWeekyear => crate::time::weeks::week_of_week_based_year(d, root_weeks()),
            DatePart::Weekyear => crate::time::weeks::week_based_year(d, root_weeks()),
            DatePart::Year => y,
            DatePart::YearOfCentury => yoe % 100,
            DatePart::YearOfEra => yoe,
        }) as f64
    }
}

/// `DateFormatters.WEEK_FIELDS_ROOT` (`WeekFields.of(Locale.ROOT)`: CLDR's world default,
/// weeks start on Monday and the first week has at least one day).
fn root_weeks() -> crate::time::weeks::WeekDef {
    crate::time::weeks::WeekDef { first: 1, min_days: 1 }
}

/// The doc-values kind's family, as the engine dispatches on the field type.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Numeric,
    Date,
    Geo,
    Other,
}

fn family(k: DocKind) -> Family {
    use DocKind::*;
    match k {
        Long | Integer | Short | Byte | UnsignedLong | Double | Float | HalfFloat | ScaledFloat | Boolean => Family::Numeric,
        Date | DateNanos => Family::Date,
        GeoPoint => Family::Geo,
        _ => Family::Other,
    }
}

#[derive(Debug, PartialEq)]
enum Part {
    Member(String),
    StrIndex(String),
    IntIndex(i32),
    Method(String),
}

/// `VariableContext.parse` of a normalized variable.
fn parse_variable(text: &str) -> Result<Vec<Part>, String> {
    let c: Vec<char> = text.chars().collect();
    let mut parts = Vec::new();
    let member = |i: usize, parts: &mut Vec<Part>| -> usize {
        let mut j = i + 1;
        while j < c.len() && !matches!(c[j], '[' | '.' | '(') {
            j += 1;
        }
        let name: String = c[i..j.min(c.len())].iter().collect();
        if j + 1 < c.len() && c[j] == '(' && c[j + 1] == ')' {
            parts.push(Part::Method(name));
            j + 2
        } else {
            parts.push(Part::Member(name));
            j
        }
    };
    let mut i = member(0, &mut parts);
    while i < c.len() {
        if c[i] == '[' {
            i += 1;
            if c.get(i) == Some(&'\'') {
                i += 1;
                let mut s = String::new();
                while i < c.len() && c[i] != '\'' {
                    if c[i] == '\\' {
                        i += 1;
                    }
                    if let Some(&ch) = c.get(i) {
                        s.push(ch);
                    }
                    i += 1;
                }
                parts.push(Part::StrIndex(s));
                i += 1;
            } else {
                let mut j = i;
                while j < c.len() && c[j] != ']' {
                    j += 1;
                }
                let digits: String = c[i..j].iter().collect();
                let n = digits.parse::<i32>().map_err(|_| format!("For input string: \"{digits}\""))?;
                parts.push(Part::IntIndex(n));
                i = j;
            }
            i += 1;
        } else {
            i = member(i + 1, &mut parts);
        }
    }
    Ok(parts)
}

/// A link failure: the exception (type, reason) and the pointer offset (`ParseException`s
/// carry one, other exceptions point at 0).
struct LinkError {
    kind: &'static str,
    reason: String,
    offset: usize,
}

impl LinkError {
    fn parse(reason: String, offset: usize) -> LinkError {
        LinkError { kind: "parse_exception", reason, offset }
    }
    fn iae(reason: String) -> LinkError {
        LinkError { kind: "illegal_argument_exception", reason, offset: 0 }
    }
}

/// The doc-values reader of a `doc[...]` variable (`ExpressionScriptEngine.getDocValueSource`).
fn doc_source(variable: &str, host: &mut dyn Host, fields: &mut Vec<(String, Family)>, buf: &mut DocValues) -> Result<Source, LinkError> {
    let parts = parse_variable(variable).map_err(|r| LinkError { kind: "number_format_exception", reason: r, offset: 0 })?;
    match parts.first() {
        Some(Part::Member(d)) if d == "doc" => {}
        Some(Part::Member(d) | Part::Method(d)) => return Err(LinkError::parse(format!("Unknown variable [{d}]"), 0)),
        _ => return Err(LinkError::parse(format!("Unknown variable [{variable}]"), 0)),
    }
    let Some(Part::StrIndex(field)) = parts.get(1) else {
        return Err(LinkError::parse("Variable 'doc' must be used with a specific field like: doc['myfield']".into(), 3));
    };
    let mut variable_name = "value".to_string();
    let mut method: Option<String> = None;
    if parts.len() == 3 {
        match &parts[2] {
            Part::Method(m) => method = Some(m.clone()),
            Part::Member(m) => variable_name = m.clone(),
            _ => return Err(LinkError::iae("Only member variables or member methods may be accessed on a field when not accessing the field directly".into())),
        }
    }
    let mut date_accessor = false;
    if parts.len() > 3 {
        if parts.len() == 4
            && matches!(&parts[2], Part::Member(t) | Part::Method(t) if t == "date" || t == "getDate")
        {
            match &parts[3] {
                Part::Method(m) => {
                    method = Some(m.clone());
                    date_accessor = true;
                }
                Part::Member(m) => {
                    variable_name = m.clone();
                    date_accessor = true;
                }
                _ => {}
            }
        }
        if !date_accessor {
            return Err(LinkError::iae(format!("Variable [{variable}] does not follow an allowed format of either doc['field'] or doc['field'].method()")));
        }
    }
    // the field's type, from what the host reads for the current document
    let fam = match fields.iter().find(|(f, _)| f == field) {
        Some((_, fam)) => *fam,
        None => match host.doc_values(field, buf) {
            Ok(false) => return Err(LinkError::parse(format!("Field [{field}] does not exist in mappings"), 5)),
            Err(e) => return Err(LinkError { kind: e.kind, reason: e.reason, offset: 0 }),
            Ok(true) => family(buf.kind),
        },
    };
    let read = match fam {
        Family::Geo => match method.as_deref() {
            None => match variable_name.as_str() {
                "empty" => Read::GeoEmpty,
                "lat" => Read::GeoLat,
                "lon" => Read::GeoLon,
                v => return Err(LinkError::iae(format!("Member variable [{v}] does not exist for geo field [{field}]."))),
            },
            Some(m) => match m {
                "isEmpty" => Read::GeoEmpty,
                "getLat" => Read::GeoLat,
                "getLon" => Read::GeoLon,
                _ => return Err(LinkError::iae(format!("Member method [{m}] does not exist for geo field [{field}]."))),
            },
        },
        Family::Date if date_accessor => match method.as_deref() {
            None => Read::DateObject(DatePart::of(&variable_name, false).ok_or_else(|| LinkError::iae(format!("Member variable [{variable_name}] does not exist for date object on field [{field}].")))?),
            Some(m) => Read::DateObject(DatePart::of(m, true).ok_or_else(|| LinkError::iae(format!("Member method [{m}] does not exist for date object on field [{field}].")))?),
        },
        Family::Date | Family::Numeric => {
            let what = if fam == Family::Date { "date field" } else { "numeric field" };
            match method.as_deref() {
                None => match variable_name.as_str() {
                    "value" => Read::Min,
                    "empty" => Read::Empty,
                    "length" => Read::Count,
                    v => return Err(LinkError::iae(format!("Member variable [{v}] does not exist for {what} [{field}]."))),
                },
                Some(m) => {
                    use crate::time::legacy as cal;
                    match (m, fam) {
                        ("getValue" | "min", _) => Read::Min,
                        ("isEmpty", _) => Read::Empty,
                        ("size" | "count", _) => Read::Count,
                        ("max", _) => Read::Max,
                        ("avg", _) => Read::Avg,
                        ("median", _) => Read::Median,
                        ("sum", _) => Read::Sum,
                        ("getYear", Family::Date) => Read::Calendar(cal::YEAR),
                        ("getMonth", Family::Date) => Read::Calendar(cal::MONTH),
                        ("getDayOfMonth", Family::Date) => Read::Calendar(cal::DAY_OF_MONTH),
                        ("getHourOfDay", Family::Date) => Read::Calendar(cal::HOUR_OF_DAY),
                        ("getMinutes", Family::Date) => Read::Calendar(cal::MINUTE),
                        ("getSeconds", Family::Date) => Read::Calendar(cal::SECOND),
                        _ => return Err(LinkError::iae(format!("Member method [{m}] does not exist for {what} [{field}]."))),
                    }
                }
            }
        }
        Family::Other => return Err(LinkError::parse(format!("Field [{field}] must be numeric, date, or geopoint"), 5)),
    };
    let slot = match fields.iter().position(|(f, _)| f == field) {
        Some(i) => i,
        None => {
            fields.push((field.clone(), fam));
            fields.len() - 1
        }
    };
    Ok(Source::Doc { field: slot, read })
}

/// The linked variables of an instance.
struct Bound {
    sources: Vec<Source>,
    /// Literal fields read per document.
    fields: Vec<String>,
    /// A `Calendar` for `getYear()`-style reads (reused, as `DateMethodValueSource` does).
    calendar: Option<crate::time::legacy::Cal>,
}

/// An evaluator of an [`Expression`] for one thread or segment.
pub struct Instance<'e> {
    expr: &'e Expression,
    params: Option<Value>,
    value: f64,
    bound: Option<Result<Bound, ScriptError>>,
    vals: Vec<f64>,
    bufs: Vec<DocValues>,
    /// The document set by [`Instance::set_document`] and the value computed for it.
    doc: Option<i32>,
    cached: Option<f64>,
}

impl Instance<'_> {
    /// Binds `params` (numeric values become constants; in the bucket contexts they are the
    /// variables).
    pub fn set_params(&mut self, params: &Json) {
        self.set_params_value(Value::from_json(params, crate::types::cls::HASH_MAP));
    }

    pub fn set_params_value(&mut self, params: Value) {
        self.params = Some(params);
        // bucket scripts read their variables from the params of each call
        if !matches!(self.expr.context, Context::BucketAggregation | Context::AggregationSelector) {
            self.bound = None;
        }
    }

    /// Sets `_value` (aggregation value scripts); other names are ignored.
    pub fn set_var(&mut self, name: &str, v: Value) {
        if name == "_value" {
            self.value = v.as_f64().unwrap_or(0.0);
        }
    }

    /// Marks the document the next executions are about: executions on the same document
    /// return the value computed by the first one, as Lucene's per-document cache does
    /// (Elasticsearch's value scripts over multi-valued fields see the first value's result).
    /// Without it every execution evaluates afresh.
    pub fn set_document(&mut self, doc: i32) {
        if self.doc != Some(doc) {
            self.doc = Some(doc);
            self.cached = None;
        }
    }

    /// Evaluates for the host's current document: a `Value::Double` (a `Value::Bool` in the
    /// `filter` and `aggregation_selector` contexts).
    pub fn execute(&mut self, host: &mut dyn Host) -> Result<Value, ScriptError> {
        let x = self.execute_f64(host)?;
        Ok(match self.expr.context {
            Context::Filter => Value::Bool(x != 0.0),
            Context::AggregationSelector => Value::Bool(x == 1.0),
            _ => Value::Double(x),
        })
    }

    /// The `filter` (non-zero) or `aggregation_selector` (exactly 1) verdict.
    pub fn execute_bool(&mut self, host: &mut dyn Host) -> Result<bool, ScriptError> {
        let x = self.execute_f64(host)?;
        Ok(if self.expr.context == Context::AggregationSelector { x == 1.0 } else { x != 0.0 })
    }

    /// Links the variables now (Elasticsearch links when it creates a segment's script, before
    /// any document): call it with the host positioned anywhere in the segment, even on a
    /// segment without matches, to fail the search with the `link error` up front. Executions
    /// link on their own otherwise.
    pub fn link(&mut self, host: &mut dyn Host) -> Result<(), ScriptError> {
        if matches!(self.expr.context, Context::BucketAggregation | Context::AggregationSelector) {
            return Ok(());
        }
        if self.bound.is_none() {
            self.bound = Some(self.link_variables(host));
        }
        match &self.bound {
            Some(Err(e)) => Err(e.clone()),
            _ => Ok(()),
        }
    }

    /// The expression's value for the host's current document (linking first if needed).
    pub fn execute_f64(&mut self, host: &mut dyn Host) -> Result<f64, ScriptError> {
        if let Some(c) = self.cached {
            return Ok(c);
        }
        if matches!(self.expr.context, Context::BucketAggregation | Context::AggregationSelector) {
            let x = self.execute_bucket()?;
            return Ok(x);
        }
        if self.bound.is_none() {
            self.bound = Some(self.link_variables(host));
        }
        let bound = match self.bound.as_mut() {
            Some(Ok(b)) => b,
            Some(Err(e)) => return Err(e.clone()),
            None => unreachable!(),
        };
        if self.bufs.len() < bound.fields.len() {
            self.bufs.resize_with(bound.fields.len(), DocValues::default);
        }
        // the bound fields are the host's literal-field slots (an instance has its own host)
        for (i, f) in bound.fields.iter().enumerate() {
            match host.doc_values_at(i, f, &mut self.bufs[i]) {
                Ok(true) => {}
                // a field missing from this document's segment reads as no values
                Ok(false) => self.bufs[i].clear(DocKind::Long),
                Err(e) => return Err(script_error("runtime error", &self.expr.source, &self.expr.source, 0, e.kind, Some(e.reason), None)),
            }
        }
        let score_ctx = self.expr.context;
        for (i, s) in bound.sources.iter().enumerate() {
            self.vals[i] = match *s {
                Source::Const(c) => c,
                Source::Score => match score_ctx {
                    // sort and filter scripts are not given scores
                    Context::NumberSort | Context::Filter => 0.0,
                    _ => host.score(),
                },
                Source::AggValue => self.value,
                Source::Doc { field, read } => read_value(&self.bufs[field], read, &mut bound.calendar),
            };
        }
        let x = (self.expr.eval)(&self.vals);
        if self.doc.is_some() {
            self.cached = Some(x);
        }
        Ok(x)
    }

    /// `BucketAggregationScript`: every param is a variable of the expression.
    fn execute_bucket(&mut self) -> Result<f64, ScriptError> {
        let expr = self.expr;
        let plain = |r: String| ScriptError::plain("illegal_argument_exception", r);
        for v in self.vals.iter_mut() {
            *v = 0.0;
        }
        if let Some(Value::Map(m)) = &self.params {
            let d = m.data.borrow();
            for (k, v) in d.m.iter() {
                let name = k.0.to_java_string();
                let Some(i) = expr.variables.iter().position(|x| *x == name) else {
                    return Err(plain(format!("Error using {}. The variable [{name}] does not exist in the executable expressions script.", expr_display(expr))));
                };
                let num = match v {
                    Value::Byte(_) | Value::Short(_) | Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => v.as_f64(),
                    _ => v.other_number().map(|(d, _)| d),
                };
                match num {
                    Some(x) => self.vals[i] = x,
                    None => {
                        return Err(plain(format!(
                            "Error using {}. Executable expressions scripts can only process numbers.  The variable [{name}] is not a number.",
                            expr_display(expr)
                        )));
                    }
                }
            }
        }
        Ok((expr.eval)(&self.vals))
    }

    /// Resolves every variable, in order of first use (the first failure is the link error).
    fn link_variables(&mut self, host: &mut dyn Host) -> Result<Bound, ScriptError> {
        let expr = self.expr;
        let ctx = expr.context;
        let mut fields: Vec<(String, Family)> = Vec::new();
        let mut sources = Vec::with_capacity(expr.variables.len());
        let mut buf = DocValues::default();
        let params = match &self.params {
            Some(Value::Map(m)) => Some(m.clone()),
            _ => None,
        };
        for var in &expr.variables {
            let special = match (var.as_str(), ctx) {
                ("_score", Context::Score | Context::Aggs | Context::NumberSort | Context::Filter) => Some(Source::Score),
                ("_value", Context::Score | Context::Filter) => Some(Source::Const(0.0)),
                ("_value", Context::Aggs) => Some(Source::AggValue),
                _ => None,
            };
            let s = if let Some(s) = special {
                Ok(s)
            } else if let Some(v) = params.as_ref().filter(|m| m.contains(&Value::str(var))).map(|m| m.get_str(var).unwrap_or(Value::Null)) {
                match v {
                    Value::Byte(_) | Value::Short(_) | Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => Ok(Source::Const(v.as_f64().unwrap_or(0.0))),
                    _ => match v.other_number() {
                        Some((d, _)) => Ok(Source::Const(d)),
                        None => Err(LinkError::parse(format!("Parameter [{var}] must be a numeric type"), 0)),
                    },
                }
            } else {
                doc_source(var, host, &mut fields, &mut buf)
            };
            match s {
                Ok(s) => sources.push(s),
                Err(e) => return Err(script_error("link error", &expr.source, var, e.offset, e.kind, Some(e.reason), None)),
            }
        }
        let calendar = sources
            .iter()
            .any(|s| matches!(s, Source::Doc { read: Read::Calendar(_), .. }))
            .then(|| crate::time::legacy::Cal::new(crate::time::legacy::JTimeZone::gmt(), ""));
        Ok(Bound { sources, fields: fields.into_iter().map(|(f, _)| f).collect(), calendar })
    }
}

/// How bucket-script errors print the expression (`Expression.toString()`).
fn expr_display(e: &Expression) -> String {
    format!("org.apache.lucene.expressions.js.JavascriptCompiler$CompiledExpression[{}]", e.source)
}

/// The numeric doc values of a buffer as doubles, in order (`getDoubleValues()`): dates in
/// milliseconds (nanosecond dates truncated), unsigned longs unsigned.
fn number(dv: &DocValues, i: usize) -> f64 {
    use DocKind::*;
    match dv.kind {
        Double | Float | HalfFloat | ScaledFloat => dv.doubles[i],
        UnsignedLong => dv.longs[i] as u64 as f64,
        DateNanos => (dv.longs[i] / 1_000_000) as f64,
        _ => dv.longs[i] as f64,
    }
}

fn count(dv: &DocValues) -> usize {
    match family(dv.kind) {
        Family::Geo => dv.geo.len(),
        Family::Numeric | Family::Date => match dv.kind {
            DocKind::Double | DocKind::Float | DocKind::HalfFloat | DocKind::ScaledFloat => dv.doubles.len(),
            _ => dv.longs.len(),
        },
        Family::Other => 0,
    }
}

/// One variable's value for the current document; a document without values reads 0 (Lucene
/// `zeroWhenUnpositioned`), except `empty`, which is then 1.
fn read_value(dv: &DocValues, read: Read, cal: &mut Option<crate::time::legacy::Cal>) -> f64 {
    let n = count(dv);
    if n == 0 {
        return if read == Read::Empty { 1.0 } else { 0.0 };
    }
    match read {
        Read::Min => number(dv, 0),
        Read::Max => number(dv, n - 1),
        Read::Sum => (0..n).fold(0.0, |s, i| s + number(dv, i)),
        Read::Avg => (0..n).fold(0.0, |s, i| s + number(dv, i)) / n as f64,
        Read::Median => {
            let mid = (n - 1) / 2;
            if n.is_multiple_of(2) { (number(dv, mid) + number(dv, mid + 1)) / 2.0 } else { number(dv, mid) }
        }
        Read::Count => n as f64,
        Read::Empty => 0.0,
        Read::Calendar(f) => {
            let ms = number(dv, 0) as i64;
            match cal {
                Some(c) => {
                    c.set_time_in_millis(ms);
                    c.get(f).map(|v| v as f64).unwrap_or(0.0)
                }
                None => 0.0,
            }
        }
        Read::DateObject(p) => p.at(number(dv, 0) as i64),
        // `GeoEmptyValueSource` answers 1 for a document that has points
        Read::GeoEmpty => 1.0,
        Read::GeoLat => dv.geo[0][0],
        Read::GeoLon => dv.geo[0][1],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(src: &str) -> f64 {
        Expression::compile(src, Context::Field).unwrap_or_else(|e| panic!("{src}: {e:?}")).evaluate(&[])
    }

    #[test]
    fn typed_compilation_follows_lucene() {
        assert_eq!(eval("1 + 2 * 3"), 7.0);
        assert_eq!(eval("!0.5"), 1.0);
        assert_eq!(eval("0.5 ? 1 : 2"), 2.0);
        assert_eq!(eval("0.5 && 1"), 0.0);
        assert_eq!(eval("~0.5"), -1.0);
        assert_eq!(eval("1 << 64"), 1.0);
        assert_eq!(eval("-16 >>> 60"), 15.0);
        assert_eq!(eval("0x10 + 010"), 24.0);
        assert_eq!(eval("1e3 + .5 + 2."), 1002.5);
        assert_eq!(eval("3 & 1 == 1"), 1.0);
        assert!(eval("0 / 0").is_nan());
        assert_eq!(eval("min(-0.0, 0.0)").to_bits(), (-0.0f64).to_bits());
    }

    #[test]
    fn tokens_take_the_longest_match() {
        let units: Vec<u16> = "doc['a\\'b'].x 017 0x1F 1..2 08".encode_utf16().collect();
        let toks = Lexer { c: &units }.tokens().unwrap();
        let kinds: Vec<Tok> = toks.iter().map(|t| t.kind).collect();
        assert_eq!(kinds, [Tok::Variable, Tok::Octal, Tok::Hex, Tok::Decimal, Tok::Decimal, Tok::Decimal, Tok::Decimal, Tok::Eof]);
        let units: Vec<u16> = "1 . x".encode_utf16().collect();
        assert_eq!(Lexer { c: &units }.tokens().unwrap_err().message, "unexpected character '. ' on line (1) position (2)");
    }

    #[test]
    fn variables_are_normalized_and_split() {
        assert_eq!(normalize_quotes("doc[\"a'b\"]"), "doc['a\\'b']");
        assert_eq!(normalize_quotes("doc['x'].min()"), "doc['x'].min()");
        assert_eq!(
            parse_variable("doc['a\\'b'].date.getYear()").unwrap(),
            [Part::Member("doc".into()), Part::StrIndex("a'b".into()), Part::Member("date".into()), Part::Method("getYear".into())]
        );
        assert_eq!(parse_variable("doc[3]").unwrap(), [Part::Member("doc".into()), Part::IntIndex(3)]);
    }

    #[test]
    fn deep_sources_fail_to_compile() {
        let deep = format!("{}1{}", "(".repeat(5000), ")".repeat(5000));
        let e = Expression::compile(&deep, Context::Field).unwrap_err();
        assert_eq!(e.cause_type, "stack_overflow_error");
        let long = vec!["1"; 20_000].join(" + ");
        assert_eq!(Expression::compile(&long, Context::Field).unwrap_err().cause_type, "stack_overflow_error");
        let ok = vec!["1"; 900].join(" + ");
        assert_eq!(Expression::compile(&ok, Context::Field).unwrap().evaluate(&[]), 900.0);
    }

    #[test]
    fn date_parts_use_the_proleptic_calendar() {
        // 1969-12-31T23:59:59.999Z
        let ms = -1;
        assert_eq!(DatePart::Year.at(ms), 1969.0);
        assert_eq!(DatePart::DayOfWeek.at(ms), 3.0);
        assert_eq!(DatePart::MillisOfDay.at(ms), 86_399_999.0);
        assert_eq!(DatePart::Weekyear.at(ms), 1970.0);
        // 0001-01-01 minus a day: year 0, era 0 (BC), year of era 1
        let bc = -62_135_596_800_000 - 1;
        assert_eq!(DatePart::Era.at(bc), 0.0);
        assert_eq!(DatePart::YearOfEra.at(bc), 1.0);
    }
}
