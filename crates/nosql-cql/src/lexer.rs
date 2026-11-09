//! The CQL lexer. It follows Cassandra's `Lexer.g` (Apache-2.0) and reproduces how the lexer
//! ANTLR 3 generates from it behaves, including its errors:
//!
//! - A token's rule is *predicted* by reading characters while more than one rule can still match
//!   the prefix. When exactly one rule remains, it is chosen even if it later fails (`123abc`
//!   is predicted as a UUID and fails at the space). When no rule can take the next character,
//!   the highest-priority rule that matches the prefix exactly wins (keywords over identifiers,
//!   `INTEGER` over `FLOAT`, `DURATION` over `IDENT`); if none does, that is a "no viable
//!   alternative" error. The predicted rule then matches greedily and reports a mismatch where
//!   its pattern breaks.
//! - Line comments must end with a newline, `$$` strings stop at the first `$$`, and `-1`,
//!   `-NaN`, `-Infinity` and `-1h` are single tokens.
//!
//! Each rule is a small deterministic automaton stepped one character at a time, so lexing is
//! linear. Whitespace and comments are counted (the error snippets index the full token stream)
//! but only kept when asked for.
//!
//! ScyllaDB's lexer is the same, without `/`, `%`, `+=`, `-=`, `expr(` and the empty quoted
//! name `""`.

use crate::Dialect;
use crate::tokens::{self as t, Tt};

pub(crate) const WS: Tt = 203;
pub(crate) const COMMENT: Tt = 8;
pub(crate) const MULTILINE_COMMENT: Tt = 187;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Token {
    pub ty: Tt,
    /// Byte offsets in the query.
    pub start: u32,
    pub end: u32,
    /// 1-based line.
    pub line: u32,
    /// 0-based column in UTF-16 units (Java's `charPositionInLine`).
    pub col: u32,
    /// 0-based column in bytes (ScyllaDB's).
    pub bcol: u32,
    /// Index in the full token stream, hidden tokens included.
    pub raw: u32,
}

impl Token {
    pub fn is_hidden(&self) -> bool {
        matches!(self.ty, WS | COMMENT | MULTILINE_COMMENT)
    }
}

/// What went wrong in the lexer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LexErrorKind {
    /// No rule matches here. `None` is end of input.
    NoViable(Option<char>),
    /// A rule wanted `expected`.
    MismatchedChar { found: Option<char>, expected: char },
    /// A rule wanted one of a set of characters.
    MismatchedSet(Option<char>),
    /// A `(...)+` loop matched nothing (the digits of an exponent).
    EarlyExit(Option<char>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LexError {
    /// The raw token index the parser reaches when the error is reported.
    pub raw: u32,
    pub line: u32,
    pub col: u32,
    pub bcol: u32,
    pub kind: LexErrorKind,
}

fn char_display(c: Option<char>) -> String {
    match c {
        None => "<EOF>".into(),
        Some('\n') => "\\n".into(),
        Some('\t') => "\\t".into(),
        Some('\r') => "\\r".into(),
        Some(c) => c.to_string(),
    }
}

impl LexError {
    pub fn message(&self, dialect: Dialect) -> String {
        match dialect {
            Dialect::Cassandra => {
                let body = match self.kind {
                    LexErrorKind::NoViable(c) => format!("no viable alternative at character '{}'", char_display(c)),
                    LexErrorKind::MismatchedChar { found, expected } => format!(
                        "mismatched character '{}' expecting '{}'",
                        char_display(found),
                        char_display(Some(expected))
                    ),
                    LexErrorKind::MismatchedSet(c) => {
                        format!("mismatched character '{}' expecting set null", char_display(c))
                    }
                    LexErrorKind::EarlyExit(c) => {
                        format!("required (...)+ loop did not match anything at character '{}'", char_display(c))
                    }
                };
                format!("line {}:{} {}", self.line, self.col, body)
            }
            Dialect::Scylla => match self.kind {
                LexErrorKind::NoViable(_) => {
                    format!("line {}:{} no viable alternative at input ''", self.line, self.bcol)
                }
                LexErrorKind::MismatchedChar { .. } => format!("line {}:{} : Syntax error", self.line, self.bcol),
                LexErrorKind::MismatchedSet(_) => format!("line {}:{} : Unexpected ''", self.line, self.bcol),
                LexErrorKind::EarlyExit(_) => format!("line {}:{} : Syntax error", self.line, self.bcol),
            },
        }
    }
}

pub(crate) struct Lexed {
    /// Visible tokens (all tokens with `keep_hidden`), ending with `EOF`.
    pub tokens: Vec<Token>,
    pub error: Option<LexError>,
}

// Candidate rules, in ANTLR's priority order (the order they are defined in `Lexer.g`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Rule {
    NegNan,
    NegInf,
    Minus,
    MinusEq,
    Expr,
    Integer,
    Float,
    Duration,
    Ident,
    Hex,
    Uuid,
    Comment,
}

/// Automaton state per rule; `None` once the rule can no longer match.
#[derive(Clone, Copy)]
struct Machines {
    neg_nan: Option<u8>,
    neg_inf: Option<u8>,
    minus: Option<u8>,
    minus_eq: Option<u8>,
    expr: Option<u8>,
    integer: Option<u8>,
    float: Option<u8>,
    duration: Option<u16>,
    ident: Option<u8>,
    hex: Option<u8>,
    uuid: Option<u8>,
    comment: Option<u8>,
}

fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

fn step_literal(state: u8, lit: &[u8], c: char, case_insensitive: bool) -> Option<u8> {
    let i = state as usize;
    let want = *lit.get(i)? as char;
    let ok = if case_insensitive { c.eq_ignore_ascii_case(&want) } else { c == want };
    ok.then_some(state + 1)
}

// INTEGER: '-'? DIGIT+          states: 0 start, 1 after '-', 2 digits (accept)
fn step_integer(s: u8, c: char) -> Option<u8> {
    match (s, c) {
        (0, '-') => Some(1),
        (0..=2, '0'..='9') => Some(2),
        _ => None,
    }
}

// FLOAT: INTEGER ('.' DIGIT*)? EXPONENT?
// 0 start, 1 '-', 2 int digits (accept), 3 after '.' (accept), 4 after E, 5 after E sign, 6 exp digits (accept)
fn step_float(s: u8, c: char) -> Option<u8> {
    match (s, c) {
        (0, '-') => Some(1),
        (0..=2, '0'..='9') => Some(2),
        (2, '.') => Some(3),
        (3, '0'..='9') => Some(3),
        (2 | 3, 'e' | 'E') => Some(4),
        (4, '+' | '-') => Some(5),
        (4..=6, '0'..='9') => Some(6),
        _ => None,
    }
}

fn float_accepts(s: u8) -> bool {
    matches!(s, 2 | 3 | 6)
}

// HEXNUMBER: '0' X HEX*        0 start, 1 after '0', 2 after x (accept)
fn step_hex(s: u8, c: char) -> Option<u8> {
    match (s, c) {
        (0, '0') => Some(1),
        (1, 'x' | 'X') => Some(2),
        (2, c) if is_hex(c) => Some(2),
        _ => None,
    }
}

// UUID: 8-4-4-4-12 hex. State = number of characters matched (accept at 36).
fn step_uuid(s: u8, c: char) -> Option<u8> {
    match s {
        8 | 13 | 18 | 23 => (c == '-').then_some(s + 1),
        0..=35 => is_hex(c).then_some(s + 1),
        _ => None,
    }
}

// IDENT: LETTER (LETTER | DIGIT | '_')*
fn step_ident(s: u8, c: char) -> Option<u8> {
    match s {
        0 => c.is_ascii_alphabetic().then_some(1),
        _ => (c.is_ascii_alphanumeric() || c == '_').then_some(1),
    }
}

// COMMENT: ('--' | '//') .* ('\n'|'\r'). 0 start, 1 first char, 2 body, 3 done (accept)
fn step_comment(s: u8, c: char, first: char) -> Option<u8> {
    match s {
        0 => (c == first).then_some(1),
        1 => (c == first).then_some(2),
        2 => Some(if c == '\n' || c == '\r' { 3 } else { 2 }),
        _ => None,
    }
}

// DURATION. Unit form: '-'? DIGIT+ UNIT (DIGIT+ UNIT)*. ISO forms: '-'? 'P' ...
// States are grouped: 0..=19 unit form, 20.. ISO forms.
mod dur {
    pub const START: u16 = 0;
    pub const NEG: u16 = 1;
    pub const DIGITS: u16 = 2;
    pub const UNIT: u16 = 3; // accept
    pub const M: u16 = 4; // accept ('m' may extend to 'mo' / 'ms')
    pub const NEED_S: u16 = 5; // after 'u', 'µ' or 'n'
    pub const P: u16 = 20; // after 'P'
    pub const PD: u16 = 21; // digits after 'P' (count in ALT_COUNT)
    pub const Y: u16 = 22; // accept
    pub const YD: u16 = 23; // digits after 'Y'
    pub const MO: u16 = 24; // accept (months)
    pub const MOD: u16 = 25; // digits after months
    pub const D: u16 = 26; // accept
    pub const W: u16 = 27; // accept, final
    pub const T: u16 = 28;
    pub const TD: u16 = 29; // digits after 'T'
    pub const TH: u16 = 30; // accept
    pub const THD: u16 = 31;
    pub const TM: u16 = 32; // accept
    pub const TMD: u16 = 33;
    pub const TS: u16 = 34; // accept, final
    pub const PD4: u16 = 35; // exactly four digits after 'P' (alternative format possible)
    pub const PD1: u16 = 36; // 1..3 digits after 'P'
    // Alternative format "PYYYY-MM-DDThh:mm:ss": positions after "PYYYY".
    pub const ALT: u16 = 40; // ALT + n characters matched of "-DD-DDTDD:DD:DD"
    pub const ALT_PATTERN: &[u8] = b"-##-##T##:##:##";
}

fn step_duration(s: u16, c: char) -> Option<u16> {
    use dur::*;
    let digit = c.is_ascii_digit();
    match s {
        START => match c {
            '-' => Some(NEG),
            'P' => Some(P),
            _ if digit => Some(DIGITS),
            _ => None,
        },
        NEG => match c {
            'P' => Some(P),
            _ if digit => Some(DIGITS),
            _ => None,
        },
        DIGITS => match c.to_ascii_lowercase() {
            _ if digit => Some(DIGITS),
            'y' | 'w' | 'd' | 'h' | 's' => Some(UNIT),
            'm' => Some(M),
            'u' | 'n' => Some(NEED_S),
            'µ' => Some(NEED_S),
            _ => None,
        },
        UNIT => digit.then_some(DIGITS),
        M => match c {
            'o' | 'O' | 's' | 'S' => Some(UNIT),
            _ if digit => Some(DIGITS),
            _ => None,
        },
        NEED_S => matches!(c, 's' | 'S').then_some(UNIT),
        P => match c {
            'T' => Some(T),
            _ if digit => Some(PD1),
            _ => None,
        },
        PD1 | PD4 | PD => match c {
            _ if digit => Some(PD),
            'Y' => Some(Y),
            'M' => Some(MO),
            'D' => Some(D),
            'W' => Some(W),
            '-' if s == PD4 => Some(ALT + 1),
            _ => None,
        },
        Y => match c {
            'T' => Some(T),
            _ if digit => Some(YD),
            _ => None,
        },
        YD => match c {
            _ if digit => Some(YD),
            'M' => Some(MO),
            'D' => Some(D),
            _ => None,
        },
        MO => match c {
            'T' => Some(T),
            _ if digit => Some(MOD),
            _ => None,
        },
        MOD => match c {
            _ if digit => Some(MOD),
            'D' => Some(D),
            _ => None,
        },
        D => (c == 'T').then_some(T),
        T => digit.then_some(TD),
        TD => match c {
            _ if digit => Some(TD),
            'H' => Some(TH),
            'M' => Some(TM),
            'S' => Some(TS),
            _ => None,
        },
        TH => digit.then_some(THD),
        THD => match c {
            _ if digit => Some(THD),
            'M' => Some(TM),
            'S' => Some(TS),
            _ => None,
        },
        TM => digit.then_some(TMD),
        TMD => match c {
            _ if digit => Some(TMD),
            'S' => Some(TS),
            _ => None,
        },
        s if s > ALT && ((s - ALT) as usize) < ALT_PATTERN.len() => {
            let want = ALT_PATTERN[(s - ALT) as usize];
            let ok = if want == b'#' { digit } else { c == want as char };
            ok.then_some(s + 1)
        }
        _ => None,
    }
}

/// The digit-count bookkeeping for "exactly four digits after P" is done by the caller, which
/// counts digits; see [`duration_step_counted`].
fn duration_step_counted(s: u16, c: char, digits_after_p: &mut u8, scylla: bool) -> Option<u16> {
    use dur::*;
    // ScyllaDB's lexer works on bytes and has no `µs` unit.
    if scylla && c == 'µ' {
        return None;
    }
    if (s == P || s == PD1 || s == PD4 || s == PD) && c.is_ascii_digit() {
        *digits_after_p = digits_after_p.saturating_add(1);
        return Some(match *digits_after_p {
            1..=3 => PD1,
            4 => PD4,
            _ => PD,
        });
    }
    step_duration(s, c)
}

fn duration_accepts(s: u16, scylla: bool) -> bool {
    use dur::*;
    // In ScyllaDB every part of an ISO 8601 duration is optional: `P`, `PT` and `P1DT` lex.
    matches!(s, UNIT | M | Y | MO | D | W | TH | TM | TS)
        || s == ALT + ALT_PATTERN.len() as u16
        || (scylla && matches!(s, P | T))
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    line: u32,
    col: u32,
    bcol: u32,
    dialect: Dialect,
}

impl Lexer<'_> {
    fn peek_at(&self, byte: usize) -> Option<char> {
        self.src.get(byte..).and_then(|s| s.chars().next())
    }

    /// Advances over `bytes` bytes from the current position, tracking line and columns.
    fn advance_to(&mut self, end: usize) {
        for c in self.src[self.pos..end].chars() {
            if c == '\n' {
                self.line += 1;
                self.col = 0;
                self.bcol = 0;
            } else {
                self.col += c.len_utf16() as u32;
                self.bcol += c.len_utf8() as u32;
            }
        }
        self.pos = end;
    }

    /// The position (line, col, bcol) of byte `at`, which is at or after the current position.
    fn position_of(&self, at: usize) -> (u32, u32, u32) {
        let (mut line, mut col, mut bcol) = (self.line, self.col, self.bcol);
        for c in self.src[self.pos..at].chars() {
            if c == '\n' {
                line += 1;
                col = 0;
                bcol = 0;
            } else {
                col += c.len_utf16() as u32;
                bcol += c.len_utf8() as u32;
            }
        }
        (line, col, bcol)
    }

    /// Lexes one token at the current position: `Ok((type, end))` or an error at a byte offset.
    fn next(&self) -> Result<(Tt, usize), (usize, LexErrorKind)> {
        let p = self.pos;
        let s = &self.src[p..];
        let mut it = s.chars();
        let c = it.next().expect("not at end");
        let c2 = it.next();
        let scylla = self.dialect == Dialect::Scylla;
        let simple = |ty: Tt, len: usize| Ok((ty, p + len));
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                let n = s.bytes().take_while(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r')).count();
                return simple(WS, n);
            }
            '\'' => return self.quoted(p, '\'', t::STRING_LITERAL),
            '"' => {
                if c2 == Some('"') && s.as_bytes().get(2) != Some(&b'"') {
                    if scylla {
                        // ScyllaDB has no empty quoted name: QUOTED_NAME needs a character.
                        return Err((p + 1, LexErrorKind::MismatchedChar { found: Some('"'), expected: '"' }));
                    }
                    return simple(t::EMPTY_QUOTED_NAME, 2);
                }
                if c2.is_none() {
                    return Err((p + 1, LexErrorKind::NoViable(None)));
                }
                return self.quoted(p, '"', t::QUOTED_NAME);
            }
            '$' => return self.dollar(p),
            '/' if c2 == Some('*') => return self.block_comment(p),
            '/' if c2 == Some('/') => {}
            // In ScyllaDB `/` only starts a comment: the character after it is not viable.
            '/' if scylla => return Err((p + 1, LexErrorKind::NoViable(c2))),
            '/' => return simple(t::SLASH, 1),
            '%' if !scylla => return simple(t::PERCENT, 1),
            '(' => return simple(t::LPAREN, 1),
            ')' => return simple(t::RPAREN, 1),
            ',' => return simple(t::COMMA, 1),
            ';' => return simple(t::SEMI, 1),
            '[' => return simple(t::LBRACKET, 1),
            ']' => return simple(t::RBRACKET, 1),
            '{' => return simple(t::LBRACE, 1),
            '}' => return simple(t::RBRACE, 1),
            '*' => return simple(t::STAR, 1),
            '?' => return simple(t::QMARK, 1),
            ':' => return simple(t::COLON, 1),
            '=' => return simple(t::EQ, 1),
            '<' => return if c2 == Some('=') { simple(t::LTE, 2) } else { simple(t::LT, 1) },
            '>' => return if c2 == Some('=') { simple(t::GTE, 2) } else { simple(t::GT, 1) },
            '+' => return if c2 == Some('=') && !scylla { simple(t::PLUS_EQ, 2) } else { simple(t::PLUS, 1) },
            '!' => {
                return if c2 == Some('=') {
                    simple(t::NEQ, 2)
                } else {
                    Err((p + 1, LexErrorKind::MismatchedChar { found: c2, expected: '=' }))
                };
            }
            '.' => return if c2 == Some('.') && !scylla { simple(t::RANGE, 2) } else { simple(t::DOT, 1) },
            _ => {}
        }
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '/') {
            return Err((p, LexErrorKind::NoViable(Some(c))));
        }
        self.predict(p)
    }

    /// `'...'` with `''` escapes, `"..."` with `""` escapes.
    fn quoted(&self, p: usize, q: char, ty: Tt) -> Result<(Tt, usize), (usize, LexErrorKind)> {
        let b = self.src.as_bytes();
        let q = q as u8;
        let mut i = p + 1;
        loop {
            match b.get(i) {
                None => return Err((i, LexErrorKind::MismatchedChar { found: None, expected: q as char })),
                Some(&c) if c == q => {
                    if b.get(i + 1) == Some(&q) {
                        i += 2;
                    } else {
                        return Ok((ty, i + 1));
                    }
                }
                Some(_) => i += 1,
            }
        }
    }

    /// `$$...$$`. The body loop of `Lexer.g` runs while more than one character remains and stops
    /// at the first `$$`.
    fn dollar(&self, p: usize) -> Result<(Tt, usize), (usize, LexErrorKind)> {
        let s = self.src;
        if s.as_bytes().get(p + 1) != Some(&b'$') {
            return Err((p + 1, LexErrorKind::MismatchedChar { found: self.peek_at(p + 1), expected: '$' }));
        }
        if let Some(i) = s[p + 2..].find("$$") {
            return Ok((t::STRING_LITERAL, p + 2 + i + 2));
        }
        if self.dialect == Dialect::Scylla {
            return Err((s.len(), LexErrorKind::MismatchedChar { found: None, expected: '$' }));
        }
        // No closing `$$`: the loop stops before the last character, which must then be `$$`.
        let last = s[p + 2..].char_indices().last().map(|(i, _)| p + 2 + i);
        let stop = last.unwrap_or(s.len());
        match self.peek_at(stop) {
            Some('$') => Err((stop + 1, LexErrorKind::MismatchedChar { found: self.peek_at(stop + 1), expected: '$' })),
            found => Err((stop, LexErrorKind::MismatchedChar { found, expected: '$' })),
        }
    }

    fn block_comment(&self, p: usize) -> Result<(Tt, usize), (usize, LexErrorKind)> {
        match self.src[p + 2..].find("*/") {
            Some(i) => Ok((MULTILINE_COMMENT, p + 2 + i + 2)),
            None => Err((self.src.len(), LexErrorKind::MismatchedChar { found: None, expected: '*' })),
        }
    }

    /// Prediction among the rules that can start with an alphanumeric, `-` or `/` (the rules
    /// whose prefixes overlap), then the greedy match of the predicted rule.
    fn predict(&self, p: usize) -> Result<(Tt, usize), (usize, LexErrorKind)> {
        let s = &self.src[p..];
        let first = s.chars().next().unwrap_or(' ');
        let scylla = self.dialect == Dialect::Scylla;
        let mut m = Machines {
            // ScyllaDB has no `-NaN` / `-Infinity` tokens: `-` is a token of its own there.
            neg_nan: if scylla { None } else { Some(0) },
            neg_inf: if scylla { None } else { Some(0) },
            minus: Some(0),
            minus_eq: if scylla { None } else { Some(0) },
            expr: if scylla { None } else { Some(0) },
            integer: Some(0),
            float: Some(0),
            duration: Some(0),
            ident: Some(0),
            hex: Some(0),
            uuid: Some(0),
            comment: Some(0),
        };
        let comment_char = if first == '/' { '/' } else { '-' };
        let mut digits_after_p = 0u8;
        // Accepting rules at the current prefix length, highest priority first.
        let accepting = |m: &Machines, len: usize| -> Option<Rule> {
            let word = &s[..len];
            [
                (m.neg_nan == Some(4), Rule::NegNan),
                (m.neg_inf == Some(9), Rule::NegInf),
                (m.minus == Some(1), Rule::Minus),
                (m.minus_eq == Some(2), Rule::MinusEq),
                (m.expr == Some(5), Rule::Expr),
                // Keywords are checked with `Ident` below: they outrank INTEGER..IDENT anyway.
                (m.integer == Some(2), Rule::Integer),
                (m.float.is_some_and(float_accepts), Rule::Float),
                (m.duration.is_some_and(|d| duration_accepts(d, scylla)), Rule::Duration),
                (m.ident == Some(1) && !word.is_empty(), Rule::Ident),
                (m.hex == Some(2), Rule::Hex),
                (m.uuid == Some(36), Rule::Uuid),
                (m.comment == Some(3), Rule::Comment),
            ]
            .into_iter()
            .find(|(ok, _)| *ok)
            .map(|(_, r)| r)
        };
        let alive = |m: &Machines| -> (usize, Option<Rule>) {
            let all = [
                (m.neg_nan.is_some(), Rule::NegNan),
                (m.neg_inf.is_some(), Rule::NegInf),
                (m.minus.is_some(), Rule::Minus),
                (m.minus_eq.is_some(), Rule::MinusEq),
                (m.expr.is_some(), Rule::Expr),
                (m.integer.is_some(), Rule::Integer),
                (m.float.is_some(), Rule::Float),
                (m.duration.is_some(), Rule::Duration),
                (m.ident.is_some(), Rule::Ident),
                (m.hex.is_some(), Rule::Hex),
                (m.uuid.is_some(), Rule::Uuid),
                (m.comment.is_some(), Rule::Comment),
            ];
            let n = all.iter().filter(|(a, _)| *a).count();
            (n, all.iter().find(|(a, _)| *a).map(|(_, r)| *r))
        };
        let mut len = 0usize;
        let mut chars = s.char_indices();
        // Prediction.
        let predicted: Rule = loop {
            let next = chars.next();
            let Some((_, c)) = next else {
                // End of input: the accepting rule at this length, if any.
                match accepting(&m, len) {
                    Some(r) => break r,
                    None => return Err((p + len, LexErrorKind::NoViable(None))),
                }
            };
            let mut n = m;
            n.neg_nan = n.neg_nan.and_then(|st| step_literal(st, b"-NAN", c, true));
            n.neg_inf = n.neg_inf.and_then(|st| step_literal(st, b"-INFINITY", c, true));
            n.minus = n.minus.and_then(|st| step_literal(st, b"-", c, false));
            n.minus_eq = n.minus_eq.and_then(|st| step_literal(st, b"-=", c, false));
            n.expr = n.expr.and_then(|st| step_literal(st, b"expr(", c, false));
            n.integer = n.integer.and_then(|st| step_integer(st, c));
            n.float = n.float.and_then(|st| step_float(st, c));
            n.duration = n.duration.and_then(|st| duration_step_counted(st, c, &mut digits_after_p, scylla));
            n.ident = n.ident.and_then(|st| step_ident(st, c));
            n.hex = n.hex.and_then(|st| step_hex(st, c));
            n.uuid = n.uuid.and_then(|st| step_uuid(st, c));
            n.comment = n.comment.and_then(|st| step_comment(st, c, comment_char));
            let (count, only) = alive(&n);
            if count == 0 {
                match accepting(&m, len) {
                    Some(r) => break r,
                    None => return Err((p + len, LexErrorKind::NoViable(Some(c)))),
                }
            }
            m = n;
            len += c.len_utf8();
            if count == 1 {
                break only.expect("one rule alive");
            }
            // A finished comment cannot continue.
            if m.comment == Some(3) {
                break Rule::Comment;
            }
        };
        self.finish(p, predicted)
    }

    /// Runs the predicted rule greedily from `p`.
    fn finish(&self, p: usize, rule: Rule) -> Result<(Tt, usize), (usize, LexErrorKind)> {
        let s = &self.src[p..];
        let at = |i: usize| s[i..].chars().next();
        // A literal rule matched character by character (keywords through case-insensitive sets).
        let literal = |lit: &str, ty: Tt, sets: bool| -> Result<(Tt, usize), (usize, LexErrorKind)> {
            let b = s.as_bytes();
            for (i, want) in lit.bytes().enumerate() {
                let ok = b.get(i).is_some_and(|c| c.eq_ignore_ascii_case(&want) && (sets || *c == want));
                if !ok {
                    let found = at(i);
                    return Err((
                        p + i,
                        if sets && want.is_ascii_alphabetic() {
                            LexErrorKind::MismatchedSet(found)
                        } else {
                            LexErrorKind::MismatchedChar { found, expected: want as char }
                        },
                    ));
                }
            }
            Ok((ty, p + lit.len()))
        };
        match rule {
            Rule::NegNan => literal("-NAN", t::K_NEGATIVE_NAN, true),
            Rule::NegInf => literal("-INFINITY", t::K_NEGATIVE_INFINITY, true),
            Rule::Minus => Ok((t::MINUS, p + 1)),
            Rule::MinusEq => Ok((t::MINUS_EQ, p + 2)),
            Rule::Expr => literal("expr(", t::EXPR_LPAREN, false),
            Rule::Comment => match s[2..].find(['\n', '\r']) {
                Some(i) => Ok((COMMENT, p + 2 + i + 1)),
                None => Err((self.src.len(), LexErrorKind::MismatchedSet(None))),
            },
            Rule::Hex => {
                let n = 2 + s[2..].bytes().take_while(|b| b.is_ascii_hexdigit()).count();
                Ok((t::HEXNUMBER, p + n))
            }
            Rule::Uuid => {
                let mut st = 0u8;
                let mut i = 0usize;
                while st < 36 {
                    let c = at(i);
                    match c.and_then(|c| step_uuid(st, c)) {
                        Some(n) => {
                            st = n;
                            i += c.map_or(0, char::len_utf8);
                        }
                        None if matches!(st, 8 | 13 | 18 | 23) => {
                            return Err((p + i, LexErrorKind::MismatchedChar { found: c, expected: '-' }));
                        }
                        None => return Err((p + i, LexErrorKind::MismatchedSet(c))),
                    }
                }
                Ok((t::UUID, p + i))
            }
            Rule::Integer => {
                let neg = s.starts_with('-') as usize;
                let n = neg + s[neg..].bytes().take_while(u8::is_ascii_digit).count();
                Ok((t::INTEGER, p + n))
            }
            Rule::Float => {
                let neg = s.starts_with('-') as usize;
                let int = neg + s[neg..].bytes().take_while(u8::is_ascii_digit).count();
                let b = s.as_bytes();
                // `1..2` is INTEGER RANGE INTEGER; `1...` is FLOAT("1.") RANGE. ScyllaDB has no
                // range token.
                if self.dialect == Dialect::Cassandra && b.get(int) == Some(&b'.') && b.get(int + 1) == Some(&b'.') {
                    return if b.get(int + 2) == Some(&b'.') {
                        Ok((t::FLOAT, p + int + 1))
                    } else {
                        Ok((t::INTEGER, p + int))
                    };
                }
                let mut st = 0u8;
                let mut i = 0usize;
                while let Some(c) = at(i) {
                    match step_float(st, c) {
                        Some(n) => {
                            st = n;
                            i += c.len_utf8();
                        }
                        None => break,
                    }
                }
                if float_accepts(st) {
                    Ok((if st == 2 { t::INTEGER } else { t::FLOAT }, p + i))
                } else if st == 4 || st == 5 {
                    // EXPONENT: E ('+'|'-')? DIGIT+
                    Err((p + i, LexErrorKind::EarlyExit(at(i))))
                } else {
                    Err((p + i, LexErrorKind::MismatchedSet(at(i))))
                }
            }
            Rule::Duration => {
                let mut st = dur::START;
                let mut digits = 0u8;
                let mut i = 0usize;
                while let Some(c) = at(i) {
                    match duration_step_counted(st, c, &mut digits, self.dialect == Dialect::Scylla) {
                        Some(n) => {
                            st = n;
                            i += c.len_utf8();
                        }
                        None => break,
                    }
                    if st == dur::W || st == dur::TS || st == dur::ALT + dur::ALT_PATTERN.len() as u16 {
                        break;
                    }
                }
                if duration_accepts(st, self.dialect == Dialect::Scylla) {
                    Ok((t::DURATION, p + i))
                } else if st == dur::NEED_S {
                    // `us`, `µs`, `ns`: the `s` is a set (`'s'|'S'`).
                    Err((p + i, LexErrorKind::MismatchedSet(at(i))))
                } else {
                    Err((p + i, LexErrorKind::NoViable(at(i))))
                }
            }
            Rule::Ident => {
                let n = s.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
                let word = &s[..n];
                let ty = if word.eq_ignore_ascii_case("true") || word.eq_ignore_ascii_case("false") {
                    t::BOOLEAN
                } else {
                    t::keyword(word, self.dialect).unwrap_or(t::IDENT)
                };
                Ok((ty, p + n))
            }
        }
    }
}

/// Lexes `src`. With `keep_hidden`, whitespace and comment tokens are returned too.
pub(crate) fn lex(src: &str, dialect: Dialect, keep_hidden: bool) -> Lexed {
    let mut lx = Lexer { src, pos: 0, line: 1, col: 0, bcol: 0, dialect };
    let mut tokens = Vec::with_capacity(if keep_hidden { 16 } else { src.len() / 4 + 2 });
    let mut raw = 0u32;
    let mut error = None;
    while lx.pos < src.len() {
        match lx.next() {
            Ok((ty, end)) => {
                let tok = Token {
                    ty,
                    start: lx.pos as u32,
                    end: end as u32,
                    line: lx.line,
                    col: lx.col,
                    bcol: lx.bcol,
                    raw,
                };
                raw += 1;
                if keep_hidden || !tok.is_hidden() {
                    tokens.push(tok);
                }
                lx.advance_to(end);
            }
            Err((at, kind)) => {
                let at = at.min(src.len());
                let (line, col, bcol) = lx.position_of(at);
                error = Some(LexError { raw, line, col, bcol, kind });
                lx.advance_to(src.len());
                break;
            }
        }
    }
    tokens.push(Token {
        ty: t::EOF,
        start: src.len() as u32,
        end: src.len() as u32,
        line: lx.line,
        col: lx.col,
        bcol: lx.bcol,
        raw,
    });
    Lexed { tokens, error }
}

/// The text ANTLR gives a token: the unescaped content of strings and quoted names, the source
/// text otherwise.
pub(crate) fn token_text<'a>(src: &'a str, tok: &Token) -> std::borrow::Cow<'a, str> {
    use std::borrow::Cow;
    let raw = &src[tok.start as usize..tok.end as usize];
    match tok.ty {
        t::STRING_LITERAL if raw.starts_with('$') => Cow::Borrowed(&raw[2..raw.len() - 2]),
        t::STRING_LITERAL => unescape(&raw[1..raw.len() - 1], "''", "'"),
        t::QUOTED_NAME => unescape(&raw[1..raw.len() - 1], "\"\"", "\""),
        t::EOF => Cow::Borrowed("<EOF>"),
        _ => Cow::Borrowed(raw),
    }
}

fn unescape<'a>(s: &'a str, pat: &str, with: &str) -> std::borrow::Cow<'a, str> {
    if s.contains(pat) { std::borrow::Cow::Owned(s.replace(pat, with)) } else { std::borrow::Cow::Borrowed(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> String {
        let l = lex(src, Dialect::Cassandra, false);
        let mut out: Vec<String> = l
            .tokens
            .iter()
            .filter(|t| t.ty != t::EOF)
            .map(|tk| format!("{}[{}]", t::token_name(tk.ty), token_text(src, tk)))
            .collect();
        if let Some(e) = l.error {
            out.push(format!("!{}", e.message(Dialect::Cassandra)));
        }
        out.join(" ")
    }

    #[test]
    fn numbers_and_durations() {
        assert_eq!(kinds("1h30m"), "DURATION[1h30m]");
        assert_eq!(kinds("1..2"), "INTEGER[1] RANGE[..] INTEGER[2]");
        assert_eq!(kinds("1...2"), "FLOAT[1.] RANGE[..] INTEGER[2]");
        assert_eq!(kinds("1.5 1. 1e5 1E+5 1.e5"), "FLOAT[1.5] FLOAT[1.] FLOAT[1e5] FLOAT[1E+5] FLOAT[1.e5]");
        assert_eq!(kinds(".5"), "'.'[.] INTEGER[5]");
        assert_eq!(kinds("0x 0xFG 0x12ab"), "HEXNUMBER[0x] HEXNUMBER[0xF] IDENT[G] HEXNUMBER[0x12ab]");
        assert_eq!(
            kinds("-NaN -nan NaN Infinity -Infinity -1h - 1"),
            "K_NEGATIVE_NAN[-NaN] K_NEGATIVE_NAN[-nan] K_POSITIVE_NAN[NaN] K_POSITIVE_INFINITY[Infinity] \
             K_NEGATIVE_INFINITY[-Infinity] DURATION[-1h] '-'[-] INTEGER[1]"
        );
        assert_eq!(
            kinds("PT1H P1Y2M P1W P0001-02-03T04:05:06 PT P1Y2 1mo 1ms 1us 1µs 1ns"),
            "DURATION[PT1H] DURATION[P1Y2M] DURATION[P1W] DURATION[P0001-02-03T04:05:06] IDENT[PT] IDENT[P1Y2] \
             DURATION[1mo] DURATION[1ms] DURATION[1us] DURATION[1µs] DURATION[1ns]"
        );
        assert_eq!(kinds("P1D -P1D -PT1H 1Y 1y"), "DURATION[P1D] DURATION[-P1D] DURATION[-PT1H] DURATION[1Y] DURATION[1y]");
        assert_eq!(kinds("550e8400-e29b-41d4-a716-446655440000 550e8400"), "UUID[550e8400-e29b-41d4-a716-446655440000] FLOAT[550e8400]");
    }

    #[test]
    fn errors_like_antlr() {
        assert_eq!(kinds("1e "), "!line 1:2 no viable alternative at character ' '");
        assert_eq!(kinds("1days"), "!line 1:3 mismatched character 'y' expecting set null");
        assert_eq!(kinds("123abc "), "!line 1:6 mismatched character ' ' expecting set null");
        assert_eq!(kinds("'abc"), "!line 1:4 mismatched character '<EOF>' expecting '''");
        assert_eq!(kinds("a @"), "IDENT[a] !line 1:2 no viable alternative at character '@'");
        assert_eq!(kinds("a -- c"), "IDENT[a] !line 1:6 mismatched character '<EOF>' expecting set null");
        assert_eq!(kinds("/* x"), "!line 1:4 mismatched character '<EOF>' expecting '*'");
        assert_eq!(kinds("$$x"), "!line 1:2 mismatched character 'x' expecting '$'");
    }

    #[test]
    fn strings_names_and_punctuation() {
        assert_eq!(kinds("'a''b' $$ a $$ \"a\"\"b\" \"\""), "STRING_LITERAL[a'b] STRING_LITERAL[ a ] QUOTED_NAME[a\"b] EMPTY_QUOTED_NAME[\"\"]");
        assert_eq!(kinds("expr( expr ("), "'expr('[expr(] IDENT[expr] '('[(]");
        assert_eq!(kinds("a!=b a+=1 a-=1"), "IDENT[a] '!='[!=] IDENT[b] IDENT[a] '+='[+=] INTEGER[1] IDENT[a] '-='[-=] INTEGER[1]");
        assert_eq!(kinds("true FALSE nul null"), "BOOLEAN[true] BOOLEAN[FALSE] IDENT[nul] K_NULL[null]");
        assert_eq!(kinds("-- c\nfoo // d\nbar /* x */ baz"), "IDENT[foo] IDENT[bar] IDENT[baz]");
    }
}
