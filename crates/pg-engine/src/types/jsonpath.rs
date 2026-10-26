//! The SQL/JSON path language (`jsonpath`): parsing to Postgres's canonical text
//! (jsonpath_gram.y / jsonpath.c) and evaluation over `jsonb` with lax and strict semantics
//! (jsonpath_exec.c).

use super::json::{Jsonb, write_json_string};
use super::numeric::Numeric;
use crate::error::{Error, Result, code};
use std::borrow::Cow;

// ── syntax tree ──

#[derive(Clone, Debug)]
pub struct JsonPath {
    pub strict: bool,
    pub expr: Node,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug)]
pub enum Node {
    Null,
    Bool(bool),
    Num(Numeric),
    Str(String),
    Root,
    Current,
    Var(String),
    Last,
    /// An item followed by accessors (`$.a[*]?(…)`).
    Chain(Box<Node>, Vec<Acc>),
    Bin(BinOp, Box<Node>, Box<Node>),
    /// Unary minus (`true`) or plus.
    Unary(bool, Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
    Cmp(CmpOp, Box<Node>, Box<Node>),
    Exists(Box<Node>),
    IsUnknown(Box<Node>),
    StartsWith(Box<Node>, Box<Node>),
    /// Expression, pattern, flags (canonical order `ismxq`).
    LikeRegex(Box<Node>, String, String),
}

/// `.**{first to last}` bound: `u32::MAX` is `last`.
pub const ANY_LAST: u32 = u32::MAX;

#[derive(Clone, Debug)]
pub enum Acc {
    Key(String),
    AnyKey,
    AnyArray,
    Index(Vec<(Node, Option<Node>)>),
    Any(u32, u32),
    Filter(Box<Node>),
    Method(Method),
}

#[derive(Clone, Debug)]
pub enum Method {
    Type,
    Size,
    Double,
    Ceiling,
    Floor,
    Abs,
    KeyValue,
    Bigint,
    Boolean,
    Integer,
    Number,
    String,
    Date,
    Datetime(Option<String>),
    Decimal(Option<Numeric>, Option<Numeric>),
    Time(Option<Numeric>),
    TimeTz(Option<Numeric>),
    Timestamp(Option<Numeric>),
    TimestampTz(Option<Numeric>),
}

impl Method {
    fn name(&self) -> &'static str {
        match self {
            Method::Type => "type",
            Method::Size => "size",
            Method::Double => "double",
            Method::Ceiling => "ceiling",
            Method::Floor => "floor",
            Method::Abs => "abs",
            Method::KeyValue => "keyvalue",
            Method::Bigint => "bigint",
            Method::Boolean => "boolean",
            Method::Integer => "integer",
            Method::Number => "number",
            Method::String => "string",
            Method::Date => "date",
            Method::Datetime(_) => "datetime",
            Method::Decimal(..) => "decimal",
            Method::Time(_) => "time",
            Method::TimeTz(_) => "time_tz",
            Method::Timestamp(_) => "timestamp",
            Method::TimestampTz(_) => "timestamp_tz",
        }
    }
}

impl Node {
    fn is_predicate(&self) -> bool {
        matches!(
            self,
            Node::And(..)
                | Node::Or(..)
                | Node::Not(_)
                | Node::Cmp(..)
                | Node::Exists(_)
                | Node::IsUnknown(_)
                | Node::StartsWith(..)
                | Node::LikeRegex(..)
        )
    }
}

// ── lexer ──

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Str(String),
    Num(String),
    Word(String),
    Var(String),
    P(&'static str),
    End,
}

fn syntax_error(near: Option<&str>) -> Error {
    match near {
        // (an empty token text is flex's end-of-buffer: a word ended by a special character)
        Some(t) if !t.is_empty() => Error::new(
            code::SYNTAX_ERROR,
            format!("syntax error at or near \"{t}\" of jsonpath input"),
        ),
        _ => Error::new(code::SYNTAX_ERROR, "syntax error at end of jsonpath input"),
    }
}

/// jsonpath_scan.l's character classes.
fn is_special(c: char) -> bool {
    "?%$.[]{}()|&!=<>@#,*:-+/".contains(c)
}

fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}')
}

/// "other": anything that's not special, blank, `\` or `"`.
fn is_other(c: char) -> bool {
    !is_special(c) && !is_blank(c) && c != '\\' && c != '"'
}

/// The longest match of the scanner's numeric rules at `b[0..]` (flex picks the longest,
/// the earliest rule on a tie): its kind and length. Kinds: 0 real, 1 decimal, 2 decinteger,
/// 3 hex, 4 oct, 5 bin, 6 realfail, 7-9 junk (decinteger, decimal, real).
fn number_rule(b: &[char]) -> Option<(u8, usize)> {
    let digit = |i: usize| b.get(i).is_some_and(|c| c.is_ascii_digit());
    // (_?digit)* from i
    let digits_tail = |mut i: usize, ok: &dyn Fn(char) -> bool| {
        loop {
            if b.get(i).is_some_and(|c| ok(*c)) {
                i += 1;
            } else if b.get(i) == Some(&'_') && b.get(i + 1).is_some_and(|c| ok(*c)) {
                i += 2;
            } else {
                return i;
            }
        }
    };
    let dec = |c: char| c.is_ascii_digit();
    // decinteger: 0 | [1-9](_?digit)*
    let decinteger = match b.first() {
        Some('0') => Some(1),
        Some(c) if c.is_ascii_digit() => Some(digits_tail(1, &dec)),
        _ => None,
    };
    // decdigits from i: digit(_?digit)*
    let decdigits = |i: usize| digit(i).then(|| digits_tail(i + 1, &dec));
    // decimal: decinteger . decdigits? | . decdigits
    let decimal = match decinteger {
        Some(n) if b.get(n) == Some(&'.') => Some(decdigits(n + 1).unwrap_or(n + 1)),
        None if b.first() == Some(&'.') => decdigits(1),
        _ => None,
    };
    let exp = |base: usize| -> (Option<usize>, Option<usize>) {
        if !matches!(b.get(base), Some('e' | 'E')) {
            return (None, None);
        }
        let mut i = base + 1;
        let sign = matches!(b.get(i), Some('+' | '-'));
        if sign {
            i += 1;
        }
        match decdigits(i) {
            Some(n) => (Some(n), None),
            None if sign => (None, Some(i)),
            None => (None, None),
        }
    };
    let (mut real, mut realfail) = (None, None);
    for base in [decimal, decinteger].into_iter().flatten() {
        let (r, f) = exp(base);
        real = real.max(r);
        realfail = realfail.max(f);
    }
    let radix = |p: char, ok: &dyn Fn(char) -> bool| {
        (b.first() == Some(&'0')
            && b.get(1).is_some_and(|c| c.eq_ignore_ascii_case(&p))
            && b.get(2).is_some_and(|c| ok(*c)))
        .then(|| digits_tail(3, ok))
    };
    let hex = radix('x', &|c: char| c.is_ascii_hexdigit());
    let oct = radix('o', &|c: char| ('0'..='7').contains(&c));
    let bin = radix('b', &|c: char| c == '0' || c == '1');
    let junk = |n: Option<usize>| n.filter(|n| b.get(*n).is_some_and(|c| is_other(*c))).map(|n| n + 1);
    let rules = [
        real,
        decimal,
        decinteger,
        hex,
        oct,
        bin,
        realfail,
        junk(decinteger),
        junk(decimal),
        junk(real),
    ];
    let mut best: Option<(u8, usize)> = None;
    for (k, n) in rules.iter().enumerate() {
        if let Some(n) = n
            && best.is_none_or(|b| *n > b.1)
        {
            best = Some((k as u8, *n));
        }
    }
    best
}

/// One escape sequence (`\…`) of a string or unquoted word at `b[0] == '\\'`: what it adds,
/// and its length.
fn lex_escape(b: &[char]) -> Result<(String, usize)> {
    // How far the escape reaches; it is then read by a quoted string's rules.
    let mut len = 2;
    if b.get(1) == Some(&'x') {
        len = 2 + b[2..].iter().take(2).take_while(|c| c.is_ascii_hexdigit()).count();
    } else if b.get(1) == Some(&'u') {
        if b.get(2) == Some(&'{') {
            let h = b[3..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
            len = 3 + h + 1;
        } else {
            len = 6;
            // a surrogate pair continues into the next escape
            if let Ok(cp) = u32::from_str_radix(&b[2..6.min(b.len())].iter().collect::<String>(), 16)
                && (0xd800..0xdc00).contains(&cp)
                && b.get(6) == Some(&'\\')
                && b.get(7) == Some(&'u')
            {
                len = 12;
            }
        }
    }
    let mut q: Vec<char> = vec!['"'];
    q.extend(b.iter().take(len.min(b.len())));
    q.push('"');
    let (s, _) = lex_string(&q)?;
    Ok((s, len.min(b.len())))
}

fn lex(s: &str) -> Result<Vec<(Tok, String)>> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if is_blank(c) {
            i += 1;
            continue;
        }
        if c == '/' && b.get(i + 1) == Some(&'*') {
            match (i + 2..b.len().saturating_sub(1)).find(|&j| b[j] == '*' && b[j + 1] == '/') {
                Some(j) => {
                    i = j + 2;
                    continue;
                }
                None => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        "unexpected end of comment at end of jsonpath input",
                    ));
                }
            }
        }
        let start = i;
        let tok = if c == '"' {
            let (s, n) = lex_string(&b[i..])?;
            i += n;
            Tok::Str(s)
        } else if c == '$' && b.get(i + 1) == Some(&'"') {
            let (s, n) = lex_string(&b[i + 1..])?;
            i += n + 1;
            Tok::Var(s)
        } else if c == '$' && b.get(i + 1).is_some_and(|c| is_other(*c)) {
            i += 1;
            while i < b.len() && is_other(b[i]) {
                i += 1;
            }
            Tok::Var(b[start + 1..i].iter().collect())
        } else if let Some((kind, n)) = (c.is_ascii_digit() || c == '.')
            .then(|| number_rule(&b[i..]))
            .flatten()
            .filter(|(_, n)| {
                // `{other}+` (an unquoted word) is the last rule: it wins only when longer.
                *n >= b[i..].iter().take_while(|c| is_other(**c)).count()
            })
        {
            let text: String = b[i..i + n].iter().collect();
            if kind >= 6 {
                let what = if kind == 6 {
                    "invalid numeric literal"
                } else {
                    "trailing junk after numeric literal"
                };
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!("{what} at or near \"{text}\" of jsonpath input"),
                ));
            }
            i += n;
            let (num, _) = lex_number(&b[start..i])?;
            Tok::Num(num)
        } else if is_other(c) || c == '\\' {
            // An unquoted word (xnq): other characters and escapes, up to a special
            // character, a blank, a quote or the end; flex's text at its end is what an
            // error at the word shows (the blanks, or nothing).
            let mut w = String::new();
            loop {
                match b.get(i) {
                    Some(&c) if is_other(c) => {
                        w.push(c);
                        i += 1;
                    }
                    Some('\\') => {
                        if i + 1 >= b.len() {
                            return Err(Error::new(
                                code::SYNTAX_ERROR,
                                "unexpected end after backslash at or near \"\\\" of jsonpath input",
                            ));
                        }
                        let (e, n) = lex_escape(&b[i..])?;
                        w.push_str(&e);
                        i += n;
                    }
                    _ => break,
                }
            }
            let mut j = i;
            while j < b.len() && is_blank(b[j]) {
                j += 1;
            }
            let near: String = b[i..j].iter().collect();
            out.push((Tok::Word(w), near));
            i = j;
            continue;
        } else {
            let two: String = b[i..(i + 2).min(b.len())].iter().collect();
            let p = ["==", "!=", "<>", "<=", ">=", "&&", "||", "**"]
                .into_iter()
                .find(|p| *p == two);
            if let Some(p) = p {
                i += 2;
                Tok::P(if p == "<>" { "!=" } else { p })
            } else {
                let one = match c {
                    '$' => "$",
                    '@' => "@",
                    '.' => ".",
                    ',' => ",",
                    '[' => "[",
                    ']' => "]",
                    '(' => "(",
                    ')' => ")",
                    '{' => "{",
                    '}' => "}",
                    '?' => "?",
                    '*' => "*",
                    '+' => "+",
                    '-' => "-",
                    '/' => "/",
                    '%' => "%",
                    '!' => "!",
                    '<' => "<",
                    '>' => ">",
                    // (Single-character tokens no rule takes: the parser stops at them.)
                    ':' => ":",
                    '|' => "|",
                    '&' => "&",
                    '=' => "=",
                    '#' => "#",
                    _ => return Err(syntax_error(Some(&c.to_string()))),
                };
                i += 1;
                Tok::P(one)
            }
        };
        out.push((tok, b[start..i].iter().collect()));
    }
    out.push((Tok::End, String::new()));
    Ok(out)
}

/// A double-quoted string starting at `b[0]`: its value and length in chars.
fn lex_string(b: &[char]) -> Result<(String, usize)> {
    let mut s = String::new();
    let mut i = 1;
    let bad = |what: &str| Error::new(code::SYNTAX_ERROR, format!("{what} of jsonpath input"));
    // Where the current run of \u escapes started and ended (the scanner's {unicode}* that
    // an {unicodefail} error shows).
    let mut uni_run: Option<(usize, usize)> = None;
    loop {
        let Some(&c) = b.get(i) else {
            return Err(bad("unexpected end of quoted string"));
        };
        i += 1;
        match c {
            '"' => return Ok((s, i)),
            '\\' => {
                let Some(&e) = b.get(i) else {
                    return Err(bad("unexpected end after backslash"));
                };
                let esc_start = i - 1;
                i += 1;
                // {unicode}*{unicodefail}: the failing escape's text, after any valid ones
                // right before it.
                let unicode_fail = || {
                    let run = match uni_run {
                        Some((st, end)) if end == esc_start => st,
                        _ => esc_start,
                    };
                    let mut j = esc_start + 2;
                    if b.get(j) == Some(&'{') {
                        j += 1 + b[j + 1..].iter().take(6).take_while(|c| c.is_ascii_hexdigit()).count();
                    } else {
                        j += b[j..].iter().take(3).take_while(|c| c.is_ascii_hexdigit()).count();
                    }
                    let near: String = b[run..j.min(b.len())].iter().collect();
                    Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "invalid Unicode escape sequence at or near \"{near}\" of jsonpath input"
                        ),
                    )
                };
                match e {
                    'b' => s.push('\u{8}'),
                    'f' => s.push('\u{c}'),
                    'n' => s.push('\n'),
                    'r' => s.push('\r'),
                    't' => s.push('\t'),
                    'v' => s.push('\u{b}'),
                    'x' => {
                        let hex: String = b[i..]
                            .iter()
                            .take(2)
                            .take_while(|c| c.is_ascii_hexdigit())
                            .collect();
                        if hex.is_empty() {
                            return Err(bad("invalid hexadecimal character sequence"));
                        }
                        i += hex.len();
                        s.push(
                            char::from_u32(u32::from_str_radix(&hex, 16).unwrap())
                                .unwrap_or('\u{fffd}'),
                        );
                    }
                    'u' => {
                        let (cp, n) = if b.get(i) == Some(&'{') {
                            let hex: String = b[i + 1..]
                                .iter()
                                .take_while(|c| c.is_ascii_hexdigit())
                                .collect();
                            if hex.is_empty()
                                || hex.len() > 6
                                || b.get(i + 1 + hex.len()) != Some(&'}')
                            {
                                return Err(unicode_fail());
                            }
                            (
                                u32::from_str_radix(&hex, 16).map_err(|_| unicode_fail())?,
                                hex.len() + 2,
                            )
                        } else {
                            let hex: String = b[i..].iter().take(4).collect();
                            if hex.len() < 4 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                                return Err(unicode_fail());
                            }
                            (u32::from_str_radix(&hex, 16).unwrap(), 4)
                        };
                        i += n;
                        uni_run = Some((
                            match uni_run {
                                Some((st, end)) if end == esc_start => st,
                                _ => esc_start,
                            },
                            i,
                        ));
                        let surrogate = |detail: &str| {
                            Error::new(
                                code::INVALID_TEXT_REPRESENTATION,
                                "invalid input syntax for type jsonpath",
                            )
                            .detail(detail)
                        };
                        let cp = if (0xd800..0xdc00).contains(&cp)
                            && b.get(i) == Some(&'\\')
                            && b.get(i + 1) == Some(&'u')
                        {
                            let lo: String = b[i + 2..].iter().take(4).collect();
                            let lo = u32::from_str_radix(&lo, 16)
                                .map_err(|_| bad("invalid Unicode surrogate pair"))?;
                            if (0xd800..0xdc00).contains(&lo) {
                                return Err(surrogate(
                                    "Unicode high surrogate must not follow a high surrogate.",
                                ));
                            }
                            if !(0xdc00..0xe000).contains(&lo) {
                                return Err(surrogate(
                                    "Unicode low surrogate must follow a high surrogate.",
                                ));
                            }
                            i += 6;
                            0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00)
                        } else if (0xd800..0xe000).contains(&cp) {
                            return Err(surrogate(
                                "Unicode low surrogate must follow a high surrogate.",
                            ));
                        } else {
                            cp
                        };
                        if let Some((st, _)) = uni_run {
                            uni_run = Some((st, i));
                        }
                        if cp == 0 {
                            return Err(Error::new(
                                code::UNTRANSLATABLE_CHARACTER,
                                "unsupported Unicode escape sequence",
                            )
                            .detail("\\u0000 cannot be converted to text."));
                        }
                        s.push(
                            char::from_u32(cp)
                                .ok_or_else(|| bad("invalid Unicode escape sequence"))?,
                        );
                    }
                    e => s.push(e),
                }
            }
            c => s.push(c),
        }
    }
}

/// A numeric literal: decimal (with `_` separators, fraction, exponent) or 0x / 0o / 0b.
fn lex_number(b: &[char]) -> Result<(String, usize)> {
    let mut i = 0;
    if b.len() > 2 && b[0] == '0' && matches!(b[1], 'x' | 'X' | 'o' | 'O' | 'b' | 'B') {
        let radix = match b[1] {
            'x' | 'X' => 16,
            'o' | 'O' => 8,
            _ => 2,
        };
        i = 2;
        while i < b.len() && (b[i].is_digit(radix) || b[i] == '_') {
            i += 1;
        }
        let digits: String = b[2..i].iter().filter(|c| **c != '_').collect();
        let v = num_bigint::BigInt::parse_bytes(digits.as_bytes(), radix)
            .ok_or_else(|| syntax_error(None))?;
        return Ok((v.to_string(), i));
    }
    let digits = |i: &mut usize| {
        while *i < b.len()
            && (b[*i].is_ascii_digit()
                || b[*i] == '_' && b.get(*i + 1).is_some_and(|c| c.is_ascii_digit()))
        {
            *i += 1;
        }
    };
    digits(&mut i);
    if i < b.len()
        && b[i] == '.'
        && !b
            .get(i + 1)
            .is_some_and(|c| c.is_alphabetic() && !matches!(c, 'e' | 'E'))
    {
        i += 1;
        digits(&mut i);
    }
    if i < b.len() && matches!(b[i], 'e' | 'E') {
        let mut j = i + 1;
        if j < b.len() && matches!(b[j], '+' | '-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            i = j;
            digits(&mut i);
        }
    }
    Ok((b[..i].iter().filter(|c| **c != '_').collect(), i))
}

// ── parser ──

struct Parser {
    toks: Vec<(Tok, String)>,
    pos: usize,
    in_filter: usize,
    in_subscript: usize,
}

const METHODS: &[&str] = &[
    "type",
    "size",
    "double",
    "ceiling",
    "floor",
    "abs",
    "keyvalue",
    "bigint",
    "boolean",
    "integer",
    "number",
    "string",
    "date",
    "datetime",
    "decimal",
    "time",
    "time_tz",
    "timestamp",
    "timestamp_tz",
];

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].0
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].0
    }

    fn word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Word(x) if x.eq_ignore_ascii_case(w))
    }

    fn err(&self) -> Error {
        match self.peek() {
            Tok::End => syntax_error(None),
            _ => syntax_error(Some(&self.toks[self.pos].1)),
        }
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].0.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, p: &str) -> bool {
        if matches!(self.peek(), Tok::P(x) if *x == p) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, p: &str) -> Result<()> {
        if self.eat(p) { Ok(()) } else { Err(self.err()) }
    }

    fn predicate(&mut self) -> Result<Node> {
        let n = self.or()?;
        if !n.is_predicate() {
            return Err(self.err());
        }
        Ok(n)
    }

    fn or(&mut self) -> Result<Node> {
        let mut l = self.and()?;
        while matches!(self.peek(), Tok::P("||")) {
            if !l.is_predicate() {
                return Err(self.err());
            }
            self.next();
            let r = self.and()?;
            if !r.is_predicate() {
                return Err(self.err());
            }
            l = Node::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and(&mut self) -> Result<Node> {
        let mut l = self.not()?;
        while matches!(self.peek(), Tok::P("&&")) {
            if !l.is_predicate() {
                return Err(self.err());
            }
            self.next();
            let r = self.not()?;
            if !r.is_predicate() {
                return Err(self.err());
            }
            l = Node::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn not(&mut self) -> Result<Node> {
        if self.eat("!") {
            // Only a parenthesized predicate or exists(…) can be negated.
            let n = if self.word("exists") {
                self.primary()?
            } else {
                self.expect("(")?;
                let p = self.predicate()?;
                self.expect(")")?;
                p
            };
            if !n.is_predicate() {
                return Err(self.err());
            }
            return Ok(Node::Not(Box::new(n)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Node> {
        let l = self.additive()?;
        let op = match self.peek() {
            Tok::P("==") => Some(CmpOp::Eq),
            Tok::P("!=") => Some(CmpOp::Ne),
            Tok::P("<") => Some(CmpOp::Lt),
            Tok::P("<=") => Some(CmpOp::Le),
            Tok::P(">") => Some(CmpOp::Gt),
            Tok::P(">=") => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            if l.is_predicate() {
                return Err(self.err());
            }
            self.next();
            let r = self.additive()?;
            if r.is_predicate() {
                return Err(self.err());
            }
            return Ok(Node::Cmp(op, Box::new(l), Box::new(r)));
        }
        if self.word("starts")
            && matches!(self.peek_at(1), Tok::Word(w) if w.eq_ignore_ascii_case("with"))
        {
            if l.is_predicate() {
                return Err(self.err());
            }
            self.next();
            self.next();
            let r = match self.next() {
                Tok::Str(s) => Node::Str(s),
                Tok::Var(v) => Node::Var(v),
                _ => {
                    self.pos -= 1;
                    return Err(self.err());
                }
            };
            return Ok(Node::StartsWith(Box::new(l), Box::new(r)));
        }
        if self.word("like_regex") {
            if l.is_predicate() {
                return Err(self.err());
            }
            self.next();
            let Tok::Str(pattern) = self.next() else {
                self.pos -= 1;
                return Err(self.err());
            };
            let mut flags = String::new();
            if self.word("flag") {
                self.next();
                let Tok::Str(f) = self.next() else {
                    self.pos -= 1;
                    return Err(self.err());
                };
                for c in f.chars() {
                    if !"ismxq".contains(c) {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "invalid input syntax for type jsonpath",
                        )
                        .detail(format!(
                            "Unrecognized flag character \"{c}\" in LIKE_REGEX predicate."
                        )));
                    }
                }
                flags = "ismxq".chars().filter(|c| f.contains(*c)).collect();
            }
            // makeItemLikeRegex: the flags must convert, and the pattern compile.
            let cflags = like_regex_cflags(&flags)?;
            crate::types::regex::Regex::new(&pattern, cflags, crate::types::regex::default_strategy())?;
            return Ok(Node::LikeRegex(Box::new(l), pattern, flags));
        }
        Ok(l)
    }

    fn additive(&mut self) -> Result<Node> {
        let mut l = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::P("+") => BinOp::Add,
                Tok::P("-") => BinOp::Sub,
                _ => return Ok(l),
            };
            if l.is_predicate() {
                return Err(self.err());
            }
            self.next();
            let r = self.multiplicative()?;
            if r.is_predicate() {
                return Err(self.err());
            }
            l = Node::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn multiplicative(&mut self) -> Result<Node> {
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::P("*") => BinOp::Mul,
                Tok::P("/") => BinOp::Div,
                Tok::P("%") => BinOp::Mod,
                _ => return Ok(l),
            };
            if l.is_predicate() {
                return Err(self.err());
            }
            self.next();
            let r = self.unary()?;
            if r.is_predicate() {
                return Err(self.err());
            }
            l = Node::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn unary(&mut self) -> Result<Node> {
        let minus = match self.peek() {
            Tok::P("-") => true,
            Tok::P("+") => false,
            _ => return self.accessor_expr(),
        };
        self.next();
        let a = self.unary()?;
        if a.is_predicate() {
            return Err(self.err());
        }
        // Signs fold into numeric literals (makeItemUnary).
        Ok(match a {
            Node::Num(n) if minus => Node::Num(n.neg()),
            Node::Num(n) => Node::Num(n),
            a => Node::Unary(minus, Box::new(a)),
        })
    }

    fn accessor_expr(&mut self) -> Result<Node> {
        let parenthesized = matches!(self.peek(), Tok::P("("));
        let base = self.primary()?;
        let mut accs = Vec::new();
        loop {
            match self.peek() {
                Tok::P(".") => {
                    self.next();
                    accs.push(self.dot_accessor()?);
                }
                Tok::P("[") => {
                    self.next();
                    if self.eat("*") {
                        self.expect("]")?;
                        accs.push(Acc::AnyArray);
                        continue;
                    }
                    self.in_subscript += 1;
                    let mut items = Vec::new();
                    loop {
                        let from = self.or()?;
                        let to = if self.word("to") {
                            self.next();
                            Some(self.or()?)
                        } else {
                            None
                        };
                        items.push((from, to));
                        if !self.eat(",") {
                            break;
                        }
                    }
                    self.in_subscript -= 1;
                    self.expect("]")?;
                    accs.push(Acc::Index(items));
                }
                Tok::P("?") => {
                    self.next();
                    self.expect("(")?;
                    self.in_filter += 1;
                    let p = self.predicate()?;
                    self.in_filter -= 1;
                    self.expect(")")?;
                    accs.push(Acc::Filter(Box::new(p)));
                }
                _ => break,
            }
        }
        if accs.is_empty() {
            return Ok(base);
        }
        // ('(' predicate ')' accessor_op)
        if base.is_predicate() && !matches!(base, Node::Exists(_)) && !parenthesized {
            return Err(self.err());
        }
        Ok(Node::Chain(Box::new(base), accs))
    }

    fn dot_accessor(&mut self) -> Result<Acc> {
        match self.next() {
            Tok::P("*") => Ok(Acc::AnyKey),
            Tok::P("**") => {
                let (mut first, mut last) = (0, ANY_LAST);
                if self.eat("{") {
                    let bound = |p: &mut Parser| -> Result<u32> {
                        match p.next() {
                            Tok::Word(w) if w.eq_ignore_ascii_case("last") => Ok(ANY_LAST),
                            Tok::Num(n) => n.parse::<u32>().map_err(|_| syntax_error(Some(&n))),
                            _ => {
                                p.pos -= 1;
                                Err(p.err())
                            }
                        }
                    };
                    first = bound(self)?;
                    last = first;
                    if self.word("to") {
                        self.next();
                        last = bound(self)?;
                    }
                    self.expect("}")?;
                }
                Ok(Acc::Any(first, last))
            }
            Tok::Str(s) => Ok(Acc::Key(s)),
            Tok::Var(v) => Err(syntax_error(Some(&format!("${v}")))),
            Tok::Word(w) => {
                let lw = w.to_ascii_lowercase();
                if matches!(self.peek(), Tok::P("(")) && METHODS.contains(&lw.as_str()) {
                    self.next();
                    let m = self.method(&lw)?;
                    self.expect(")")?;
                    return Ok(Acc::Method(m));
                }
                Ok(Acc::Key(w))
            }
            Tok::Num(_) | Tok::P(_) => {
                self.pos -= 1;
                Err(self.err())
            }
            Tok::End => Err(syntax_error(None)),
        }
    }

    fn int_arg(&mut self) -> Result<Option<Numeric>> {
        let neg = self.eat("-");
        if !neg {
            self.eat("+");
        }
        match self.peek().clone() {
            Tok::Num(n) => {
                self.next();
                let v = Numeric::parse_pg(&n)?;
                Ok(Some(if neg { v.neg() } else { v }))
            }
            _ if neg => Err(self.err()),
            _ => Ok(None),
        }
    }

    /// opt_datetime_precision: an unsigned integer literal, or nothing.
    fn prec_arg(&mut self) -> Result<Option<Numeric>> {
        match self.peek().clone() {
            Tok::Num(n) if n.bytes().all(|b| b.is_ascii_digit() || b == b'_') => {
                self.next();
                Ok(Some(Numeric::parse_pg(&n)?))
            }
            Tok::Num(_) => Err(self.err()),
            _ => Ok(None),
        }
    }

    fn method(&mut self, name: &str) -> Result<Method> {
        Ok(match name {
            "type" => Method::Type,
            "size" => Method::Size,
            "double" => Method::Double,
            "ceiling" => Method::Ceiling,
            "floor" => Method::Floor,
            "abs" => Method::Abs,
            "keyvalue" => Method::KeyValue,
            "bigint" => Method::Bigint,
            "boolean" => Method::Boolean,
            "integer" => Method::Integer,
            "number" => Method::Number,
            "string" => Method::String,
            "date" => Method::Date,
            "datetime" => match self.peek().clone() {
                Tok::Str(s) => {
                    self.next();
                    Method::Datetime(Some(s))
                }
                _ => Method::Datetime(None),
            },
            "decimal" => {
                let p = self.int_arg()?;
                let s = if p.is_some() && self.eat(",") {
                    let s = self.int_arg()?;
                    if s.is_none() {
                        return Err(self.err());
                    }
                    s
                } else {
                    None
                };
                Method::Decimal(p, s)
            }
            "time" => Method::Time(self.prec_arg()?),
            "time_tz" => Method::TimeTz(self.prec_arg()?),
            "timestamp" => Method::Timestamp(self.prec_arg()?),
            "timestamp_tz" => Method::TimestampTz(self.prec_arg()?),
            _ => return Err(self.err()),
        })
    }

    fn primary(&mut self) -> Result<Node> {
        let t = self.next();
        Ok(match t {
            Tok::Num(n) => Node::Num(Numeric::parse_pg(&n).map_err(|_| syntax_error(Some(&n)))?),
            Tok::Str(s) => Node::Str(s),
            Tok::Var(v) => Node::Var(v),
            Tok::P("$") => Node::Root,
            Tok::P("@") => {
                if self.in_filter == 0 {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        "@ is not allowed in root expressions",
                    ));
                }
                Node::Current
            }
            Tok::P("(") => {
                let e = self.or()?;
                self.expect(")")?;
                if e.is_predicate()
                    && self.word("is")
                    && matches!(self.peek_at(1), Tok::Word(w) if w.eq_ignore_ascii_case("unknown"))
                {
                    self.next();
                    self.next();
                    return Ok(Node::IsUnknown(Box::new(e)));
                }
                e
            }
            Tok::Word(w) => match w.to_ascii_lowercase().as_str() {
                "true" => Node::Bool(true),
                "false" => Node::Bool(false),
                "null" => Node::Null,
                "last" => {
                    if self.in_subscript == 0 {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "LAST is allowed only in array subscripts",
                        ));
                    }
                    Node::Last
                }
                "exists" => {
                    self.expect("(")?;
                    let e = self.or()?;
                    if e.is_predicate() {
                        return Err(self.err());
                    }
                    self.expect(")")?;
                    Node::Exists(Box::new(e))
                }
                _ => {
                    self.pos -= 1;
                    return Err(self.err());
                }
            },
            Tok::End => return Err(syntax_error(None)),
            Tok::P(_) => {
                self.pos -= 1;
                return Err(self.err());
            }
        })
    }
}

pub fn parse(s: &str) -> Result<JsonPath> {
    let toks = lex(s)?;
    // An empty path (nothing but blanks and comments) isn't one.
    if matches!(toks.first(), Some((Tok::End, _))) {
        return Err(Error::new(
            code::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type jsonpath: \"{s}\""),
        ));
    }
    let mut p = Parser {
        toks,
        pos: 0,
        in_filter: 0,
        in_subscript: 0,
    };
    let mut strict = false;
    if p.word("strict") || p.word("lax") {
        strict = p.word("strict");
        p.next();
    }
    let expr = p.or()?;
    if !matches!(p.peek(), Tok::End) {
        return Err(p.err());
    }
    Ok(JsonPath { strict, expr })
}

// ── canonical text (printJsonPathItem) ──

fn priority(n: &Node) -> u8 {
    match n {
        // A chain prints as its first item (operationPriority of that item).
        Node::Chain(base, _) => priority(base),
        Node::Or(..) => 0,
        Node::And(..) => 1,
        Node::Cmp(..) | Node::StartsWith(..) => 2,
        Node::Bin(BinOp::Add | BinOp::Sub, ..) => 3,
        Node::Bin(..) => 4,
        Node::Unary(..) => 5,
        _ => 6,
    }
}

fn print(out: &mut String, n: &Node, brackets: bool, has_next: bool) {
    let binary = |out: &mut String, op: &str, l: &Node, r: &Node| {
        if brackets {
            out.push('(');
        }
        print(out, l, priority(l) <= priority(n), false);
        out.push(' ');
        out.push_str(op);
        out.push(' ');
        print(out, r, priority(r) <= priority(n), false);
        if brackets {
            out.push(')');
        }
    };
    match n {
        Node::Null => out.push_str("null"),
        Node::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Node::Num(v) => {
            if has_next {
                out.push('(');
            }
            out.push_str(&v.to_string());
            if has_next {
                out.push(')');
            }
        }
        Node::Str(s) => write_json_string(out, s),
        Node::Root => out.push('$'),
        Node::Current => out.push('@'),
        Node::Var(v) => {
            out.push('$');
            write_json_string(out, v);
        }
        Node::Last => out.push_str("last"),
        Node::Chain(base, accs) => {
            print(out, base, brackets, true);
            for a in accs {
                print_acc(out, a);
            }
        }
        Node::Bin(op, l, r) => {
            let s = match op {
                BinOp::Add => "+",
                BinOp::Sub => "-",
                BinOp::Mul => "*",
                BinOp::Div => "/",
                BinOp::Mod => "%",
            };
            binary(out, s, l, r)
        }
        Node::Cmp(op, l, r) => {
            let s = match op {
                CmpOp::Eq => "==",
                CmpOp::Ne => "!=",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
            };
            binary(out, s, l, r)
        }
        Node::And(l, r) => binary(out, "&&", l, r),
        Node::Or(l, r) => binary(out, "||", l, r),
        Node::StartsWith(l, r) => binary(out, "starts with", l, r),
        Node::Unary(minus, a) => {
            if brackets {
                out.push('(');
            }
            out.push(if *minus { '-' } else { '+' });
            print(out, a, priority(a) <= priority(n), false);
            if brackets {
                out.push(')');
            }
        }
        Node::Not(a) => {
            out.push_str("!(");
            print(out, a, false, false);
            out.push(')');
        }
        Node::IsUnknown(a) => {
            out.push('(');
            print(out, a, false, false);
            out.push_str(") is unknown");
        }
        Node::Exists(a) => {
            out.push_str("exists (");
            print(out, a, false, false);
            out.push(')');
        }
        Node::LikeRegex(e, pattern, flags) => {
            if brackets {
                out.push('(');
            }
            print(out, e, priority(e) <= priority(n), false);
            out.push_str(" like_regex ");
            write_json_string(out, pattern);
            if !flags.is_empty() {
                out.push_str(&format!(" flag \"{flags}\""));
            }
            if brackets {
                out.push(')');
            }
        }
    }
}

fn print_acc(out: &mut String, a: &Acc) {
    match a {
        Acc::Key(k) => {
            out.push('.');
            write_json_string(out, k);
        }
        Acc::AnyKey => out.push_str(".*"),
        Acc::AnyArray => out.push_str("[*]"),
        Acc::Index(items) => {
            out.push('[');
            for (i, (from, to)) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                print(out, from, false, false);
                if let Some(to) = to {
                    out.push_str(" to ");
                    print(out, to, false, false);
                }
            }
            out.push(']');
        }
        Acc::Any(first, last) => {
            let b = |v: u32| {
                if v == ANY_LAST {
                    "last".to_string()
                } else {
                    v.to_string()
                }
            };
            out.push_str(".**");
            if *first == 0 && *last == ANY_LAST {
            } else if first == last {
                out.push_str(&format!("{{{}}}", b(*first)));
            } else {
                out.push_str(&format!("{{{} to {}}}", b(*first), b(*last)));
            }
        }
        Acc::Filter(p) => {
            out.push_str("?(");
            print(out, p, false, false);
            out.push(')');
        }
        Acc::Method(m) => {
            out.push('.');
            out.push_str(m.name());
            out.push('(');
            match m {
                Method::Datetime(Some(t)) => write_json_string(out, t),
                Method::Decimal(p, s) => {
                    if let Some(p) = p {
                        out.push_str(&p.to_string());
                    }
                    if let Some(s) = s {
                        out.push(',');
                        out.push_str(&s.to_string());
                    }
                }
                Method::Time(Some(p))
                | Method::TimeTz(Some(p))
                | Method::Timestamp(Some(p))
                | Method::TimestampTz(Some(p)) => out.push_str(&p.to_string()),
                _ => {}
            }
            out.push(')');
        }
    }
}

impl std::fmt::Display for JsonPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = String::new();
        if self.strict {
            s.push_str("strict ");
        }
        print(&mut s, &self.expr, true, false);
        f.write_str(&s)
    }
}

/// jsonpath input: the canonical text.
pub fn normalize(s: &str) -> Result<String> {
    parse(s).map(|p| p.to_string())
}

// ── evaluation ──

/// A date/time item produced by `.datetime()` & co: the value, its type, and for types with a
/// time zone the UTC offset it was written with (seconds east).
#[derive(Clone, Debug)]
pub struct DateTime {
    pub kind: DtKind,
    /// Days (date), microseconds of day (time), or microseconds since 2000-01-01 (timestamps;
    /// UTC for timestamptz).
    pub value: i64,
    pub offset: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DtKind {
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
}

impl DtKind {
    fn type_name(self) -> &'static str {
        match self {
            DtKind::Date => "date",
            DtKind::Time => "time without time zone",
            DtKind::TimeTz => "time with time zone",
            DtKind::Timestamp => "timestamp without time zone",
            DtKind::TimestampTz => "timestamp with time zone",
        }
    }
}

#[derive(Clone, Debug)]
enum It<'a> {
    J(Cow<'a, Jsonb>),
    Dt(DateTime),
}

impl<'a> It<'a> {
    fn json(&self) -> Option<&Jsonb> {
        match self {
            It::J(j) => Some(j),
            It::Dt(_) => None,
        }
    }

    fn owned(j: Jsonb) -> It<'a> {
        It::J(Cow::Owned(j))
    }

    fn into_jsonb(self) -> Jsonb {
        match self {
            It::J(j) => j.into_owned(),
            It::Dt(d) => Jsonb::String(format_datetime(&d)),
        }
    }
}

/// An evaluation failure: SQL/JSON errors are suppressed by `silent`; others always raise.
enum Fail {
    Soft(Error),
    Hard(Error),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Hard(e)
    }
}

type R<T> = std::result::Result<T, Fail>;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tri {
    True,
    False,
    Unknown,
}

fn soft(c: &'static str, msg: impl Into<String>) -> Fail {
    Fail::Soft(Error::new(c, msg))
}

/// Evaluation context.
pub struct Exec<'a> {
    root: &'a Jsonb,
    vars: Option<&'a Jsonb>,
    /// Date/time variables (SQL/JSON PASSING values of date/time types), looked up before
    /// `vars`.
    dt_vars: &'a [(String, DateTime)],
    strict: bool,
    /// The `_tz` function variants: date/time conversions may use the session time zone.
    pub use_tz: bool,
    pub tz: super::datetime::TimeZone,
    /// Where warnings (precision reductions) go.
    notice: &'a dyn Fn(Error),
}

/// Results of running `path` over `target`: the items, or the error. `silent` turns SQL/JSON
/// errors into an empty result (`Ok(None)`).
pub fn query(
    target: &Jsonb,
    path: &JsonPath,
    vars: Option<&Jsonb>,
    silent: bool,
    use_tz: bool,
    tz: &super::datetime::TimeZone,
) -> Result<Option<Vec<Jsonb>>> {
    query_with(target, path, vars, silent, use_tz, tz, &|_| {})
}

/// `query`, reporting warnings through `notice`.
pub fn query_with(
    target: &Jsonb,
    path: &JsonPath,
    vars: Option<&Jsonb>,
    silent: bool,
    use_tz: bool,
    tz: &super::datetime::TimeZone,
    notice: &dyn Fn(Error),
) -> Result<Option<Vec<Jsonb>>> {
    if let Some(v) = vars
        && !matches!(v, Jsonb::Object(_))
    {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "\"vars\" argument is not an object",
        )
        .detail("Jsonpath parameters should be encoded as key-value pairs of \"vars\" object."));
    }
    let cx = Exec {
        root: target,
        vars,
        dt_vars: &[],
        strict: path.strict,
        use_tz,
        tz: tz.clone(),
        notice,
    };
    match cx.eval(&path.expr, None, None) {
        Ok(items) => Ok(Some(items.into_iter().map(It::into_jsonb).collect())),
        Err(Fail::Soft(_)) if silent => Ok(None),
        Err(Fail::Soft(e)) | Err(Fail::Hard(e)) => Err(e),
    }
}

/// `query_with` as jsonb_path_query & co run it: the items are found one by one (an
/// accessor chain depth first), so a silenced error keeps those found before it.
pub fn query_partial_with(
    target: &Jsonb,
    path: &JsonPath,
    vars: Option<&Jsonb>,
    silent: bool,
    use_tz: bool,
    tz: &super::datetime::TimeZone,
    notice: &dyn Fn(Error),
) -> Result<Vec<Jsonb>> {
    if !silent {
        return Ok(query_with(target, path, vars, silent, use_tz, tz, notice)?.unwrap_or_default());
    }
    if let Some(v) = vars
        && !matches!(v, Jsonb::Object(_))
    {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "\"vars\" argument is not an object",
        )
        .detail("Jsonpath parameters should be encoded as key-value pairs of \"vars\" object."));
    }
    let cx = Exec {
        root: target,
        vars,
        dt_vars: &[],
        strict: path.strict,
        use_tz,
        tz: tz.clone(),
        notice,
    };
    let mut out = Vec::new();
    match cx.eval_top(&path.expr, &mut out) {
        Ok(()) | Err(Fail::Soft(_)) => Ok(out.into_iter().map(It::into_jsonb).collect()),
        Err(Fail::Hard(e)) => Err(e),
    }
}

/// What kind of date/time value a path item is, for `is_mutable` (JsonPathDatatypeStatus).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DtStatus {
    /// null, bool, numeric, string, array, object.
    Non,
    /// A date/time value of unknown type (`.datetime()` without a template).
    Unknown,
    /// time with time zone, timestamp with time zone.
    Zoned,
    /// time, timestamp, date.
    NonZoned,
}

/// Whether a datetime template reads a time zone (datetime_format_has_tz): TZH, TZM, TZ or
/// OF outside quoted text.
fn format_has_tz(t: &str) -> bool {
    let up = t.to_ascii_uppercase();
    let mut quoted = false;
    let b = up.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            quoted = !quoted;
        } else if !quoted && (up[i..].starts_with("TZ") || up[i..].starts_with("OF")) {
            return true;
        }
        i += 1;
    }
    false
}

/// jspIsMutable: whether evaluating `path` depends on the session time zone. `var`: a
/// PASSING variable's kind (None: not passed).
pub fn is_mutable(path: &JsonPath, var: &dyn Fn(&str) -> Option<DtStatus>) -> bool {
    struct W<'a> {
        var: &'a dyn Fn(&str) -> Option<DtStatus>,
        lax: bool,
        mutable: bool,
    }
    impl W<'_> {
        fn node(&mut self, n: &Node, current: DtStatus) -> DtStatus {
            match n {
                Node::Current => current,
                Node::Var(v) => (self.var)(v).unwrap_or(DtStatus::Non),
                Node::Chain(base, accs) => {
                    let mut st = self.node(base, current);
                    for a in accs {
                        if self.mutable {
                            break;
                        }
                        st = match a {
                            Acc::Key(_) | Acc::AnyKey => DtStatus::Non,
                            Acc::AnyArray if !self.lax => DtStatus::Non,
                            Acc::AnyArray => st,
                            Acc::Index(subs) => {
                                for (from, to) in subs {
                                    if let Some(to) = to {
                                        self.node(to, current);
                                    }
                                    self.node(from, current);
                                }
                                if self.lax { st } else { DtStatus::Non }
                            }
                            Acc::Any(first, _) if *first > 0 => DtStatus::Non,
                            Acc::Any(..) => st,
                            Acc::Filter(p) => {
                                self.node(p, st);
                                st
                            }
                            Acc::Method(m) => match m {
                                Method::Datetime(Some(t)) => {
                                    if format_has_tz(t) {
                                        DtStatus::Zoned
                                    } else {
                                        DtStatus::NonZoned
                                    }
                                }
                                Method::Datetime(None) => DtStatus::Unknown,
                                Method::Time(_)
                                | Method::Date
                                | Method::Timestamp(_)
                                | Method::TimeTz(_)
                                | Method::TimestampTz(_) => {
                                    self.mutable = true;
                                    DtStatus::NonZoned
                                }
                                _ => DtStatus::Non,
                            },
                        };
                    }
                    st
                }
                Node::Cmp(_, l, r) => {
                    let ls = self.node(l, current);
                    let rs = self.node(r, current);
                    if ls != DtStatus::Non
                        && rs != DtStatus::Non
                        && (ls == DtStatus::Unknown || rs == DtStatus::Unknown || ls != rs)
                    {
                        self.mutable = true;
                    }
                    DtStatus::Non
                }
                Node::Not(a) | Node::IsUnknown(a) | Node::Exists(a) | Node::Unary(_, a) => {
                    self.node(a, current);
                    DtStatus::Non
                }
                Node::And(l, r)
                | Node::Or(l, r)
                | Node::Bin(_, l, r)
                | Node::StartsWith(l, r) => {
                    self.node(l, current);
                    self.node(r, current);
                    DtStatus::Non
                }
                Node::LikeRegex(e, ..) => {
                    self.node(e, current);
                    DtStatus::Non
                }
                _ => DtStatus::Non,
            }
        }
    }
    let mut w = W {
        var,
        lax: !path.strict,
        mutable: false,
    };
    w.node(&path.expr, DtStatus::Non);
    w.mutable
}

/// A result item of `query_items`: a JSON value, or a date/time value.
#[derive(Clone, Debug)]
pub enum Item {
    Json(Jsonb),
    Datetime(DateTime),
}

impl Item {
    /// As jsonb (a date/time value becomes its ISO text).
    pub fn into_jsonb(self) -> Jsonb {
        match self {
            Item::Json(j) => j,
            Item::Datetime(d) => Jsonb::String(format_datetime(&d)),
        }
    }
}

/// `query` keeping date/time items as they are (the SQL/JSON query functions output them
/// with their types' output functions), with date/time PASSING variables `dt_vars`.
#[allow(clippy::too_many_arguments)]
pub fn query_items(
    target: &Jsonb,
    path: &JsonPath,
    vars: Option<&Jsonb>,
    dt_vars: &[(String, DateTime)],
    silent: bool,
    use_tz: bool,
    tz: &super::datetime::TimeZone,
) -> Result<Option<Vec<Item>>> {
    let cx = Exec {
        root: target,
        vars,
        dt_vars,
        strict: path.strict,
        use_tz,
        tz: tz.clone(),
        notice: &|_| {},
    };
    match cx.eval(&path.expr, None, None) {
        Ok(items) => Ok(Some(
            items
                .into_iter()
                .map(|i| match i {
                    It::J(j) => Item::Json(j.into_owned()),
                    It::Dt(d) => Item::Datetime(d),
                })
                .collect(),
        )),
        Err(Fail::Soft(_)) if silent => Ok(None),
        Err(Fail::Soft(e)) | Err(Fail::Hard(e)) => Err(e),
    }
}

/// `jsonb_path_exists`: `None` for a (silenced) error.
pub fn exists(
    target: &Jsonb,
    path: &JsonPath,
    vars: Option<&Jsonb>,
    silent: bool,
    use_tz: bool,
    tz: &super::datetime::TimeZone,
) -> Result<Option<bool>> {
    exists_with(target, path, vars, silent, use_tz, tz, &|_| {})
}

pub fn exists_with(
    target: &Jsonb,
    path: &JsonPath,
    vars: Option<&Jsonb>,
    silent: bool,
    use_tz: bool,
    tz: &super::datetime::TimeZone,
    notice: &dyn Fn(Error),
) -> Result<Option<bool>> {
    // A unary +/- at the top: true at its first numeric operand, the others skipped
    // (executeUnaryArithmExpr without a result list; strict mode evaluates everything, to
    // find any error).
    if !path.strict
        && let Node::Unary(_, arg) = &path.expr
    {
        let cx = Exec {
            root: target,
            vars,
            dt_vars: &[],
            strict: path.strict,
            use_tz,
            tz: tz.clone(),
            notice,
        };
        let r = cx
            .eval(arg, None, None)
            .map(|items| cx.unwrap(items))
            .map(|items| {
                items
                    .iter()
                    .any(|it| matches!(it.json(), Some(Jsonb::Number(_))))
            });
        return match r {
            Ok(b) => Ok(Some(b)),
            Err(Fail::Soft(_)) if silent => Ok(None),
            Err(Fail::Soft(e)) | Err(Fail::Hard(e)) => Err(e),
        };
    }
    Ok(query_with(target, path, vars, silent, use_tz, tz, notice)?.map(|r| !r.is_empty()))
}

/// `jsonb_path_match`: the single boolean result (`None` for null / silenced errors).
pub fn matches(
    target: &Jsonb,
    path: &JsonPath,
    vars: Option<&Jsonb>,
    silent: bool,
    use_tz: bool,
    tz: &super::datetime::TimeZone,
) -> Result<Option<bool>> {
    matches_with(target, path, vars, silent, use_tz, tz, &|_| {})
}

pub fn matches_with(
    target: &Jsonb,
    path: &JsonPath,
    vars: Option<&Jsonb>,
    silent: bool,
    use_tz: bool,
    tz: &super::datetime::TimeZone,
    notice: &dyn Fn(Error),
) -> Result<Option<bool>> {
    let Some(r) = query_with(target, path, vars, silent, use_tz, tz, notice)? else {
        return Ok(None);
    };
    match r.as_slice() {
        [Jsonb::Bool(b)] => Ok(Some(*b)),
        [Jsonb::Null] => Ok(None),
        _ if silent => Ok(None),
        _ => Err(Error::new(
            code::SINGLETON_SQL_JSON_ITEM_REQUIRED,
            "single boolean result is expected",
        )),
    }
}

fn type_name(it: &It) -> &'static str {
    match it {
        It::Dt(d) => d.kind.type_name(),
        It::J(j) => match &**j {
            Jsonb::Null => "null",
            Jsonb::Bool(_) => "boolean",
            Jsonb::Number(_) => "number",
            Jsonb::String(_) => "string",
            Jsonb::Array(_) => "array",
            Jsonb::Object(_) => "object",
        },
    }
}

impl<'a> Exec<'a> {
    /// Lax mode unwraps arrays one level for accessors that expect non-arrays.
    fn unwrap(&self, items: Vec<It<'a>>) -> Vec<It<'a>> {
        if self.strict {
            return items;
        }
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            match it {
                It::J(Cow::Borrowed(Jsonb::Array(a))) => {
                    out.extend(a.iter().map(|x| It::J(Cow::Borrowed(x))))
                }
                It::J(Cow::Owned(Jsonb::Array(a))) => out.extend(a.into_iter().map(It::owned)),
                it => out.push(it),
            }
        }
        out
    }

    /// The root expression, its items appended to `out` as they are found.
    fn eval_top(&self, n: &Node, out: &mut Vec<It<'a>>) -> R<()> {
        match n {
            Node::Chain(base, accs) => {
                for it in self.eval(base, None, None)? {
                    self.chain_dfs(it, accs, out)?;
                }
                Ok(())
            }
            Node::Unary(minus, a) => {
                for it in self.unwrap(self.eval(a, None, None)?) {
                    match it.json() {
                        Some(Jsonb::Number(v)) => out.push(It::owned(Jsonb::Number(if *minus {
                            v.neg()
                        } else {
                            v.clone()
                        }))),
                        _ => {
                            return Err(soft(
                                code::SQL_JSON_NUMBER_NOT_FOUND,
                                format!(
                                    "operand of unary jsonpath operator {} is not a numeric value",
                                    if *minus { "-" } else { "+" }
                                ),
                            ));
                        }
                    }
                }
                Ok(())
            }
            n => {
                out.extend(self.eval(n, None, None)?);
                Ok(())
            }
        }
    }

    fn chain_dfs(&self, it: It<'a>, accs: &[Acc], out: &mut Vec<It<'a>>) -> R<()> {
        let Some((a, rest)) = accs.split_first() else {
            out.push(it);
            return Ok(());
        };
        let mut next = Vec::new();
        self.access(a, it, &mut next, None)?;
        for x in next {
            self.chain_dfs(x, rest, out)?;
        }
        Ok(())
    }

    fn eval(&self, n: &Node, cur: Option<&It<'a>>, last: Option<i64>) -> R<Vec<It<'a>>> {
        Ok(match n {
            Node::Null => vec![It::owned(Jsonb::Null)],
            Node::Bool(b) => vec![It::owned(Jsonb::Bool(*b))],
            Node::Num(v) => vec![It::owned(Jsonb::Number(v.clone()))],
            Node::Str(s) => vec![It::owned(Jsonb::String(s.clone()))],
            Node::Root => vec![It::J(Cow::Borrowed(self.root))],
            Node::Current => vec![
                cur.cloned()
                    .ok_or_else(|| Fail::Hard(crate::error::internal("@ outside a filter")))?,
            ],
            Node::Var(v) if self.dt_vars.iter().any(|d| d.0 == *v) => {
                let d = self.dt_vars.iter().find(|d| d.0 == *v).map(|d| d.1.clone());
                d.map(It::Dt).into_iter().collect()
            }
            Node::Var(v) => {
                let found = self.vars.and_then(|vars| vars.get(v));
                match found {
                    Some(j) => vec![It::J(Cow::Borrowed(j))],
                    None => {
                        return Err(Fail::Hard(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("could not find jsonpath variable \"{v}\""),
                        )));
                    }
                }
            }
            Node::Last => {
                let l = last.ok_or_else(|| {
                    Fail::Hard(Error::new(
                        code::SYNTAX_ERROR,
                        "evaluating jsonpath LAST outside of array subscript",
                    ))
                })?;
                vec![It::owned(Jsonb::Number(Numeric::from_i64(l)))]
            }
            Node::Chain(base, accs) => {
                let mut items = self.eval(base, cur, last)?;
                for a in accs {
                    let mut next = Vec::new();
                    for it in items {
                        self.access(a, it, &mut next, last)?;
                    }
                    items = next;
                }
                items
            }
            Node::Bin(op, l, r) => {
                let name = match op {
                    BinOp::Add => "+",
                    BinOp::Sub => "-",
                    BinOp::Mul => "*",
                    BinOp::Div => "/",
                    BinOp::Mod => "%",
                };
                let single = |side: &str, n: &Node| -> R<Numeric> {
                    let items = self.unwrap(self.eval(n, cur, last)?);
                    match items.as_slice() {
                        [It::J(j)] if matches!(&**j, Jsonb::Number(_)) => {
                            let Jsonb::Number(v) = &**j else {
                                unreachable!()
                            };
                            Ok(v.clone())
                        }
                        _ => Err(soft(
                            code::SINGLETON_SQL_JSON_ITEM_REQUIRED,
                            format!(
                                "{side} operand of jsonpath operator {name} is not a single numeric value"
                            ),
                        )),
                    }
                };
                let a = single("left", l)?;
                let b = single("right", r)?;
                let v = match op {
                    BinOp::Add => a.add(&b),
                    BinOp::Sub => a.sub(&b),
                    BinOp::Mul => a.mul(&b),
                    BinOp::Div => a.div(&b).map_err(Fail::Soft)?,
                    BinOp::Mod => a.rem(&b).map_err(Fail::Soft)?,
                };
                vec![It::owned(Jsonb::Number(v))]
            }
            Node::Unary(minus, a) => {
                let items = self.unwrap(self.eval(a, cur, last)?);
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    match it.json() {
                        Some(Jsonb::Number(v)) => out.push(It::owned(Jsonb::Number(if *minus {
                            v.neg()
                        } else {
                            v.clone()
                        }))),
                        _ => {
                            return Err(soft(
                                code::SQL_JSON_NUMBER_NOT_FOUND,
                                format!(
                                    "operand of unary jsonpath operator {} is not a numeric value",
                                    if *minus { "-" } else { "+" }
                                ),
                            ));
                        }
                    }
                }
                out
            }
            // A predicate in a value position: its boolean (null for unknown).
            p => {
                let t = self.pred(p, cur, last)?;
                vec![It::owned(match t {
                    Tri::True => Jsonb::Bool(true),
                    Tri::False => Jsonb::Bool(false),
                    Tri::Unknown => Jsonb::Null,
                })]
            }
        })
    }

    /// One accessor over an item; `last`: the innermost array subscript's `last` (filters
    /// inside a subscript see it).
    fn access(&self, a: &Acc, it: It<'a>, out: &mut Vec<It<'a>>, last: Option<i64>) -> R<()> {
        // Lax mode applies member accessors, filters and most methods to array elements.
        let unwraps = match a {
            Acc::Key(_) | Acc::AnyKey | Acc::Filter(_) => true,
            Acc::Method(m) => !matches!(m, Method::Type | Method::Size),
            _ => false,
        };
        if unwraps && !self.strict && matches!(it.json(), Some(Jsonb::Array(_))) {
            for x in self.unwrap(vec![it]) {
                // One level only: nested arrays are structural mismatches (ignored in lax mode).
                if matches!(x.json(), Some(Jsonb::Array(_))) && !matches!(a, Acc::Filter(_)) {
                    if let Acc::Method(m) = a {
                        self.method(m, x, out)?;
                    }
                    continue;
                }
                self.access_one(a, x, out, last)?;
            }
            return Ok(());
        }
        self.access_one(a, it, out, last)
    }

    fn access_one(
        &self,
        a: &Acc,
        it: It<'a>,
        out: &mut Vec<It<'a>>,
        last: Option<i64>,
    ) -> R<()> {
        match a {
            Acc::Key(k) => match it {
                It::J(Cow::Borrowed(j @ Jsonb::Object(_))) => match j.get(k) {
                    Some(v) => out.push(It::J(Cow::Borrowed(v))),
                    None if self.strict => {
                        return Err(soft(
                            code::SQL_JSON_MEMBER_NOT_FOUND,
                            format!("JSON object does not contain key \"{k}\""),
                        ));
                    }
                    None => {}
                },
                It::J(Cow::Owned(j @ Jsonb::Object(_))) => match j.get(k) {
                    Some(v) => out.push(It::owned(v.clone())),
                    None if self.strict => {
                        return Err(soft(
                            code::SQL_JSON_MEMBER_NOT_FOUND,
                            format!("JSON object does not contain key \"{k}\""),
                        ));
                    }
                    None => {}
                },
                _ if self.strict => {
                    return Err(soft(
                        code::SQL_JSON_MEMBER_NOT_FOUND,
                        "jsonpath member accessor can only be applied to an object",
                    ));
                }
                _ => {}
            },
            Acc::AnyKey => match it {
                It::J(Cow::Borrowed(Jsonb::Object(pairs))) => {
                    out.extend(pairs.iter().map(|(_, v)| It::J(Cow::Borrowed(v))))
                }
                It::J(Cow::Owned(Jsonb::Object(pairs))) => {
                    out.extend(pairs.into_iter().map(|(_, v)| It::owned(v)))
                }
                _ if self.strict => {
                    return Err(soft(
                        code::SQL_JSON_OBJECT_NOT_FOUND,
                        "jsonpath wildcard member accessor can only be applied to an object",
                    ));
                }
                _ => {}
            },
            Acc::AnyArray => match it {
                It::J(Cow::Borrowed(Jsonb::Array(a))) => {
                    out.extend(a.iter().map(|x| It::J(Cow::Borrowed(x))))
                }
                It::J(Cow::Owned(Jsonb::Array(a))) => out.extend(a.into_iter().map(It::owned)),
                _ if self.strict => {
                    return Err(soft(
                        code::SQL_JSON_ARRAY_NOT_FOUND,
                        "jsonpath wildcard array accessor can only be applied to an array",
                    ));
                }
                it => out.push(it),
            },
            Acc::Index(subs) => {
                let elems: Vec<It<'a>> = match it {
                    It::J(Cow::Borrowed(Jsonb::Array(a))) => {
                        a.iter().map(|x| It::J(Cow::Borrowed(x))).collect()
                    }
                    It::J(Cow::Owned(Jsonb::Array(a))) => a.into_iter().map(It::owned).collect(),
                    _ if self.strict => {
                        return Err(soft(
                            code::SQL_JSON_ARRAY_NOT_FOUND,
                            "jsonpath array accessor can only be applied to an array",
                        ));
                    }
                    // Lax mode treats a non-array as a one-element array.
                    it => vec![it],
                };
                let size = elems.len() as i64;
                for (from, to) in subs {
                    let idx = |n: &Node| -> R<i64> {
                        let items = self.unwrap(self.eval(
                            n,
                            Some(&It::owned(Jsonb::Null)),
                            Some(size - 1),
                        )?);
                        match items.as_slice() {
                            [It::J(j)] if matches!(&**j, Jsonb::Number(_)) => {
                                let Jsonb::Number(v) = &**j else {
                                    unreachable!()
                                };
                                v.trunc(0)
                                    .to_i64_round()
                                    .filter(|v| i32::try_from(*v).is_ok())
                                    .ok_or_else(|| {
                                        soft(
                                            code::INVALID_SQL_JSON_SUBSCRIPT,
                                            "jsonpath array subscript is out of integer range",
                                        )
                                    })
                            }
                            _ => Err(soft(
                                code::INVALID_SQL_JSON_SUBSCRIPT,
                                "jsonpath array subscript is not a single numeric value",
                            )),
                        }
                    };
                    let lo = idx(from)?;
                    let hi = match to {
                        Some(t) => idx(t)?,
                        None => lo,
                    };
                    if self.strict && (lo < 0 || lo > hi || hi >= size) {
                        return Err(soft(
                            code::INVALID_SQL_JSON_SUBSCRIPT,
                            "jsonpath array subscript is out of bounds",
                        ));
                    }
                    let (lo, hi) = (lo.max(0), hi.min(size - 1));
                    let mut i = lo;
                    while i <= hi {
                        out.push(elems[i as usize].clone());
                        i += 1;
                    }
                }
            }
            Acc::Any(first, last) => {
                if *first == 0 {
                    out.push(it.clone());
                }
                if let It::J(j) = &it {
                    self.descend(j, 1, *first, *last, out);
                }
            }
            Acc::Filter(p) => {
                if self.pred(p, Some(&it), last)? == Tri::True {
                    out.push(it);
                }
            }
            Acc::Method(m) => self.method(m, it, out)?,
        }
        Ok(())
    }

    /// `.**`: containers' members, depth first, at levels first..=last. The `Cow` matters:
    /// borrowed members stay borrowed (keyvalue ids need their addresses).
    #[allow(clippy::ptr_arg)]
    fn descend(
        &self,
        j: &Cow<'a, Jsonb>,
        level: u32,
        first: u32,
        last: u32,
        out: &mut Vec<It<'a>>,
    ) {
        if level > last && last != ANY_LAST {
            return;
        }
        let children: Vec<Cow<'a, Jsonb>> = match j {
            Cow::Borrowed(Jsonb::Array(a)) => a.iter().map(Cow::Borrowed).collect(),
            Cow::Borrowed(Jsonb::Object(p)) => p.iter().map(|(_, v)| Cow::Borrowed(v)).collect(),
            Cow::Owned(Jsonb::Array(a)) => a.iter().cloned().map(Cow::Owned).collect(),
            Cow::Owned(Jsonb::Object(p)) => p.iter().map(|(_, v)| Cow::Owned(v.clone())).collect(),
            _ => return,
        };
        // `.**{last}`: the leaves only (executeAnyItem).
        let leaves_only = first == ANY_LAST && last == ANY_LAST;
        for c in children {
            let container = matches!(&*c, Jsonb::Array(_) | Jsonb::Object(_));
            if level >= first || (leaves_only && !container) {
                out.push(It::J(c.clone()));
            }
            if level < last && matches!(&*c, Jsonb::Array(_) | Jsonb::Object(_)) {
                self.descend(&c, level + 1, first, last, out);
            }
        }
    }

    fn method(&self, m: &Method, it: It<'a>, out: &mut Vec<It<'a>>) -> R<()> {
        let name = m.name();
        let num_only = |what: &str| {
            soft(
                code::NON_NUMERIC_SQL_JSON_ITEM,
                format!("jsonpath item method .{name}() can only be applied to {what}"),
            )
        };
        match m {
            Method::Type => out.push(It::owned(Jsonb::String(type_name(&it).into()))),
            Method::Size => match it.json() {
                Some(Jsonb::Array(a)) => {
                    out.push(It::owned(Jsonb::Number(Numeric::from_i64(a.len() as i64))))
                }
                _ if self.strict => {
                    return Err(soft(
                        code::SQL_JSON_ARRAY_NOT_FOUND,
                        "jsonpath item method .size() can only be applied to an array",
                    ));
                }
                _ => out.push(It::owned(Jsonb::Number(Numeric::from_i64(1)))),
            },
            Method::Abs | Method::Floor | Method::Ceiling => match it.json() {
                Some(Jsonb::Number(v)) => out.push(It::owned(Jsonb::Number(match m {
                    Method::Abs => v.abs(),
                    Method::Floor => v.floor(),
                    _ => v.ceil(),
                }))),
                _ => return Err(num_only("a numeric value")),
            },
            Method::Double => {
                let f = match it.json() {
                    // numeric_out, then float8in.
                    Some(Jsonb::Number(v)) => {
                        let tmp = v.to_string();
                        super::text::parse_float8(&tmp).map_err(|_| {
                            soft(code::NON_NUMERIC_SQL_JSON_ITEM, format!("argument \"{tmp}\" of jsonpath item method .double() is invalid for type double precision"))
                        })?
                    }
                    Some(Jsonb::String(s)) => super::text::parse_float8(s).map_err(|_| {
                        soft(code::NON_NUMERIC_SQL_JSON_ITEM, format!("argument \"{s}\" of jsonpath item method .double() is invalid for type double precision"))
                    })?,
                    _ => return Err(num_only("a string or numeric value")),
                };
                if !f.is_finite() {
                    return Err(soft(
                        code::NON_NUMERIC_SQL_JSON_ITEM,
                        "NaN or Infinity is not allowed for jsonpath item method .double()",
                    ));
                }
                let v = Numeric::parse_pg(&super::text::format_float8(f, 1)).map_err(Fail::Soft)?;
                out.push(It::owned(Jsonb::Number(v)));
            }
            Method::Number | Method::Decimal(..) => {
                let (v, numstr) = match it.json() {
                    Some(Jsonb::Number(v)) => (v.clone(), v.to_string()),
                    Some(Jsonb::String(s)) => (
                        Numeric::parse_pg(s).map_err(|_| {
                            soft(code::NON_NUMERIC_SQL_JSON_ITEM, format!("argument \"{s}\" of jsonpath item method .{name}() is invalid for type numeric"))
                        })?,
                        s.clone(),
                    ),
                    _ => return Err(num_only("a string or numeric value")),
                };
                if !v.is_finite() {
                    return Err(soft(
                        code::NON_NUMERIC_SQL_JSON_ITEM,
                        format!(
                            "NaN or Infinity is not allowed for jsonpath item method .{name}()"
                        ),
                    ));
                }
                let v = match m {
                    Method::Decimal(Some(p), s) => {
                        let int4 = |n: &Numeric, what: &str| {
                            n.to_i64_round()
                                .and_then(|v| i32::try_from(v).ok())
                                .ok_or_else(|| {
                                    soft(
                                        code::NON_NUMERIC_SQL_JSON_ITEM,
                                        format!("{what} of jsonpath item method .decimal() is out of range for type integer"),
                                    )
                                })
                        };
                        let p = int4(p, "precision")?;
                        let s = match s {
                            Some(s) => int4(s, "scale")?,
                            None => 0,
                        };
                        // numerictypmodin.
                        if !(1..=1000).contains(&p) {
                            return Err(soft(
                                code::INVALID_PARAMETER_VALUE,
                                format!("NUMERIC precision {p} must be between 1 and 1000"),
                            ));
                        }
                        if !(-1000..=1000).contains(&s) {
                            return Err(soft(
                                code::INVALID_PARAMETER_VALUE,
                                format!("NUMERIC scale {s} must be between -1000 and 1000"),
                            ));
                        }
                        let typmod = ((p << 16) | (s & 0x7ff)) + 4;
                        v.apply_typmod(typmod).map_err(|_| {
                            soft(code::NON_NUMERIC_SQL_JSON_ITEM, format!("argument \"{numstr}\" of jsonpath item method .decimal() is invalid for type numeric"))
                        })?
                    }
                    _ => v,
                };
                out.push(It::owned(Jsonb::Number(v)));
            }
            Method::Integer | Method::Bigint => {
                let (lo, hi, ty) = if matches!(m, Method::Integer) {
                    (i32::MIN as i64, i32::MAX as i64, "integer")
                } else {
                    (i64::MIN, i64::MAX, "bigint")
                };
                let bad = |s: &str| {
                    soft(
                        code::NON_NUMERIC_SQL_JSON_ITEM,
                        format!(
                            "argument \"{s}\" of jsonpath item method .{name}() is invalid for type {ty}"
                        ),
                    )
                };
                let v = match it.json() {
                    Some(Jsonb::Number(v)) => v
                        .round(0)
                        .to_i64_round()
                        .filter(|x| (lo..=hi).contains(x))
                        .ok_or_else(|| bad(&v.to_string()))?,
                    Some(Jsonb::String(s)) => s
                        .trim()
                        .parse::<i64>()
                        .ok()
                        .filter(|x| (lo..=hi).contains(x))
                        .ok_or_else(|| bad(s))?,
                    _ => return Err(num_only("a string or numeric value")),
                };
                out.push(It::owned(Jsonb::Number(Numeric::from_i64(v))));
            }
            Method::Boolean => {
                let b = match it.json() {
                    Some(Jsonb::Bool(b)) => *b,
                    Some(Jsonb::Number(v)) => {
                        let i = v.to_i64_round().filter(|_| v.trunc(0) == *v).ok_or_else(|| {
                            soft(code::NON_NUMERIC_SQL_JSON_ITEM, format!("argument \"{v}\" of jsonpath item method .boolean() is invalid for type boolean"))
                        })?;
                        i != 0
                    }
                    Some(Jsonb::String(s)) => super::text::parse_bool(s).map_err(|_| {
                        soft(code::NON_NUMERIC_SQL_JSON_ITEM, format!("argument \"{s}\" of jsonpath item method .boolean() is invalid for type boolean"))
                    })?,
                    _ => return Err(soft(code::NON_NUMERIC_SQL_JSON_ITEM, "jsonpath item method .boolean() can only be applied to a boolean, string, or numeric value")),
                };
                out.push(It::owned(Jsonb::Bool(b)));
            }
            Method::String => {
                let s = match &it {
                    It::Dt(d) => format_datetime(d),
                    It::J(j) => match &**j {
                        Jsonb::String(s) => s.clone(),
                        Jsonb::Number(v) => v.to_string(),
                        Jsonb::Bool(b) => b.to_string(),
                        _ => {
                            return Err(soft(
                                code::NON_NUMERIC_SQL_JSON_ITEM,
                                "jsonpath item method .string() can only be applied to a boolean, string, numeric, or datetime value",
                            ));
                        }
                    },
                };
                out.push(It::owned(Jsonb::String(s)));
            }
            Method::KeyValue => {
                let it_ptr = it.json().map(|j| j as *const Jsonb);
                let Some(Jsonb::Object(pairs)) = it.json() else {
                    return Err(soft(
                        code::SQL_JSON_OBJECT_NOT_FOUND,
                        "jsonpath item method .keyvalue() can only be applied to an object",
                    ));
                };
                let id = match (&it, it_ptr) {
                    (It::J(Cow::Borrowed(_)), Some(p)) => {
                        container_offset(self.root, p).unwrap_or(0) as i64
                    }
                    _ => 0,
                };
                for (k, v) in pairs {
                    out.push(It::owned(Jsonb::Object(vec![
                        ("id".into(), Jsonb::Number(Numeric::from_i64(id))),
                        ("key".into(), Jsonb::String(k.clone())),
                        ("value".into(), v.clone()),
                    ])));
                }
            }
            Method::Datetime(_)
            | Method::Date
            | Method::Time(_)
            | Method::TimeTz(_)
            | Method::Timestamp(_)
            | Method::TimestampTz(_) => {
                out.push(It::Dt(self.datetime_method(m, &it)?));
            }
        }
        Ok(())
    }

    fn dt_ctx(&self) -> super::datetime::DtCtx<'_> {
        super::datetime::DtCtx {
            tz: &self.tz,
            now: 0,
            order: super::datetime::DateOrder::Mdy,
            interval_style: super::text::IntervalStyle::Postgres,
            abbrevs: "Default",
        }
    }

    /// checkTimezoneIsUsedForCast.
    fn need_tz(&self, from: &str, to: &str) -> R<()> {
        if self.use_tz {
            return Ok(());
        }
        Err(Fail::Hard(
            Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("cannot convert value from {from} to {to} without time zone usage"),
            )
            .hint("Use *_tz() function for time zone support."),
        ))
    }

    /// anytime_typmod_check / anytimestamp_typmod_check for a (non-negative) precision.
    fn typmod_check(&self, what: &str, tz: bool, p: i32) -> i32 {
        if p > 6 {
            let mut e = Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "{what}({p}){} precision reduced to maximum allowed, 6",
                    if tz { " WITH TIME ZONE" } else { "" }
                ),
            );
            e.severity = crate::error::Severity::Warning;
            (self.notice)(e);
            return 6;
        }
        p
    }

    /// time_timetz: the session zone's offset (east) on the current date.
    fn time_zone_now(&self, time: i64) -> i32 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0)
            - super::datetime::UNIX_EPOCH_OFFSET_SECS * 1_000_000;
        let today = crate::sql::func::datetime::to_local(now, &self.tz).div_euclid(USECS_PER_DAY);
        self.tz.offset_for_local(today * USECS_PER_DAY + time)
    }

    /// executeDateTimeMethod: `.datetime([template])`, `.date()`, `.time([p])`, `.time_tz([p])`,
    /// `.timestamp([p])` and `.timestamp_tz([p])`.
    fn datetime_method(&self, m: &Method, it: &It<'a>) -> R<DateTime> {
        use crate::sql::func::datetime::{from_local, to_local};
        use crate::sql::func::formatting::{ParsedDatetime as P, parse_datetime};
        use DtKind::*;
        let name = m.name();
        let Some(Jsonb::String(s)) = it.json() else {
            return Err(soft(
                code::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION,
                format!("jsonpath item method .{name}() can only be applied to a string"),
            ));
        };
        let cx = self.dt_ctx();
        let mut prec = None;
        let parsed = if let Method::Datetime(Some(tpl)) = m {
            parse_datetime(s, tpl, true, &cx).map_err(Fail::Soft)?
        } else {
            if let Method::Time(Some(p))
            | Method::TimeTz(Some(p))
            | Method::Timestamp(Some(p))
            | Method::TimestampTz(Some(p)) = m
            {
                prec = Some(
                    p.to_i64_round()
                        .and_then(|v| i32::try_from(v).ok())
                        .ok_or_else(|| {
                            soft(
                                code::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION,
                                format!("time precision of jsonpath item method .{name}() is out of range for type integer"),
                            )
                        })?,
                );
            }
            ISO_DATETIME_FORMATS
                .iter()
                .find_map(|f| parse_datetime(s, f, true, &cx).ok())
                .ok_or_else(|| {
                    if matches!(m, Method::Datetime(_)) {
                        Fail::Soft(
                            Error::new(
                                code::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION,
                                format!("datetime format is not recognized: \"{s}\""),
                            )
                            .hint("Use a datetime template argument to specify the input data format."),
                        )
                    } else {
                        soft(
                            code::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION,
                            format!("{name} format is not recognized: \"{s}\""),
                        )
                    }
                })?
        };
        let dt = |kind, value, offset| DateTime {
            kind,
            value,
            offset,
        };
        let d = match parsed {
            P::Date(v) => dt(Date, v as i64, 0),
            P::Time(t) => dt(Time, t, 0),
            P::TimeTz(t, west) => dt(TimeTz, t, -west),
            P::Timestamp(t) => dt(Timestamp, t, 0),
            P::TimestampTz(t, west) => dt(TimestampTz, t, -west),
        };
        let not_recognized = |what: &str| {
            soft(
                code::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION,
                format!("{what} format is not recognized: \"{s}\""),
            )
        };
        let round = |v: i64, p: i32| super::datetime::round_micros(v, p);
        Ok(match m {
            Method::Datetime(_) => d,
            Method::Date => match d.kind {
                Date => d,
                Time | TimeTz => return Err(not_recognized("date")),
                Timestamp => dt(Date, d.value.div_euclid(USECS_PER_DAY), 0),
                TimestampTz => {
                    self.need_tz("timestamptz", "date")?;
                    dt(Date, to_local(d.value, &self.tz).div_euclid(USECS_PER_DAY), 0)
                }
            },
            Method::Time(_) => {
                let t = match d.kind {
                    Date => return Err(not_recognized("time")),
                    Time => d.value,
                    TimeTz => {
                        self.need_tz("timetz", "time")?;
                        d.value
                    }
                    Timestamp => d.value.rem_euclid(USECS_PER_DAY),
                    TimestampTz => {
                        self.need_tz("timestamptz", "time")?;
                        to_local(d.value, &self.tz).rem_euclid(USECS_PER_DAY)
                    }
                };
                let t = match prec {
                    Some(p) => round(t, self.typmod_check("TIME", false, p)),
                    None => t,
                };
                dt(Time, t, 0)
            }
            Method::TimeTz(_) => {
                let (t, off) = match d.kind {
                    Date | Timestamp => return Err(not_recognized("time_tz")),
                    Time => {
                        self.need_tz("time", "timetz")?;
                        (d.value, self.time_zone_now(d.value))
                    }
                    TimeTz => (d.value, d.offset),
                    TimestampTz => (
                        to_local(d.value, &self.tz).rem_euclid(USECS_PER_DAY),
                        self.tz.offset_at_utc(d.value),
                    ),
                };
                let t = match prec {
                    Some(p) => round(t, self.typmod_check("TIME", true, p)),
                    None => t,
                };
                dt(TimeTz, t, off)
            }
            Method::Timestamp(_) => {
                let t = match d.kind {
                    Date => date_ts(d.value)?,
                    Time | TimeTz => return Err(not_recognized("timestamp")),
                    Timestamp => d.value,
                    TimestampTz => {
                        self.need_tz("timestamptz", "timestamp")?;
                        to_local(d.value, &self.tz)
                    }
                };
                let t = match prec {
                    Some(p) => round(t, self.typmod_check("TIMESTAMP", false, p)),
                    None => t,
                };
                dt(Timestamp, t, 0)
            }
            _ => {
                let (t, off) = match d.kind {
                    Date => {
                        self.need_tz("date", "timestamptz")?;
                        let local = date_ts(d.value)?;
                        (from_local(local, &self.tz), self.tz.offset_for_local(local))
                    }
                    Time | TimeTz => return Err(not_recognized("timestamp_tz")),
                    Timestamp => {
                        self.need_tz("timestamp", "timestamptz")?;
                        (
                            from_local(d.value, &self.tz),
                            self.tz.offset_for_local(d.value),
                        )
                    }
                    TimestampTz => (d.value, d.offset),
                };
                let t = match prec {
                    Some(p) => round(t, self.typmod_check("TIMESTAMP", true, p)),
                    None => t,
                };
                dt(TimestampTz, t, off)
            }
        })
    }

    fn pred(&self, n: &Node, cur: Option<&It<'a>>, last: Option<i64>) -> R<Tri> {
        Ok(match n {
            Node::And(l, r) => {
                let a = self.pred(l, cur, last)?;
                if a == Tri::False {
                    return Ok(Tri::False);
                }
                let b = self.pred(r, cur, last)?;
                if b == Tri::True { a } else { b }
            }
            Node::Or(l, r) => {
                let a = self.pred(l, cur, last)?;
                if a == Tri::True {
                    return Ok(Tri::True);
                }
                let b = self.pred(r, cur, last)?;
                if b == Tri::False { a } else { b }
            }
            Node::Not(a) => match self.pred(a, cur, last)? {
                Tri::True => Tri::False,
                Tri::False => Tri::True,
                Tri::Unknown => Tri::Unknown,
            },
            Node::IsUnknown(a) => {
                if self.pred(a, cur, last)? == Tri::Unknown {
                    Tri::True
                } else {
                    Tri::False
                }
            }
            Node::Exists(a) => match self.eval(a, cur, last) {
                Ok(items) => {
                    if items.is_empty() {
                        Tri::False
                    } else {
                        Tri::True
                    }
                }
                Err(Fail::Soft(_)) => Tri::Unknown,
                Err(e) => return Err(e),
            },
            Node::Cmp(op, l, r) => {
                let Some(ls) = self.operand(l, cur, last)? else {
                    return Ok(Tri::Unknown);
                };
                let Some(rs) = self.operand(r, cur, last)? else {
                    return Ok(Tri::Unknown);
                };
                self.existential(&ls, |a| {
                    let mut res = Vec::with_capacity(rs.len());
                    for b in &rs {
                        res.push(self.compare(*op, a, b)?);
                    }
                    Ok(res)
                })?
            }
            Node::StartsWith(l, r) => {
                let Some(ls) = self.operand(l, cur, last)? else {
                    return Ok(Tri::Unknown);
                };
                let Some(rs) = self.operand(r, cur, last)? else {
                    return Ok(Tri::Unknown);
                };
                let prefix = match rs.as_slice() {
                    [It::J(j)] => match &**j {
                        Jsonb::String(s) => s.clone(),
                        _ => return Ok(Tri::Unknown),
                    },
                    _ => return Ok(Tri::Unknown),
                };
                self.existential(&ls, |a| {
                    Ok(vec![match a.json() {
                        Some(Jsonb::String(s)) => {
                            if s.starts_with(prefix.as_str()) {
                                Tri::True
                            } else {
                                Tri::False
                            }
                        }
                        _ => Tri::Unknown,
                    }])
                })?
            }
            Node::LikeRegex(e, pattern, flags) => {
                let Some(ls) = self.operand(e, cur, last)? else {
                    return Ok(Tri::Unknown);
                };
                let re = like_regex(pattern, flags)?;
                self.existential(&ls, |a| {
                    Ok(vec![match a.json() {
                        Some(Jsonb::String(s)) => {
                            if re.is_match(s).map_err(Fail::Hard)? {
                                Tri::True
                            } else {
                                Tri::False
                            }
                        }
                        _ => Tri::Unknown,
                    }])
                })?
            }
            // A non-predicate where a predicate is expected: true / false / null items.
            e => {
                let items = match self.eval(e, cur, last) {
                    Ok(i) => i,
                    Err(Fail::Soft(_)) => return Ok(Tri::Unknown),
                    Err(e) => return Err(e),
                };
                match items.as_slice() {
                    [It::J(j)] => match &**j {
                        Jsonb::Bool(true) => Tri::True,
                        Jsonb::Bool(false) => Tri::False,
                        _ => Tri::Unknown,
                    },
                    _ => Tri::Unknown,
                }
            }
        })
    }

    /// A comparison operand: its (lax-unwrapped) items, or `None` after an error.
    fn operand(&self, n: &Node, cur: Option<&It<'a>>, last: Option<i64>) -> R<Option<Vec<It<'a>>>> {
        match self.eval(n, cur, last) {
            Ok(items) => Ok(Some(self.unwrap(items))),
            Err(Fail::Soft(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `executePredicate`: true if any combination is true (lax) / no unknowns (strict).
    fn existential(&self, left: &[It<'a>], mut f: impl FnMut(&It<'a>) -> R<Vec<Tri>>) -> R<Tri> {
        let (mut found, mut error) = (false, false);
        for a in left {
            for r in f(a)? {
                match r {
                    Tri::Unknown => {
                        if self.strict {
                            return Ok(Tri::Unknown);
                        }
                        error = true;
                    }
                    Tri::True => {
                        if !self.strict {
                            return Ok(Tri::True);
                        }
                        found = true;
                    }
                    Tri::False => {}
                }
            }
        }
        Ok(if found {
            Tri::True
        } else if error {
            Tri::Unknown
        } else {
            Tri::False
        })
    }

    fn compare(&self, op: CmpOp, a: &It<'a>, b: &It<'a>) -> R<Tri> {
        use std::cmp::Ordering;
        let ord = match (a, b) {
            (It::Dt(x), It::Dt(y)) => match self.compare_dt(x, y)? {
                Some(o) => o,
                None => return Ok(Tri::Unknown),
            },
            (It::J(x), It::J(y)) => match (&**x, &**y) {
                (Jsonb::Null, Jsonb::Null) => Ordering::Equal,
                (Jsonb::Null, _) | (_, Jsonb::Null) => {
                    return Ok(if op == CmpOp::Ne {
                        Tri::True
                    } else {
                        Tri::False
                    });
                }
                (Jsonb::Bool(p), Jsonb::Bool(q)) => p.cmp(q),
                (Jsonb::Number(p), Jsonb::Number(q)) => p.cmp(q),
                (Jsonb::String(p), Jsonb::String(q)) => p.as_bytes().cmp(q.as_bytes()),
                _ => return Ok(Tri::Unknown),
            },
            (It::J(x), _) | (_, It::J(x)) if matches!(&**x, Jsonb::Null) => {
                return Ok(if op == CmpOp::Ne {
                    Tri::True
                } else {
                    Tri::False
                });
            }
            _ => return Ok(Tri::Unknown),
        };
        let r = match op {
            CmpOp::Eq => ord == Ordering::Equal,
            CmpOp::Ne => ord != Ordering::Equal,
            CmpOp::Lt => ord == Ordering::Less,
            CmpOp::Le => ord != Ordering::Greater,
            CmpOp::Gt => ord == Ordering::Greater,
            CmpOp::Ge => ord != Ordering::Less,
        };
        Ok(if r { Tri::True } else { Tri::False })
    }

    /// compareDatetime: None for uncomparable types.
    fn compare_dt(&self, x: &DateTime, y: &DateTime) -> R<Option<std::cmp::Ordering>> {
        use crate::sql::func::datetime::from_local;
        use DtKind::*;
        let tz = &self.tz;
        // timetz_cmp: by the UTC-equivalent time, then by zone.
        let timetz_cmp = |t1: i64, o1: i32, t2: i64, o2: i32| {
            (t1 - o1 as i64 * 1_000_000)
                .cmp(&(t2 - o2 as i64 * 1_000_000))
                .then((-o1).cmp(&-o2))
        };
        Ok(Some(match (x.kind, y.kind) {
            (Date, Date) | (Time, Time) | (Timestamp, Timestamp) | (TimestampTz, TimestampTz) => {
                x.value.cmp(&y.value)
            }
            (Date, Timestamp) => date_ts_no_overflow(x.value).cmp(&y.value),
            (Timestamp, Date) => x.value.cmp(&date_ts_no_overflow(y.value)),
            (Date, TimestampTz) => {
                self.need_tz("date", "timestamptz")?;
                from_local(date_ts_no_overflow(x.value), tz).cmp(&y.value)
            }
            (TimestampTz, Date) => {
                self.need_tz("date", "timestamptz")?;
                x.value.cmp(&from_local(date_ts_no_overflow(y.value), tz))
            }
            (Timestamp, TimestampTz) => {
                self.need_tz("timestamp", "timestamptz")?;
                from_local(x.value, tz).cmp(&y.value)
            }
            (TimestampTz, Timestamp) => {
                self.need_tz("timestamp", "timestamptz")?;
                x.value.cmp(&from_local(y.value, tz))
            }
            (Time, TimeTz) => {
                self.need_tz("time", "timetz")?;
                timetz_cmp(x.value, self.time_zone_now(x.value), y.value, y.offset)
            }
            (TimeTz, Time) => {
                self.need_tz("time", "timetz")?;
                timetz_cmp(x.value, x.offset, y.value, self.time_zone_now(y.value))
            }
            (TimeTz, TimeTz) => timetz_cmp(x.value, x.offset, y.value, y.offset),
            _ => return Ok(None),
        }))
    }
}

/// `jspConvertRegexFlags`: XQuery flags as regex compile flags.
fn like_regex_cflags(flags: &str) -> Result<i32> {
    use crate::types::regex::{REG_ADVANCED, REG_ICASE, REG_NLANCH, REG_NLSTOP, REG_QUOTE};
    let mut cflags = REG_ADVANCED;
    if flags.contains('i') {
        cflags |= REG_ICASE;
    }
    if flags.contains('q') {
        cflags &= !REG_ADVANCED;
        cflags |= REG_QUOTE;
    } else {
        // dotall mode is the default in POSIX
        if !flags.contains('s') {
            cflags |= REG_NLSTOP;
        }
        if flags.contains('m') {
            cflags |= REG_NLANCH;
        }
        if flags.contains('x') {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "XQuery \"x\" flag (expanded regular expressions) is not implemented",
            ));
        }
    }
    Ok(cflags)
}

/// The compiled pattern of a like_regex predicate (RE_compile_and_execute's, under the
/// default collation).
fn like_regex(pattern: &str, flags: &str) -> R<std::sync::Arc<crate::types::regex::Regex>> {
    let cflags = like_regex_cflags(flags).map_err(Fail::Hard)?;
    crate::types::regex::compile(
        pattern,
        cflags | crate::types::regex::REG_NOSUB,
        crate::types::regex::default_strategy(),
    )
    .map_err(Fail::Hard)
}

// ── date/time items ──

const USECS_PER_DAY: i64 = 86_400_000_000;

/// date2timestamp_no_overflow: dates past the timestamp range compare as infinity.
fn date_ts_no_overflow(d: i64) -> i64 {
    use super::datetime::{POSTGRES_EPOCH_JDATE, TIMESTAMP_END_JULIAN, TS_POS_INF};
    if d >= (TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE) as i64 {
        TS_POS_INF
    } else {
        d * USECS_PER_DAY
    }
}

/// date2timestamp.
fn date_ts(d: i64) -> R<i64> {
    use super::datetime::{POSTGRES_EPOCH_JDATE, TIMESTAMP_END_JULIAN};
    if d >= (TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE) as i64 {
        return Err(Fail::Hard(Error::new(
            code::DATETIME_FIELD_OVERFLOW,
            "date out of range for timestamp",
        )));
    }
    Ok(d * USECS_PER_DAY)
}

/// The ISO formats `.datetime()` & co. try, in order (date, timetz, time, timestamptz,
/// timestamp; with "T" as well, as to_json writes it).
const ISO_DATETIME_FORMATS: [&str; 13] = [
    "yyyy-mm-dd",
    "HH24:MI:SS.USTZ",
    "HH24:MI:SSTZ",
    "HH24:MI:SS.US",
    "HH24:MI:SS",
    "yyyy-mm-dd HH24:MI:SS.USTZ",
    "yyyy-mm-dd HH24:MI:SSTZ",
    "yyyy-mm-dd\"T\"HH24:MI:SS.USTZ",
    "yyyy-mm-dd\"T\"HH24:MI:SSTZ",
    "yyyy-mm-dd HH24:MI:SS.US",
    "yyyy-mm-dd HH24:MI:SS",
    "yyyy-mm-dd\"T\"HH24:MI:SS.US",
    "yyyy-mm-dd\"T\"HH24:MI:SS",
];

/// JsonEncodeDateTime: the type's output in the XSD (ISO 8601 with `T`) style.
fn format_datetime(d: &DateTime) -> String {
    use super::datetime::{
        DateFmt, DateOrder, DateStyle, TS_NEG_INF, TS_POS_INF, Tm, dt2time, encode_date_only,
        encode_date_time, encode_time_only, timestamp2tm,
    };
    let style = DateStyle {
        fmt: DateFmt::Xsd,
        order: DateOrder::Mdy,
    };
    let time_tm = |us: i64| {
        let (hour, min, sec, fsec) = dt2time(us);
        (
            Tm {
                hour,
                min,
                sec,
                ..Default::default()
            },
            fsec,
        )
    };
    match d.kind {
        DtKind::Date => match super::datetime::format_date(d.value as i32).as_str() {
            s @ ("infinity" | "-infinity") => s.to_string(),
            _ => {
                let (y, m, dd) = super::datetime::j2date(d.value as i32 + super::datetime::POSTGRES_EPOCH_JDATE);
                encode_date_only(
                    &Tm {
                        year: y,
                        mon: m,
                        mday: dd,
                        ..Default::default()
                    },
                    style,
                )
            }
        },
        DtKind::Time => {
            let (tm, fsec) = time_tm(d.value);
            encode_time_only(&tm, fsec, None, DateFmt::Xsd)
        }
        DtKind::TimeTz => {
            let (tm, fsec) = time_tm(d.value);
            encode_time_only(&tm, fsec, Some(-d.offset), DateFmt::Xsd)
        }
        DtKind::Timestamp | DtKind::TimestampTz => {
            let zoned = d.kind == DtKind::TimestampTz;
            if d.value == TS_POS_INF {
                return "infinity".into();
            }
            if d.value == TS_NEG_INF {
                return "-infinity".into();
            }
            let local = if zoned {
                d.value + d.offset as i64 * 1_000_000
            } else {
                d.value
            };
            match timestamp2tm(local, None) {
                Some((mut tm, fsec)) => {
                    tm.isdst = 1;
                    encode_date_time(&tm, fsec, zoned, -d.offset, None, style)
                }
                None => String::new(),
            }
        }
    }
}

// ── keyvalue ids: offsets in Postgres's binary jsonb layout ──

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// On-disk size of a numeric (varlena header + short or long numeric header + base-10000
/// digits).
fn numeric_size(v: &Numeric) -> usize {
    let Numeric::Finite { int, scale } = v else {
        return 6;
    };
    let digits = int.magnitude().to_string();
    let scale = *scale as usize;
    let (ip, fp) = if digits.len() > scale {
        digits.split_at(digits.len() - scale)
    } else {
        ("", "")
    };
    let fp_full = if digits.len() > scale {
        fp.to_string()
    } else {
        format!("{}{}", "0".repeat(scale - digits.len()), digits)
    };
    let ip = ip.trim_start_matches('0');
    // Groups of 4 decimal digits around the point.
    let int_groups: Vec<u32> = {
        let pad = (4 - ip.len() % 4) % 4;
        let s = format!("{}{}", "0".repeat(pad), ip);
        s.as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).unwrap().parse().unwrap())
            .collect()
    };
    let frac_groups: Vec<u32> = {
        let pad = (4 - fp_full.len() % 4) % 4;
        let s = format!("{}{}", fp_full, "0".repeat(pad));
        s.as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).unwrap().parse().unwrap())
            .collect()
    };
    let mut all: Vec<u32> = int_groups
        .iter()
        .chain(frac_groups.iter())
        .copied()
        .collect();
    let mut weight = int_groups.len() as i64 - 1;
    while all.first() == Some(&0) {
        all.remove(0);
        weight -= 1;
    }
    while all.last() == Some(&0) {
        all.pop();
    }
    if all.is_empty() {
        weight = 0;
    }
    let short = scale <= 63 && (-64..=63).contains(&weight);
    4 + if short { 2 } else { 4 } + 2 * all.len()
}

/// Size of a container's binary form starting at an aligned offset.
fn container_size(j: &Jsonb) -> usize {
    let mut pos = 4;
    match j {
        Jsonb::Array(items) => {
            pos += 4 * items.len();
            for it in items {
                pos = place(it, pos);
            }
        }
        Jsonb::Object(pairs) => {
            pos += 8 * pairs.len();
            for (k, _) in pairs {
                pos += k.len();
            }
            for (_, v) in pairs {
                pos = place(v, pos);
            }
        }
        // A raw scalar is stored as a one-element array.
        s => {
            pos += 4;
            pos = place(s, pos);
        }
    }
    pos
}

/// The position after a value stored at `pos`.
fn place(v: &Jsonb, pos: usize) -> usize {
    match v {
        Jsonb::Null | Jsonb::Bool(_) => pos,
        Jsonb::String(s) => pos + s.len(),
        Jsonb::Number(n) => align4(pos) + numeric_size(n),
        c => align4(pos) + container_size(c),
    }
}

/// Byte offset of the container `target` (by address) from the root container's header.
fn container_offset(root: &Jsonb, target: *const Jsonb) -> Option<usize> {
    fn walk(j: &Jsonb, start: usize, target: *const Jsonb) -> Option<usize> {
        if std::ptr::eq(j, target) {
            return Some(start);
        }
        let mut pos = start + 4;
        let children: Vec<&Jsonb> = match j {
            Jsonb::Array(items) => {
                pos += 4 * items.len();
                items.iter().collect()
            }
            Jsonb::Object(pairs) => {
                pos += 8 * pairs.len();
                for (k, _) in pairs {
                    pos += k.len();
                }
                pairs.iter().map(|(_, v)| v).collect()
            }
            _ => return None,
        };
        for c in children {
            if matches!(c, Jsonb::Array(_) | Jsonb::Object(_)) {
                let at = align4(pos);
                if let Some(o) = walk(c, at, target) {
                    return Some(o);
                }
            }
            pos = place(c, pos);
        }
        None
    }
    walk(root, 0, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(s: &str) -> String {
        normalize(s).unwrap()
    }

    #[test]
    fn canonical_text() {
        assert_eq!(canon("$.a[*] ? (@ > 2)"), "$.\"a\"[*]?(@ > 2)");
        assert_eq!(canon("strict $.a.b"), "strict $.\"a\".\"b\"");
        assert_eq!(canon("$.a + 1"), "($.\"a\" + 1)");
        assert_eq!(canon("-1"), "-1");
        assert_eq!(canon("1 + 2 * 3"), "(1 + 2 * 3)");
        assert_eq!(canon("(1 + 2) * 3"), "((1 + 2) * 3)");
        assert_eq!(canon("$[1, 2 to 3, last]"), "$[1,2 to 3,last]");
        assert_eq!(canon("$.**{2 to last}"), "$.**{2 to last}");
        assert_eq!(canon("$.A.TYPE()"), "$.\"A\".type()");
        assert_eq!(canon("- $.a"), "(-$.\"a\")");
        assert_eq!(canon("0x1F"), "31");
        assert_eq!(canon("$.size"), "$.\"size\"");
    }

    #[test]
    fn syntax_errors() {
        assert_eq!(
            normalize("$.a[").unwrap_err().message,
            "syntax error at end of jsonpath input"
        );
        assert_eq!(
            normalize("@.a").unwrap_err().message,
            "@ is not allowed in root expressions"
        );
        assert_eq!(
            normalize("last").unwrap_err().message,
            "LAST is allowed only in array subscripts"
        );
        assert_eq!(
            normalize("$ ? (1)").unwrap_err().message,
            "syntax error at or near \")\" of jsonpath input"
        );
        assert_eq!(
            normalize("$.a == 1 == 2").unwrap_err().message,
            "syntax error at or near \"==\" of jsonpath input"
        );
    }

    #[test]
    fn keyvalue_offsets() {
        let j = super::super::json::parse_jsonb(r#"{"a":{"x":{"y":1}, "z": {"q": 2}}}"#).unwrap();
        let a = j.get("a").unwrap();
        assert_eq!(container_offset(&j, a), Some(16));
        assert_eq!(container_offset(&j, a.get("x").unwrap()), Some(40));
        assert_eq!(container_offset(&j, a.get("z").unwrap()), Some(64));
    }
}
