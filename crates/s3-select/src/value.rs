//! Values: S3 Select's data types plus MISSING, comparisons with S3 Select's (lenient) implicit
//! conversions, number parsing and text forms, and a bounded JSON parser and writer.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt::Write as _;

use crate::error::{Result, SelectError};
use crate::ts::Timestamp;

/// A value. CSV fields are strings; JSON gives every type. `Missing` is an absent attribute (it
/// is dropped from JSON output and empty in CSV output); `Null` is SQL NULL.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Missing,
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Timestamp(Timestamp),
    List(Vec<Value>),
    /// A JSON object, in document order.
    Struct(Vec<(String, Value)>),
}

impl Value {
    pub fn is_absent(&self) -> bool {
        matches!(self, Value::Missing | Value::Null)
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Value::Missing => "MISSING",
            Value::Null => "NULL",
            Value::Bool(_) => "BOOL",
            Value::Int(_) => "INT",
            Value::Float(_) => "FLOAT",
            Value::Str(_) => "STRING",
            Value::Timestamp(_) => "TIMESTAMP",
            Value::List(_) => "LIST",
            Value::Struct(_) => "STRUCT",
        }
    }

    /// The value's text, as CAST(x AS STRING) and CSV output give it (`None` for NULL/MISSING).
    pub fn text(&self) -> Option<Cow<'_, str>> {
        Some(match self {
            Value::Missing | Value::Null => return None,
            Value::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
            Value::Int(i) => Cow::Owned(i.to_string()),
            Value::Float(f) => Cow::Owned(fmt_float(*f)),
            Value::Str(s) => Cow::Borrowed(s.as_str()),
            Value::Timestamp(t) => Cow::Owned(t.ion_text()),
            Value::List(_) | Value::Struct(_) => {
                let mut s = String::new();
                write_json(&mut s, self);
                Cow::Owned(s)
            }
        })
    }
}

/// A number parsed from text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    pub(crate) fn value(self) -> Value {
        match self {
            Num::Int(i) => Value::Int(i),
            Num::Float(f) => Value::Float(f),
        }
    }
}

/// Parse a decimal number (`12`, `-3.5`, `+1e3`, `.5`), ignoring surrounding whitespace. No
/// `inf`/`nan`/hex. Integers beyond i64 become floats.
pub(crate) fn parse_num(s: &str) -> Option<Num> {
    let t = s.trim();
    let b = t.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i = 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - start;
    let mut float = false;
    if i < b.len() && b[i] == b'.' {
        float = true;
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        digits += i - f;
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        float = true;
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let e = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    if !float && let Ok(v) = t.parse::<i64>() {
        return Some(Num::Int(v));
    }
    t.parse::<f64>().ok().filter(|f| f.is_finite()).map(Num::Float)
}

pub(crate) fn parse_bool(s: &str) -> Option<bool> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("true") {
        Some(true)
    } else if t.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

/// A float's text: `3.0`, `0.25`, `1e20`, `1.5e-7`.
pub fn fmt_float(f: f64) -> String {
    if !f.is_finite() {
        return if f.is_nan() { "nan".into() } else if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let a = f.abs();
    if f == f.trunc() && a < 1e16 {
        format!("{f:.1}")
    } else if a >= 1e16 || a < 1e-5 {
        format!("{f:e}")
    } else {
        format!("{f}")
    }
}

fn cmp_int_float(i: i64, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f >= i64::MAX as f64 {
        return Some(Ordering::Less);
    }
    if f < i64::MIN as f64 {
        return Some(Ordering::Greater);
    }
    let t = f.trunc();
    match i.cmp(&(t as i64)) {
        Ordering::Equal => (0.0f64).partial_cmp(&(f - t)),
        o => Some(o),
    }
}

/// Order two values, converting where S3 Select does (and leniently where unambiguous): numbers
/// with numbers; a string with a number when the string is a number (CSV fields are strings); a
/// string with a bool or a timestamp when it parses as one. `None`: not comparable (or NULL).
pub(crate) fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => Some(x.cmp(y)),
        (Float(x), Float(y)) => x.partial_cmp(y),
        (Int(x), Float(y)) => cmp_int_float(*x, *y),
        (Float(x), Int(y)) => cmp_int_float(*y, *x).map(Ordering::reverse),
        (Str(x), Str(y)) => Some(x.as_str().cmp(y.as_str())),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Timestamp(x), Timestamp(y)) => Some(x.cmp_instant(y)),
        (Str(s), Int(_) | Float(_)) => parse_num(s).and_then(|n| compare(&n.value(), b)),
        (Int(_) | Float(_), Str(s)) => parse_num(s).and_then(|n| compare(a, &n.value())),
        (Str(s), Bool(y)) => parse_bool(s).map(|x| x.cmp(y)),
        (Bool(x), Str(s)) => parse_bool(s).map(|y| x.cmp(&y)),
        (Str(s), Timestamp(y)) => crate::ts::Timestamp::parse(s).map(|x| x.cmp_instant(y)),
        (Timestamp(x), Str(s)) => crate::ts::Timestamp::parse(s).map(|y| x.cmp_instant(&y)),
        _ => None,
    }
}

/// `=` for two non-NULL values: incomparable types are unequal.
pub(crate) fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::List(x), Value::List(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| equal(p, q)),
        (Value::Struct(x), Value::Struct(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|((k1, v1), (k2, v2))| k1 == k2 && equal(v1, v2))
        }
        _ => compare(a, b) == Some(Ordering::Equal),
    }
}

/// Equality without string conversions (NULLIF: `NULLIF(1, '1')` is 1).
pub(crate) fn strict_equal(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Str(_), Int(_) | Float(_) | Bool(_) | Timestamp(_)) | (Int(_) | Float(_) | Bool(_) | Timestamp(_), Str(_)) => false,
        _ => equal(a, b),
    }
}

// ---- JSON ----

/// Nesting limit for parsed JSON values (keeps recursion bounded).
pub const MAX_JSON_DEPTH: usize = 512;

/// Parse one JSON value (the whole input, surrounding whitespace allowed). The input is bounded by
/// the caller (a record is at most 1 MiB), and nesting by [`MAX_JSON_DEPTH`].
pub(crate) fn parse_json(input: &[u8]) -> Result<Value> {
    let mut p = JsonParser { s: input, i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return Err(p.err("unexpected data after a value"));
    }
    Ok(v)
}

/// Decode a JSON string token (with its quotes).
pub(crate) fn parse_json_string(input: &[u8]) -> Result<String> {
    let mut p = JsonParser { s: input, i: 0 };
    let s = p.string()?;
    if p.i != p.s.len() {
        return Err(p.err("unexpected data after a string"));
    }
    Ok(s)
}

struct JsonParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl JsonParser<'_> {
    fn err(&self, what: &str) -> SelectError {
        SelectError::json(format_args!("{what} at byte {} of the record", self.i))
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn lit(&mut self, word: &[u8], v: Value) -> Result<Value> {
        if self.s[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth >= MAX_JSON_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        match self.s.get(self.i) {
            None => Err(self.err("unexpected end of input")),
            Some(b'{') => {
                self.i += 1;
                let mut fields = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Value::Struct(fields));
                }
                loop {
                    self.ws();
                    if self.s.get(self.i) != Some(&b'"') {
                        return Err(self.err("expected a key"));
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.s.get(self.i) != Some(&b':') {
                        return Err(self.err("expected ':'"));
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    fields.push((k, v));
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Value::Struct(fields));
                        }
                        _ => return Err(self.err("expected ',' or '}'")),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Value::List(items));
                }
                loop {
                    self.ws();
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Value::List(items));
                        }
                        _ => return Err(self.err("expected ',' or ']'")),
                    }
                }
            }
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => self.lit(b"true", Value::Bool(true)),
            Some(b'f') => self.lit(b"false", Value::Bool(false)),
            Some(b'n') => self.lit(b"null", Value::Null),
            Some(b'-' | b'0'..=b'9') => {
                let start = self.i;
                if self.s[self.i] == b'-' {
                    self.i += 1;
                }
                let digits = self.i;
                while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                    self.i += 1;
                }
                if self.i == digits || (self.s[digits] == b'0' && self.i - digits > 1) {
                    return Err(self.err("invalid number"));
                }
                let mut float = false;
                if self.s.get(self.i) == Some(&b'.') {
                    float = true;
                    self.i += 1;
                    let f = self.i;
                    while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                        self.i += 1;
                    }
                    if self.i == f {
                        return Err(self.err("invalid number"));
                    }
                }
                if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
                    float = true;
                    self.i += 1;
                    if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                        self.i += 1;
                    }
                    let e = self.i;
                    while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                        self.i += 1;
                    }
                    if self.i == e {
                        return Err(self.err("invalid number"));
                    }
                }
                let text = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err("invalid number"))?;
                if !float && let Ok(v) = text.parse::<i64>() {
                    return Ok(Value::Int(v));
                }
                match text.parse::<f64>() {
                    Ok(f) if f.is_finite() => Ok(Value::Float(f)),
                    _ => Err(self.err("number out of range")),
                }
            }
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn hex4(&mut self) -> Result<u32> {
        let h = self.s.get(self.i..self.i + 4).ok_or_else(|| self.err("truncated \\u escape"))?;
        let t = std::str::from_utf8(h).map_err(|_| self.err("invalid \\u escape"))?;
        let v = u32::from_str_radix(t, 16).map_err(|_| self.err("invalid \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String> {
        debug_assert_eq!(self.s.get(self.i), Some(&b'"'));
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let run = self.i;
            while self.i < self.s.len() && !matches!(self.s[self.i], b'"' | b'\\') && self.s[self.i] >= 0x20 {
                self.i += 1;
            }
            out.extend_from_slice(&self.s[run..self.i]);
            match self.s.get(self.i) {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.i += 1;
                    return String::from_utf8(out).map_err(|_| SelectError::encoding());
                }
                Some(b'\\') => {
                    self.i += 1;
                    let e = *self.s.get(self.i).ok_or_else(|| self.err("unterminated string"))?;
                    self.i += 1;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi) {
                                if self.s.get(self.i..self.i + 2) != Some(b"\\u") {
                                    return Err(self.err("unpaired surrogate"));
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(self.err("unpaired surrogate"));
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            char::from_u32(code).ok_or_else(|| self.err("invalid \\u escape"))?
                        }
                        _ => return Err(self.err("invalid escape")),
                    };
                    let mut b = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
                }
                Some(_) => return Err(self.err("control character in a string")),
            }
        }
    }
}

/// Append `s` as a JSON string.
pub(crate) fn write_json_str(out: &mut String, s: &str) {
    out.push('"');
    let mut run = 0;
    for (i, c) in s.char_indices() {
        let esc = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            '\u{8}' => "\\b",
            '\u{c}' => "\\f",
            c if (c as u32) < 0x20 => "",
            _ => continue,
        };
        out.push_str(&s[run..i]);
        if esc.is_empty() {
            let _ = write!(out, "\\u{:04x}", c as u32);
        } else {
            out.push_str(esc);
        }
        run = i + c.len_utf8();
    }
    out.push_str(&s[run..]);
    out.push('"');
}

/// Append a value as JSON (MISSING and non-finite floats as `null`).
pub(crate) fn write_json(out: &mut String, v: &Value) {
    match v {
        Value::Missing | Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Float(f) if f.is_finite() => out.push_str(&fmt_float(*f)),
        Value::Float(_) => out.push_str("null"),
        Value::Str(s) => write_json_str(out, s),
        Value::Timestamp(t) => write_json_str(out, &t.ion_text()),
        Value::List(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(out, x);
            }
            out.push(']');
        }
        Value::Struct(fields) => {
            out.push('{');
            let mut first = true;
            for (k, x) in fields {
                if matches!(x, Value::Missing) {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                write_json_str(out, k);
                out.push(':');
                write_json(out, x);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(parse_num("12"), Some(Num::Int(12)));
        assert_eq!(parse_num(" -3 "), Some(Num::Int(-3)));
        assert_eq!(parse_num("+00"), Some(Num::Int(0)));
        assert_eq!(parse_num("1.5"), Some(Num::Float(1.5)));
        assert_eq!(parse_num(".5"), Some(Num::Float(0.5)));
        assert_eq!(parse_num("1e3"), Some(Num::Float(1000.0)));
        assert_eq!(parse_num("99999999999999999999"), Some(Num::Float(1e20)));
        for bad in ["", "-", ".", "1e", "abc", "inf", "nan", "1,5", "0x10", "1 2"] {
            assert_eq!(parse_num(bad), None, "{bad}");
        }
    }

    #[test]
    fn float_text() {
        assert_eq!(fmt_float(3.0), "3.0");
        assert_eq!(fmt_float(-0.25), "-0.25");
        assert_eq!(fmt_float(1e20), "1e20");
        assert_eq!(fmt_float(1.5e-7), "1.5e-7");
        assert_eq!(fmt_float(504.3), "504.3");
    }

    #[test]
    fn comparisons_convert_csv_strings() {
        let s = |x: &str| Value::Str(x.into());
        assert_eq!(compare(&s("10"), &Value::Int(9)), Some(Ordering::Greater));
        assert_eq!(compare(&s("10"), &s("9")), Some(Ordering::Less));
        assert_eq!(compare(&s("2.5"), &Value::Int(2)), Some(Ordering::Greater));
        assert_eq!(compare(&s("abc"), &Value::Int(2)), None);
        assert_eq!(compare(&Value::Int(2), &Value::Float(2.0)), Some(Ordering::Equal));
        assert_eq!(compare(&Value::Int(i64::MAX), &Value::Float(1e19)), Some(Ordering::Less));
        assert_eq!(compare(&s("TRUE"), &Value::Bool(true)), Some(Ordering::Equal));
        assert!(equal(&s("5"), &Value::Int(5)));
        assert!(!strict_equal(&s("5"), &Value::Int(5)));
        assert!(strict_equal(&Value::Float(1.0), &Value::Int(1)));
        assert!(!equal(&Value::Bool(true), &Value::Int(1)));
    }

    #[test]
    fn json_roundtrip() {
        let v = parse_json(br#" {"b": [1, -2.5, "x\u00e9\n", true, null, {}], "a": {"z": 1e2}} "#).unwrap();
        let mut s = String::new();
        write_json(&mut s, &v);
        assert_eq!(s, r#"{"b":[1,-2.5,"xé\n",true,null,{}],"a":{"z":100.0}}"#);
        assert_eq!(parse_json_string(br#""\ud83d\ude00""#).unwrap(), "😀");
        for bad in [&b"{"[..], b"[1,]", b"{\"a\" 1}", b"01", b"1 2", b"\"\x01\"", b"tru", b"\"\\ud800\""] {
            assert_eq!(parse_json(bad).unwrap_err().code, "JSONParsingError", "{bad:?}");
        }
        let deep = "[".repeat(MAX_JSON_DEPTH + 1);
        assert_eq!(parse_json(deep.as_bytes()).unwrap_err().code, "JSONParsingError");
        let mut s = String::new();
        write_json_str(&mut s, "a\"\\\u{1}");
        assert_eq!(s, r#""a\"\\\u0001""#);
    }
}
