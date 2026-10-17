//! VSIM's FILTER expressions (`expr.c`) and the top-level JSON field extractor they read
//! element attributes with (`fastjson.c`): numbers, strings, tuples, `.field` selectors and
//! `null`; arithmetic, comparisons, `in`, `and`/`or`/`not`, with C's loose conversions.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    OParen,
    CParen,
    Not,
    Pow,
    Mult,
    Div,
    Mod,
    Sum,
    Diff,
    Gt,
    Gte,
    Lt,
    Lte,
    Eq,
    Neq,
    In,
    And,
    Or,
}

/// `ExprOptable`: name, operator, precedence, arity.
const OPTABLE: &[(&[u8], Op, i32, i32)] = &[
    (b"(", Op::OParen, 7, 0),
    (b")", Op::CParen, 7, 0),
    (b"!", Op::Not, 6, 1),
    (b"not", Op::Not, 6, 1),
    (b"**", Op::Pow, 5, 2),
    (b"*", Op::Mult, 4, 2),
    (b"/", Op::Div, 4, 2),
    (b"%", Op::Mod, 4, 2),
    (b"+", Op::Sum, 3, 2),
    (b"-", Op::Diff, 3, 2),
    (b">", Op::Gt, 2, 2),
    (b">=", Op::Gte, 2, 2),
    (b"<", Op::Lt, 2, 2),
    (b"<=", Op::Lte, 2, 2),
    (b"==", Op::Eq, 2, 2),
    (b"!=", Op::Neq, 2, 2),
    (b"in", Op::In, 2, 2),
    (b"and", Op::And, 1, 2),
    (b"&&", Op::And, 1, 2),
    (b"or", Op::Or, 0, 2),
    (b"||", Op::Or, 0, 2),
];

const OP_SPECIALCHARS: &[u8] = b"+-*%/!()<>=|&";

fn precedence(op: Op) -> i32 {
    OPTABLE.iter().find(|e| e.1 == op).map_or(-1, |e| e.2)
}

fn arity(op: Op) -> i32 {
    OPTABLE.iter().find(|e| e.1 == op).map_or(-1, |e| e.3)
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Eof,
    Num(f64),
    Str(Vec<u8>),
    Tuple(Vec<Tok>),
    Selector(Vec<u8>),
    Op(Op),
    Null,
}

/// A compiled expression.
#[derive(Clone, Debug)]
pub struct Expr {
    program: Vec<Tok>,
}

// C's <ctype.h> in the "C" locale.
fn isspace(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn is_selector_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

struct Lexer<'a> {
    s: &'a [u8],
    p: usize,
}

impl Lexer<'_> {
    /// The byte at `p + k`, NUL past the end (the C string's terminator).
    fn at(&self, k: usize) -> u8 {
        self.s.get(self.p + k).copied().unwrap_or(0)
    }

    fn spaces(&mut self) {
        while self.at(0) != 0 && isspace(self.at(0)) {
            self.p += 1;
        }
    }

    fn operator_or_literal(&mut self) -> Option<Tok> {
        let start = self.p;
        while self.at(0) != 0 && (self.at(0).is_ascii_alphabetic() || OP_SPECIALCHARS.contains(&self.at(0))) {
            self.p += 1;
        }
        let word = &self.s[start..self.p];
        if word == b"null" {
            return Some(Tok::Null);
        }
        let mut best: Option<(usize, Op)> = None;
        for &(name, op, _, _) in OPTABLE {
            if name.len() > word.len() || &word[..name.len()] != name {
                continue;
            }
            if best.is_none_or(|(l, _)| name.len() > l) {
                best = Some((name.len(), op));
            }
        }
        let (len, op) = best?;
        self.p = start + len;
        Some(Tok::Op(op))
    }

    fn selector(&mut self) -> Tok {
        self.p += 1;
        let start = self.p;
        while self.at(0) != 0 && is_selector_char(self.at(0)) {
            self.p += 1;
        }
        Tok::Selector(self.s[start..self.p].to_vec())
    }

    fn number(&mut self) -> Option<Tok> {
        let mut buf = Vec::new();
        loop {
            let c = self.at(0);
            if !(c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || (buf.is_empty() && c == b'-')) {
                break;
            }
            if buf.len() >= 255 {
                return None;
            }
            buf.push(c);
            self.p += 1;
        }
        let s = std::str::from_utf8(&buf).ok()?;
        s.parse::<f64>().ok().map(Tok::Num)
    }

    fn string(&mut self) -> Option<Tok> {
        let quote = self.at(0);
        self.p += 1;
        let start = self.p;
        while self.at(0) != 0 {
            if self.at(0) == b'\\' && self.at(1) != 0 {
                self.p += 2;
                continue;
            }
            if self.at(0) == quote {
                let t = Tok::Str(self.s[start..self.p].to_vec());
                self.p += 1;
                return Some(t);
            }
            self.p += 1;
        }
        None
    }

    fn tuple(&mut self) -> Option<Tok> {
        self.p += 1;
        let mut ele = Vec::new();
        loop {
            self.spaces();
            if self.at(0) == b']' {
                self.p += 1;
                break;
            }
            let c = self.at(0);
            let e = if c.is_ascii_digit() || c == b'-' {
                self.number()?
            } else if c == b'"' || c == b'\'' {
                self.string()?
            } else {
                return None;
            };
            ele.push(e);
            self.spaces();
            if self.at(0) == b']' {
                self.p += 1;
                break;
            }
            if self.at(0) != b',' {
                return None;
            }
            self.p += 1;
        }
        Some(Tok::Tuple(ele))
    }

    /// `exprTokenize`; on error the offset where it stopped.
    fn tokenize(&mut self) -> Result<Vec<Tok>, usize> {
        let mut toks: Vec<Tok> = Vec::new();
        loop {
            self.spaces();
            let minus_is_number = match toks.last() {
                None => true,
                Some(Tok::Op(op)) => *op != Op::CParen,
                Some(_) => false,
            };
            let c = self.at(0);
            let cur = if c == 0 {
                Some(Tok::Eof)
            } else if c.is_ascii_digit() || (minus_is_number && c == b'-' && self.at(1).is_ascii_digit()) {
                self.number()
            } else if c == b'"' || c == b'\'' {
                self.string()
            } else if c == b'.' && is_selector_char(self.at(1)) {
                Some(self.selector())
            } else if c == b'[' {
                self.tuple()
            } else if c.is_ascii_alphabetic() || OP_SPECIALCHARS.contains(&c) {
                self.operator_or_literal()
            } else {
                None
            };
            let Some(t) = cur else { return Err(self.p) };
            let eof = t == Tok::Eof;
            toks.push(t);
            if eof {
                return Ok(toks);
            }
        }
    }
}

impl Expr {
    /// `exprCompile`: tokens into a postfix program. On a syntax error, the byte offset to
    /// report it near (0 for errors found after tokenizing: tokens carry no offsets).
    pub fn compile(src: &[u8]) -> Result<Expr, usize> {
        // The expression is a C string: it ends at a NUL.
        let src = &src[..src.iter().position(|&c| c == 0).unwrap_or(src.len())];
        let toks = Lexer { s: src, p: 0 }.tokenize()?;
        let mut program = Vec::new();
        let mut ops: Vec<Op> = Vec::new();
        let mut items: i32 = 0;
        for t in toks {
            match t {
                Tok::Eof => break,
                Tok::Op(op) => process_operator(op, &mut ops, &mut program, &mut items)?,
                v => {
                    program.push(v);
                    items += 1;
                }
            }
        }
        while let Some(op) = ops.pop() {
            if op == Op::OParen {
                return Err(0);
            }
            let a = arity(op);
            if items < a {
                return Err(0);
            }
            program.push(Tok::Op(op));
            items = items - a + 1;
        }
        if items != 1 {
            return Err(0);
        }
        Ok(Expr { program })
    }

    /// `exprRun` against an element's JSON attributes.
    pub fn run(&self, json: &[u8]) -> bool {
        let mut stack: Vec<Tok> = Vec::new();
        for t in &self.program {
            match t {
                Tok::Selector(name) => {
                    if name.is_empty() {
                        return false;
                    }
                    match extract_field(json, name) {
                        Some(v) => stack.push(v),
                        None => return false,
                    }
                }
                Tok::Op(op) => {
                    let b = stack.pop().unwrap_or(Tok::Null);
                    let a = if arity(*op) == 2 { stack.pop() } else { None };
                    let a = a.as_ref().unwrap_or(&Tok::Null);
                    let r = match op {
                        Op::Not => b2f(to_bool(&b) == 0.0),
                        Op::Pow => libm::pow(to_num(a), to_num(&b)),
                        Op::Mult => to_num(a) * to_num(&b),
                        Op::Div => to_num(a) / to_num(&b),
                        Op::Mod => libm::fmod(to_num(a), to_num(&b)),
                        Op::Sum => to_num(a) + to_num(&b),
                        Op::Diff => to_num(a) - to_num(&b),
                        Op::Gt => b2f(to_num(a) > to_num(&b)),
                        Op::Gte => b2f(to_num(a) >= to_num(&b)),
                        Op::Lt => b2f(to_num(a) < to_num(&b)),
                        Op::Lte => b2f(to_num(a) <= to_num(&b)),
                        Op::Eq => b2f(equal(a, &b)),
                        Op::Neq => b2f(!equal(a, &b)),
                        Op::In => match (a, &b) {
                            (_, Tok::Tuple(v)) => b2f(v.iter().any(|e| equal(a, e))),
                            (Tok::Str(x), Tok::Str(y)) => b2f(x.len() <= y.len() && (x.is_empty() || y.windows(x.len()).any(|w| w == x.as_slice()))),
                            _ => 0.0,
                        },
                        Op::And => b2f(to_bool(a) != 0.0 && to_bool(&b) != 0.0),
                        Op::Or => b2f(to_bool(a) != 0.0 || to_bool(&b) != 0.0),
                        Op::OParen | Op::CParen => 0.0,
                    };
                    stack.push(Tok::Num(r));
                }
                v => stack.push(v.clone()),
            }
        }
        stack.pop().is_some_and(|t| to_bool(&t) != 0.0)
    }
}

/// `exprProcessOperator`.
fn process_operator(op: Op, ops: &mut Vec<Op>, program: &mut Vec<Tok>, items: &mut i32) -> Result<(), usize> {
    if op == Op::OParen {
        ops.push(op);
        return Ok(());
    }
    if op == Op::CParen {
        loop {
            let top = ops.pop().ok_or(0usize)?;
            if top == Op::OParen {
                return Ok(());
            }
            let a = arity(top);
            if *items < a {
                return Err(0);
            }
            program.push(Tok::Op(top));
            *items = *items - a + 1;
        }
    }
    let cur = precedence(op);
    while let Some(&top) = ops.last() {
        if top == Op::OParen {
            break;
        }
        let tp = precedence(top);
        if tp < cur || (op == Op::Pow && tp <= cur) {
            break;
        }
        ops.pop();
        let a = arity(top);
        if *items < a {
            return Err(0);
        }
        program.push(Tok::Op(top));
        *items = *items - a + 1;
    }
    ops.push(op);
    Ok(())
}

fn b2f(b: bool) -> f64 {
    if b { 1.0 } else { 0.0 }
}

/// `strtod` that must consume the whole C string: leading spaces skipped, NaN spellings kept.
fn strtod_all(s: &[u8]) -> Option<f64> {
    let s = &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())];
    let start = s.iter().position(|&c| !isspace(c)).unwrap_or(s.len());
    let body = &s[start..];
    if body.is_empty() {
        // No conversion: only an empty string ends where the conversion stopped.
        return if s.is_empty() { Some(0.0) } else { None };
    }
    let lower = body.to_ascii_lowercase();
    let unsigned = lower.strip_prefix(b"+").or_else(|| lower.strip_prefix(b"-")).unwrap_or(&lower);
    if let Some(rest) = unsigned.strip_prefix(b"nan") {
        let ok = rest.is_empty()
            || (rest.first() == Some(&b'(') && rest.last() == Some(&b')') && rest[1..rest.len() - 1].iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_'));
        return ok.then_some(f64::NAN);
    }
    redis_proto::parse_double(body)
}

/// `exprTokenToNum`.
fn to_num(t: &Tok) -> f64 {
    match t {
        Tok::Num(n) => *n,
        Tok::Str(s) if s.len() < 256 => strtod_all(s).unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `exprTokenToBool`.
fn to_bool(t: &Tok) -> f64 {
    match t {
        Tok::Num(n) => b2f(*n != 0.0),
        Tok::Str(s) if s.is_empty() => 0.0,
        Tok::Null => 0.0,
        _ => 1.0,
    }
}

/// `exprTokensEqual`.
fn equal(a: &Tok, b: &Tok) -> bool {
    match (a, b) {
        (Tok::Str(x), Tok::Str(y)) => x == y,
        (Tok::Num(x), Tok::Num(y)) => x == y,
        (Tok::Null, _) | (_, Tok::Null) => matches!((a, b), (Tok::Null, Tok::Null)),
        _ => to_num(a) == to_num(b),
    }
}

// ---------------------------------------------------------------------------------------------
// fastjson.c
// ---------------------------------------------------------------------------------------------

const MAX_DEPTH: u32 = 1000;

fn json_number_char(c: u8) -> bool {
    c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E')
}

fn skip_ws(s: &[u8], p: &mut usize) {
    while *p < s.len() && isspace(s[*p]) {
        *p += 1;
    }
}

fn skip_string(s: &[u8], p: &mut usize) -> bool {
    if *p >= s.len() || s[*p] != b'"' {
        return false;
    }
    *p += 1;
    while *p < s.len() {
        if s[*p] == b'\\' {
            *p += 2;
            continue;
        }
        if s[*p] == b'"' {
            *p += 1;
            return true;
        }
        *p += 1;
    }
    false
}

fn skip_bracketed(s: &[u8], p: &mut usize, open: u8, close: u8) -> bool {
    let mut depth = 1;
    *p += 1;
    while *p < s.len() && depth > 0 {
        let c = s[*p];
        if c == b'"' {
            if !skip_string(s, p) {
                return false;
            }
            continue;
        }
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
        }
        *p += 1;
    }
    depth == 0
}

fn skip_literal(s: &[u8], p: &mut usize, lit: &[u8]) -> bool {
    if s.len() - *p < lit.len() {
        return false;
    }
    if &s[*p..*p + lit.len()] == lit {
        *p += lit.len();
        return true;
    }
    false
}

fn skip_value(s: &[u8], p: &mut usize) -> bool {
    skip_ws(s, p);
    if *p >= s.len() {
        return false;
    }
    match s[*p] {
        b'"' => skip_string(s, p),
        b'{' => skip_bracketed(s, p, b'{', b'}'),
        b'[' => skip_bracketed(s, p, b'[', b']'),
        b't' => skip_literal(s, p, b"true"),
        b'f' => skip_literal(s, p, b"false"),
        b'n' => skip_literal(s, p, b"null"),
        _ => {
            let start = *p;
            while *p < s.len() && json_number_char(s[*p]) {
                *p += 1;
            }
            *p > start
        }
    }
}

fn parse_string(s: &[u8], p: &mut usize) -> Option<Tok> {
    if *p >= s.len() || s[*p] != b'"' {
        return None;
    }
    *p += 1;
    let start = *p;
    let mut q = start;
    let mut esc = false;
    while q < s.len() {
        if esc {
            esc = false;
            q += 1;
            continue;
        }
        if s[q] == b'\\' {
            esc = true;
            q += 1;
            continue;
        }
        if s[q] == b'"' {
            break;
        }
        q += 1;
    }
    if q >= s.len() || s[q] != b'"' {
        return None;
    }
    let mut out = Vec::with_capacity(q - start);
    let mut esc = false;
    for &c in &s[start..q] {
        if esc {
            out.push(match c {
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                other => other,
            });
            esc = false;
        } else if c == b'\\' {
            esc = true;
        } else {
            out.push(c);
        }
    }
    *p = q + 1;
    Some(Tok::Str(out))
}

fn parse_number(s: &[u8], p: &mut usize) -> Option<Tok> {
    let start = *p;
    while *p < s.len() && *p - start < 255 && json_number_char(s[*p]) {
        *p += 1;
    }
    if *p == start {
        return None;
    }
    let txt = std::str::from_utf8(&s[start..*p]).ok()?;
    match txt.parse::<f64>() {
        Ok(v) => Some(Tok::Num(v)),
        Err(_) => {
            *p = start;
            None
        }
    }
}

fn parse_literal(s: &[u8], p: &mut usize, lit: &[u8], tok: Tok) -> Option<Tok> {
    if s.len() - *p < lit.len() || &s[*p..*p + lit.len()] != lit {
        return None;
    }
    if let Some(&next) = s.get(*p + lit.len())
        && !isspace(next)
        && !matches!(next, b',' | b']' | b'}')
    {
        return None;
    }
    *p += lit.len();
    Some(tok)
}

fn parse_array(s: &[u8], p: &mut usize, depth: u32) -> Option<Tok> {
    if *p >= s.len() || s[*p] != b'[' {
        return None;
    }
    *p += 1;
    skip_ws(s, p);
    let mut ele = Vec::new();
    if *p < s.len() && s[*p] == b']' {
        *p += 1;
        return Some(Tok::Tuple(ele));
    }
    loop {
        ele.push(parse_value(s, p, depth + 1)?);
        skip_ws(s, p);
        if *p >= s.len() {
            return None;
        }
        match s[*p] {
            b',' => {
                *p += 1;
                skip_ws(s, p);
            }
            b']' => {
                *p += 1;
                return Some(Tok::Tuple(ele));
            }
            _ => return None,
        }
    }
}

fn parse_value(s: &[u8], p: &mut usize, depth: u32) -> Option<Tok> {
    if depth >= MAX_DEPTH {
        return None;
    }
    skip_ws(s, p);
    if *p >= s.len() {
        return None;
    }
    match s[*p] {
        b'"' => parse_string(s, p),
        b'[' => parse_array(s, p, depth),
        b'{' => None,
        b't' => parse_literal(s, p, b"true", Tok::Num(1.0)),
        b'f' => parse_literal(s, p, b"false", Tok::Num(0.0)),
        b'n' => parse_literal(s, p, b"null", Tok::Null),
        c if c.is_ascii_digit() || c == b'-' || c == b'+' => parse_number(s, p),
        _ => None,
    }
}

/// `jsonSeekField`: where the value of a top-level field starts (keys compared raw).
fn seek_field(s: &[u8], field: &[u8]) -> Option<usize> {
    let mut p = 0;
    skip_ws(s, &mut p);
    if p >= s.len() || s[p] != b'{' {
        return None;
    }
    p += 1;
    loop {
        skip_ws(s, &mut p);
        if p >= s.len() || s[p] == b'}' || s[p] != b'"' {
            return None;
        }
        let key_start = p + 1;
        let mut key_end = p;
        if !skip_string(s, &mut key_end) {
            return None;
        }
        let matched = &s[key_start..key_end - 1] == field;
        p = key_end;
        skip_ws(s, &mut p);
        if p >= s.len() || s[p] != b':' {
            return None;
        }
        p += 1;
        skip_ws(s, &mut p);
        if p >= s.len() {
            return None;
        }
        if matched {
            return Some(p);
        }
        if !skip_value(s, &mut p) {
            return None;
        }
        skip_ws(s, &mut p);
        if p >= s.len() || s[p] != b',' {
            return None;
        }
        p += 1;
    }
}

/// `jsonExtractField`.
fn extract_field(json: &[u8], field: &[u8]) -> Option<Tok> {
    let mut p = seek_field(json, field)?;
    parse_value(json, &mut p, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(e: &str, json: &str) -> bool {
        Expr::compile(e.as_bytes()).unwrap().run(json.as_bytes())
    }

    #[test]
    fn evaluates() {
        let j = r#"{"year": 1984, "name": "The Matrix", "tags": ["a", "b"], "ok": true, "n": null}"#;
        assert!(run("(5+2)*3 and .year > 1980 and 'foo' == 'foo'", j));
        assert!(run(".year == 1984", j));
        assert!(!run(".missing == 1", j));
        assert!(run("'a' in .tags", j));
        assert!(run("'Mat' in .name", j));
        assert!(run(".ok", j));
        assert!(run(".n == null", j));
        assert!(run("2 ** 3 ** 2 == 512", j));
        assert!(run("not (.year < 1000)", j));
        assert!(run("-5 + 3 == -2", j));
        assert!(run(".year in [1984, 1999]", j));
        assert!(run("'10' == 10", j));
    }

    #[test]
    fn syntax_errors() {
        assert_eq!(Expr::compile(b".a ==").err(), Some(0));
        assert_eq!(Expr::compile(b"(.a == 1").err(), Some(0));
        assert_eq!(Expr::compile(b".a == 'x").err(), Some(8));
        assert_eq!(Expr::compile(b".a == 1e-5").err(), Some(8));
        assert_eq!(Expr::compile(b".a @ 1").err(), Some(3));
        // "null)" is read as one word, which is no operator.
        assert!(Expr::compile(b"(.a == null)").is_err());
    }
}
