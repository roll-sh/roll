//! JSON as Alternator reads and writes it.
//!
//! ScyllaDB parses request bodies with RapidJSON, so a syntax error is reported as
//! `Parsing JSON failed: <RapidJSON message> at <byte offset>` and the offset is where RapidJSON
//! noticed the problem. [`parse`] reproduces those messages and offsets.
//!
//! The tree keeps what Alternator's behaviour depends on: object members in document order
//! (duplicates included; lookups find the first), and numbers as their source text plus the
//! RapidJSON classification (`IsInt`, `IsUint64`, `IsDouble`, ...). Parsing is iterative with a
//! nesting limit, so a deeply nested body costs neither stack nor unbounded memory.
//!
//! [`Json::write`] produces RapidJSON `Writer` output: compact, control characters escaped as
//! `\b \t \n \f \r` or `\u00XX`, everything else (including non-ASCII) verbatim.

use std::fmt::Write as _;

/// A JSON value. Objects keep member order and duplicates.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// A JSON number: its text and how RapidJSON classified it.
#[derive(Clone, Debug, PartialEq)]
pub struct Number {
    text: Box<str>,
    kind: NumKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum NumKind {
    /// No fraction or exponent, fits `i64` (negative) or `u64`.
    Int(i64),
    Uint(u64),
    Double(f64),
}

impl Number {
    pub fn from_u64(v: u64) -> Number {
        Number {
            text: v.to_string().into(),
            kind: NumKind::Uint(v),
        }
    }

    pub fn from_i64(v: i64) -> Number {
        if v >= 0 {
            return Number::from_u64(v as u64);
        }
        Number {
            text: v.to_string().into(),
            kind: NumKind::Int(v),
        }
    }

    /// A double, written as RapidJSON's `Writer::Double` does.
    pub fn from_f64(v: f64) -> Number {
        Number {
            text: format_double(v).into(),
            kind: NumKind::Double(v),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// RapidJSON `IsInt64`.
    pub fn as_i64(&self) -> Option<i64> {
        match self.kind {
            NumKind::Int(v) => Some(v),
            NumKind::Uint(v) => i64::try_from(v).ok(),
            NumKind::Double(_) => None,
        }
    }

    /// RapidJSON `IsUint64`.
    pub fn as_u64(&self) -> Option<u64> {
        match self.kind {
            NumKind::Uint(v) => Some(v),
            _ => None,
        }
    }

    /// RapidJSON `IsInt`.
    pub fn as_i32(&self) -> Option<i32> {
        self.as_i64().and_then(|v| i32::try_from(v).ok())
    }

    /// `GetDouble` (integers convert).
    pub fn as_f64(&self) -> f64 {
        match self.kind {
            NumKind::Int(v) => v as f64,
            NumKind::Uint(v) => v as f64,
            NumKind::Double(v) => v,
        }
    }

    pub fn is_double(&self) -> bool {
        matches!(self.kind, NumKind::Double(_))
    }
}

impl Json {
    pub fn str(s: impl Into<String>) -> Json {
        Json::String(s.into())
    }

    pub fn u64(v: u64) -> Json {
        Json::Number(Number::from_u64(v))
    }

    pub fn i64(v: i64) -> Json {
        Json::Number(Number::from_i64(v))
    }

    pub fn f64(v: f64) -> Json {
        Json::Number(Number::from_f64(v))
    }

    pub fn object() -> Json {
        Json::Object(Vec::new())
    }

    /// The first member called `key`, if this is an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Object(m) => m.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Appends a member (objects only; no-op otherwise).
    pub fn push(&mut self, key: impl Into<String>, value: Json) {
        if let Json::Object(m) = self {
            m.push((key.into(), value));
        }
    }

    /// Builder form of [`push`](Self::push).
    pub fn with(mut self, key: impl Into<String>, value: Json) -> Json {
        self.push(key, value);
        self
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<&Number> {
        match self {
            Json::Number(n) => Some(n),
            _ => None,
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Json::Object(_))
    }

    /// Compact serialisation.
    pub fn write(&self, out: &mut String) {
        // Iterative to keep deep (but already validated) documents off the stack.
        enum Step<'a> {
            Value(&'a Json),
            Raw(&'static str),
            Key(&'a str),
        }
        let mut stack = vec![Step::Value(self)];
        while let Some(step) = stack.pop() {
            match step {
                Step::Raw(s) => out.push_str(s),
                Step::Key(k) => {
                    write_string(out, k);
                    out.push(':');
                }
                Step::Value(v) => match v {
                    Json::Null => out.push_str("null"),
                    Json::Bool(true) => out.push_str("true"),
                    Json::Bool(false) => out.push_str("false"),
                    Json::Number(n) => match n.kind {
                        // RapidJSON re-prints parsed numbers from their value.
                        NumKind::Int(v) => {
                            let _ = write!(out, "{v}");
                        }
                        NumKind::Uint(v) => {
                            let _ = write!(out, "{v}");
                        }
                        NumKind::Double(v) => out.push_str(&format_double(v)),
                    },
                    Json::String(s) => write_string(out, s),
                    Json::Array(a) => {
                        out.push('[');
                        stack.push(Step::Raw("]"));
                        for (i, x) in a.iter().enumerate().rev() {
                            stack.push(Step::Value(x));
                            if i > 0 {
                                stack.push(Step::Raw(","));
                            }
                        }
                    }
                    Json::Object(m) => {
                        out.push('{');
                        stack.push(Step::Raw("}"));
                        for (i, (k, x)) in m.iter().enumerate().rev() {
                            stack.push(Step::Value(x));
                            stack.push(Step::Key(k));
                            if i > 0 {
                                stack.push(Step::Raw(","));
                            }
                        }
                    }
                },
            }
        }
    }

    pub fn to_json_string(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::String(s.to_string())
    }
}

impl From<String> for Json {
    fn from(s: String) -> Json {
        Json::String(s)
    }
}

impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}

/// RapidJSON's `Writer::Double`: the shortest round-trip digits, laid out by its `Prettify`
/// (`100.0`, `0.001`, `1e21`, `1.5e-7`).
pub fn format_double(v: f64) -> String {
    if !v.is_finite() {
        return "0.0".into();
    }
    let mut out = String::new();
    if v.is_sign_negative() {
        out.push('-');
    }
    let a = v.abs();
    if a == 0.0 {
        out.push_str("0.0");
        return out;
    }
    // `{:e}` gives the shortest digits as `d.ddde±x`.
    let sci = format!("{a:e}");
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let exp: i32 = exp.parse().unwrap_or(0);
    let length = digits.len() as i32;
    // value = digits × 10^k
    let k = exp - (length - 1);
    let kk = length + k;
    if 0 <= k && kk <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', k as usize));
        out.push_str(".0");
    } else if 0 < kk && kk <= 21 {
        out.push_str(&digits[..kk as usize]);
        out.push('.');
        out.push_str(&digits[kk as usize..]);
    } else if -6 < kk && kk <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-kk) as usize));
        out.push_str(&digits);
    } else if length == 1 {
        out.push_str(&digits);
        let _ = write!(out, "e{}", kk - 1);
    } else {
        out.push_str(&digits[..1]);
        out.push('.');
        out.push_str(&digits[1..]);
        let _ = write!(out, "e{}", kk - 1);
    }
    out
}

/// Writes `s` as a JSON string literal, escaping as RapidJSON's `Writer` does.
pub fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0a}' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{0d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A RapidJSON parse error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub message: &'static str,
    pub offset: usize,
    /// The document nests deeper than [`MAX_DEPTH`] (ScyllaDB reports this as
    /// `Max nested level reached: 78`, without the parse-error wrapping).
    pub too_deep: bool,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at {}", self.message, self.offset)
    }
}

const EMPTY: &str = "The document is empty.";
const NOT_SINGULAR: &str = "The document root must not be followed by other values.";
const INVALID_VALUE: &str = "Invalid value.";
const MISS_NAME: &str = "Missing a name for object member.";
const MISS_COLON: &str = "Missing a colon after a name of object member.";
const MISS_COMMA_CURLY: &str = "Missing a comma or '}' after an object member.";
const MISS_COMMA_SQUARE: &str = "Missing a comma or ']' after an array element.";
const BAD_HEX: &str = "Incorrect hex digit after \\u escape in string.";
const BAD_SURROGATE: &str = "The surrogate pair in string is invalid.";
const BAD_ESCAPE: &str = "Invalid escape character in string.";
const MISS_QUOTE: &str = "Missing a closing quotation mark in string.";
const BAD_ENCODING: &str = "Invalid encoding in string.";
const TOO_BIG: &str = "Number too big to be stored in double.";
const MISS_FRACTION: &str = "Miss fraction part in number.";
const MISS_EXPONENT: &str = "Miss exponent in number.";

/// Deepest nesting of objects and arrays ScyllaDB accepts in a request.
pub const MAX_DEPTH: usize = 78;

/// Parses a request body as RapidJSON's `Parse<kParseDefaultFlags>` does.
///
/// RapidJSON reads a NUL byte as the end of input, so a body with an embedded NUL fails exactly
/// where RapidJSON's would.
pub fn parse(input: &[u8]) -> Result<Json, ParseError> {
    let mut p = Parser { s: input, pos: 0 };
    p.skip_ws();
    if p.peek() == 0 {
        return Err(p.err(EMPTY));
    }
    let v = p.value()?;
    p.skip_ws();
    if p.peek() != 0 {
        return Err(p.err(NOT_SINGULAR));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

enum Frame {
    Array(Vec<Json>),
    Object(Vec<(String, Json)>, String),
}

impl Parser<'_> {
    fn peek(&self) -> u8 {
        self.s.get(self.pos).copied().unwrap_or(0)
    }

    fn take(&mut self) -> u8 {
        let c = self.peek();
        if self.pos < self.s.len() {
            self.pos += 1;
        }
        c
    }

    fn consume(&mut self, c: u8) -> bool {
        if self.peek() == c && c != 0 {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn err(&self, message: &'static str) -> ParseError {
        ParseError {
            message,
            offset: self.pos,
            too_deep: false,
        }
    }

    fn err_at(&self, message: &'static str, offset: usize) -> ParseError {
        ParseError {
            message,
            offset,
            too_deep: false,
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), b' ' | b'\n' | b'\r' | b'\t') {
            self.pos += 1;
        }
    }

    /// Parses one value; containers are handled with an explicit stack.
    fn value(&mut self) -> Result<Json, ParseError> {
        let mut stack: Vec<Frame> = Vec::new();
        loop {
            // Parse the start of a value.
            if matches!(self.peek(), b'{' | b'[') && stack.len() + 1 > MAX_DEPTH {
                return Err(ParseError {
                    too_deep: true,
                    ..self.err(INVALID_VALUE)
                });
            }
            let mut done = match self.peek() {
                b'{' => {
                    self.take();
                    self.skip_ws();
                    if self.consume(b'}') {
                        Some(Json::Object(Vec::new()))
                    } else {
                        let key = self.member_name()?;
                        stack.push(Frame::Object(Vec::new(), key));
                        None
                    }
                }
                b'[' => {
                    self.take();
                    self.skip_ws();
                    if self.consume(b']') {
                        Some(Json::Array(Vec::new()))
                    } else {
                        stack.push(Frame::Array(Vec::new()));
                        None
                    }
                }
                _ => Some(self.scalar()?),
            };
            // Fold finished values into their containers.
            while let Some(v) = done.take() {
                let Some(top) = stack.last_mut() else {
                    return Ok(v);
                };
                self.skip_ws();
                match top {
                    Frame::Array(items) => {
                        items.push(v);
                        match self.peek() {
                            b',' => {
                                self.take();
                                self.skip_ws();
                            }
                            b']' => {
                                self.take();
                                let Some(Frame::Array(items)) = stack.pop() else {
                                    unreachable!()
                                };
                                done = Some(Json::Array(items));
                            }
                            _ => return Err(self.err(MISS_COMMA_SQUARE)),
                        }
                    }
                    Frame::Object(members, key) => {
                        members.push((std::mem::take(key), v));
                        match self.peek() {
                            b',' => {
                                self.take();
                                self.skip_ws();
                                *key = self.member_name()?;
                            }
                            b'}' => {
                                self.take();
                                let Some(Frame::Object(m, _)) = stack.pop() else {
                                    unreachable!()
                                };
                                done = Some(Json::Object(m));
                            }
                            _ => return Err(self.err(MISS_COMMA_CURLY)),
                        }
                    }
                }
            }
        }
    }

    /// `"name" :` followed by whitespace, leaving the parser at the member's value.
    fn member_name(&mut self) -> Result<String, ParseError> {
        if self.peek() != b'"' {
            return Err(self.err(MISS_NAME));
        }
        let key = self.string()?;
        self.skip_ws();
        if !self.consume(b':') {
            return Err(self.err(MISS_COLON));
        }
        self.skip_ws();
        Ok(key)
    }

    fn scalar(&mut self) -> Result<Json, ParseError> {
        match self.peek() {
            b'n' => self.literal(b"ull", Json::Null),
            b't' => self.literal(b"rue", Json::Bool(true)),
            b'f' => self.literal(b"alse", Json::Bool(false)),
            b'"' => Ok(Json::String(self.string()?)),
            _ => self.number(),
        }
    }

    fn literal(&mut self, rest: &[u8], v: Json) -> Result<Json, ParseError> {
        self.take();
        for &c in rest {
            if !self.consume(c) {
                return Err(self.err(INVALID_VALUE));
            }
        }
        Ok(v)
    }

    fn hex4(&mut self, escape_at: usize) -> Result<u32, ParseError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.peek();
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(self.err_at(BAD_HEX, escape_at)),
            };
            v = (v << 4) | d as u32;
            self.take();
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.take(); // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = self.peek();
            match c {
                b'\\' => {
                    let escape_at = self.pos;
                    self.take();
                    let e = self.peek();
                    let simple = match e {
                        b'"' => Some(b'"'),
                        b'\\' => Some(b'\\'),
                        b'/' => Some(b'/'),
                        b'b' => Some(0x08),
                        b'f' => Some(0x0c),
                        b'n' => Some(b'\n'),
                        b'r' => Some(b'\r'),
                        b't' => Some(b'\t'),
                        _ => None,
                    };
                    if let Some(b) = simple {
                        self.take();
                        out.push(b);
                    } else if e == b'u' {
                        self.take();
                        let mut cp = self.hex4(escape_at)?;
                        if (0xD800..=0xDBFF).contains(&cp) {
                            if !(self.consume(b'\\') && self.consume(b'u')) {
                                return Err(self.err_at(BAD_SURROGATE, escape_at));
                            }
                            let lo = self.hex4(escape_at)?;
                            if !(0xDC00..=0xDFFF).contains(&lo) {
                                return Err(self.err_at(BAD_SURROGATE, escape_at));
                            }
                            cp = (((cp - 0xD800) << 10) | (lo - 0xDC00)) + 0x10000;
                        } else if (0xDC00..=0xDFFF).contains(&cp) {
                            return Err(self.err_at(BAD_SURROGATE, escape_at));
                        }
                        let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    } else {
                        return Err(self.err_at(BAD_ESCAPE, escape_at));
                    }
                }
                b'"' => {
                    self.take();
                    break;
                }
                0 => return Err(self.err(MISS_QUOTE)),
                c if c < 0x20 => return Err(self.err(BAD_ENCODING)),
                _ => {
                    // Copy a run of plain bytes at once.
                    let start = self.pos;
                    while let Some(&b) = self.s.get(self.pos) {
                        if b == b'"' || b == b'\\' || b < 0x20 {
                            break;
                        }
                        self.pos += 1;
                    }
                    out.extend_from_slice(&self.s[start..self.pos]);
                }
            }
        }
        // RapidJSON does not validate UTF-8 by default; Rust strings must be valid, so invalid
        // sequences become U+FFFD (Alternator rejects or stores such bytes opaquely either way).
        Ok(match String::from_utf8(out) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        })
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.pos;
        let minus = self.consume(b'-');
        let mut big = false; // integer part beyond u64: accumulated as a double, like RapidJSON
        let mut acc: u64 = 0;
        let mut d = 0f64;
        match self.peek() {
            b'0' => {
                self.take();
            }
            b'1'..=b'9' => {
                while let c @ b'0'..=b'9' = self.peek() {
                    let digit = (c - b'0') as u64;
                    if !big {
                        match acc.checked_mul(10).and_then(|v| v.checked_add(digit)) {
                            Some(v) => acc = v,
                            None => {
                                big = true;
                                d = acc as f64;
                            }
                        }
                    }
                    if big {
                        if d >= 1.797_693_134_862_315_7e307 {
                            return Err(self.err_at(TOO_BIG, start));
                        }
                        d = d * 10.0 + digit as f64;
                    }
                    self.take();
                }
            }
            _ => return Err(self.err(INVALID_VALUE)),
        }
        let mut is_double = big;
        let mut frac_digits: i64 = 0;
        if self.consume(b'.') {
            if !self.peek().is_ascii_digit() {
                return Err(self.err(MISS_FRACTION));
            }
            while self.peek().is_ascii_digit() {
                self.take();
                frac_digits += 1;
            }
            is_double = true;
        }
        if self.consume(b'e') || self.consume(b'E') {
            is_double = true;
            let exp_minus = self.consume(b'-');
            if !exp_minus {
                self.consume(b'+');
            }
            if !self.peek().is_ascii_digit() {
                return Err(self.err(MISS_EXPONENT));
            }
            let mut exp: i64 = 0;
            while let c @ b'0'..=b'9' = self.peek() {
                if exp < 1_000_000_000 {
                    exp = exp * 10 + (c - b'0') as i64;
                }
                self.take();
                if !exp_minus && exp > 308 + frac_digits.min(400) {
                    return Err(self.err_at(TOO_BIG, start));
                }
            }
        }
        let text = std::str::from_utf8(&self.s[start..self.pos]).unwrap_or("0");
        let kind = if is_double {
            let v: f64 = text.parse().unwrap_or(0.0);
            if !v.is_finite() {
                return Err(self.err_at(TOO_BIG, start));
            }
            NumKind::Double(v)
        } else if minus {
            if acc <= i64::MAX as u64 + 1 {
                NumKind::Int((acc as i128).wrapping_neg() as i64)
            } else {
                NumKind::Double(-(acc as f64))
            }
        } else {
            NumKind::Uint(acc)
        };
        Ok(Json::Number(Number {
            text: text.into(),
            kind,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(s: &str) -> String {
        parse(s.as_bytes()).unwrap_err().to_string()
    }

    #[test]
    fn rapidjson_errors() {
        assert_eq!(err(""), "The document is empty. at 0");
        assert_eq!(err("  "), "The document is empty. at 2");
        assert_eq!(err("bad json"), "Invalid value. at 0");
        assert_eq!(
            err("{} x"),
            "The document root must not be followed by other values. at 3"
        );
        assert_eq!(
            err("{\"a\" 1}"),
            "Missing a colon after a name of object member. at 5"
        );
        assert_eq!(err("{1}"), "Missing a name for object member. at 1");
        assert_eq!(
            err("{\"a\":1 \"b\":2}"),
            "Missing a comma or '}' after an object member. at 7"
        );
        assert_eq!(
            err("[1 2]"),
            "Missing a comma or ']' after an array element. at 3"
        );
        assert_eq!(err("\"\\x\""), "Invalid escape character in string. at 1");
        assert_eq!(
            err("\"\\u12g4\""),
            "Incorrect hex digit after \\u escape in string. at 1"
        );
        assert_eq!(
            err("\"\\ud800\""),
            "The surrogate pair in string is invalid. at 1"
        );
        assert_eq!(
            err("\"abc"),
            "Missing a closing quotation mark in string. at 4"
        );
        assert_eq!(err("\"a\tb\""), "Invalid encoding in string. at 2");
        assert_eq!(err("1."), "Miss fraction part in number. at 2");
        assert_eq!(err("1e"), "Miss exponent in number. at 2");
        assert_eq!(err("1e400"), "Number too big to be stored in double. at 0");
        assert_eq!(err("nul"), "Invalid value. at 3");
        assert_eq!(err("{\"a\":1,}"), "Missing a name for object member. at 7");
        assert_eq!(err("[1,]"), "Invalid value. at 3");
        // A NUL byte reads as the end of input.
        assert!(parse(b"{\"a\":1}\0x").is_ok());
    }

    #[test]
    fn values_and_numbers() {
        let v = parse(br#"{"a":[1,-2,3.5,1e2,18446744073709551615,-9223372036854775808],"b":"\u00e9\ud83d\ude00","a":null}"#).unwrap();
        assert_eq!(v.get("b").unwrap().as_str(), Some("é😀"));
        let a = v.get("a").unwrap().as_array().unwrap();
        assert_eq!(a[0].as_number().unwrap().as_i32(), Some(1));
        assert_eq!(a[1].as_number().unwrap().as_i64(), Some(-2));
        assert!(a[2].as_number().unwrap().is_double());
        assert!(a[3].as_number().unwrap().is_double());
        assert_eq!(a[4].as_number().unwrap().as_u64(), Some(u64::MAX));
        assert_eq!(a[5].as_number().unwrap().as_i64(), Some(i64::MIN));
        // First duplicate wins on lookup; both are kept.
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn nesting_limit() {
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(ok.as_bytes()).is_ok());
        let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        assert!(parse(deep.as_bytes()).unwrap_err().too_deep);
        let huge = "[".repeat(1_000_000);
        assert!(parse(huge.as_bytes()).unwrap_err().too_deep);
    }

    #[test]
    fn doubles_print_like_rapidjson() {
        let f = format_double;
        assert_eq!(f(100.0), "100.0");
        assert_eq!(f(0.1), "0.1");
        assert_eq!(f(1.5), "1.5");
        assert_eq!(f(1e21), "1e21");
        assert_eq!(f(1e20), "100000000000000000000.0");
        assert_eq!(f(1e-6), "0.000001");
        assert_eq!(f(1e-7), "1e-7");
        assert_eq!(f(1.5e-7), "1.5e-7");
        assert_eq!(f(-0.0), "-0.0");
        assert_eq!(f(0.0), "0.0");
        assert_eq!(f(3.4028234663852886e38), "3.4028234663852886e38");
        assert_eq!(f(123.456), "123.456");
        let v = parse(b"[-0, 1.0, 1e2, -0.0]").unwrap();
        assert_eq!(v.to_json_string(), "[0,1.0,100.0,-0.0]");
    }

    #[test]
    fn writer_escapes_like_rapidjson() {
        let v = Json::object()
            .with("s", Json::str("a\"b\\c\n\u{1}é/"))
            .with("n", Json::f64(1.0))
            .with(
                "l",
                Json::Array(vec![Json::Null, Json::Bool(true), Json::u64(3)]),
            );
        assert_eq!(
            v.to_json_string(),
            r#"{"s":"a\"b\\c\n\u0001é/","n":1.0,"l":[null,true,3]}"#
        );
        let round = parse(v.to_json_string().as_bytes()).unwrap();
        assert_eq!(round.get("s"), v.get("s"));
    }
}
