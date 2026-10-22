//! Ordered JSON values for aggregation requests and responses.
//!
//! Elasticsearch parses aggregation definitions token by token in document order (the first
//! error wins, sibling aggregations come back in request order) and prints responses in a fixed
//! field order with Java's number formatting (`1.0E10`, `Infinity`). `serde_json::Value` loses
//! both (its maps sort keys unless a crate-wide feature is enabled), so requests and responses
//! use this small ordered value type instead. Objects are vectors of pairs: aggregation objects
//! are small and lookups are rare compared to walks.
//!
//! Numbers keep Jackson's integer/floating distinction. `Float32` prints like Java's
//! `Float.toString` (scores, `float` fields); `Float` like `Double.toString`.

use std::fmt::Write as _;

/// A JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    /// Integers above `i64::MAX`.
    UInt(u64),
    Float(f64),
    /// A Java `float` (printed with `Float.toString`).
    Float32(f32),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj() -> Json {
        Json::Obj(Vec::new())
    }

    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    /// The value of `key` in an object (the last one, as Jackson maps keep the last duplicate).
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Obj(m) => m.iter_mut().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Appends `key: value` to an object (no-op on other values).
    pub fn push(&mut self, key: impl Into<String>, value: impl Into<Json>) {
        if let Json::Obj(m) = self {
            m.push((key.into(), value.into()));
        }
    }

    /// Builder form of [`push`](Self::push).
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Json>) -> Json {
        self.push(key, value);
        self
    }

    /// Sets `key` (replacing an existing entry in place).
    pub fn set(&mut self, key: &str, value: impl Into<Json>) {
        if let Json::Obj(m) = self {
            let value = value.into();
            if let Some(e) = m.iter_mut().find(|(k, _)| k == key) {
                e.1 = value;
            } else {
                m.push((key.to_string(), value));
            }
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Int(i) => Some(*i as f64),
            Json::UInt(u) => Some(*u as f64),
            Json::Float(f) => Some(*f),
            Json::Float32(f) => Some(*f as f64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(i) => Some(*i),
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
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(m) => Some(m),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn is_number(&self) -> bool {
        matches!(self, Json::Int(_) | Json::UInt(_) | Json::Float(_) | Json::Float32(_))
    }

    /// A double as Elasticsearch renders a metric value: `null` for infinities and NaN is the
    /// caller's choice, this keeps the number.
    pub fn double(d: f64) -> Json {
        Json::Float(d)
    }

    /// Compact JSON text with Java number formatting.
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    /// Appends compact JSON text.
    pub fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Json::UInt(u) => {
                let _ = write!(out, "{u}");
            }
            Json::Float(f) => {
                if f.is_finite() {
                    out.push_str(&java_double(*f));
                } else {
                    // Jackson writes non-finite doubles as quoted strings.
                    out.push('"');
                    out.push_str(&java_double(*f));
                    out.push('"');
                }
            }
            Json::Float32(f) => {
                if f.is_finite() {
                    out.push_str(&java_float(*f));
                } else {
                    out.push('"');
                    out.push_str(&java_float(*f));
                    out.push('"');
                }
            }
            Json::Str(s) => write_str(out, s),
            Json::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Json::Obj(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(out, k);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    /// Converts from `serde_json` (object order is whatever `serde_json` kept).
    pub fn from_serde(v: &serde_json::Value) -> Json {
        match v {
            serde_json::Value::Null => Json::Null,
            serde_json::Value::Bool(b) => Json::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Json::Int(i)
                } else if let Some(u) = n.as_u64() {
                    Json::UInt(u)
                } else {
                    Json::Float(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            serde_json::Value::String(s) => Json::Str(s.clone()),
            serde_json::Value::Array(a) => Json::Arr(a.iter().map(Json::from_serde).collect()),
            serde_json::Value::Object(m) => Json::Obj(m.iter().map(|(k, v)| (k.clone(), Json::from_serde(v))).collect()),
        }
    }

    /// Converts to `serde_json` (non-finite doubles become strings, as Jackson writes them).
    pub fn to_serde(&self) -> serde_json::Value {
        match self {
            Json::Null => serde_json::Value::Null,
            Json::Bool(b) => serde_json::Value::Bool(*b),
            Json::Int(i) => serde_json::Value::from(*i),
            Json::UInt(u) => serde_json::Value::from(*u),
            Json::Float(f) => match serde_json::Number::from_f64(*f) {
                Some(n) => serde_json::Value::Number(n),
                None => serde_json::Value::String(java_double(*f)),
            },
            Json::Float32(f) => {
                // Float.toString's digits, read back as a double, as Jackson renders a float.
                let d: f64 = java_float(*f).parse().unwrap_or(*f as f64);
                match serde_json::Number::from_f64(d) {
                    Some(n) => serde_json::Value::Number(n),
                    None => serde_json::Value::String(java_float(*f)),
                }
            }
            Json::Str(s) => serde_json::Value::String(s.clone()),
            Json::Arr(a) => serde_json::Value::Array(a.iter().map(Json::to_serde).collect()),
            Json::Obj(m) => {
                let mut map = serde_json::Map::new();
                for (k, v) in m {
                    map.insert(k.clone(), v.to_serde());
                }
                serde_json::Value::Object(map)
            }
        }
    }

    /// Parses JSON text (strict RFC 8259 plus Jackson's leniencies that matter here: none).
    /// Object key order is kept; duplicate keys are kept as written.
    pub fn parse(text: &str) -> Result<Json, String> {
        let mut p = Parser { s: text.as_bytes(), pos: 0, depth: 0 };
        p.ws();
        let v = p.value()?;
        p.ws();
        if p.pos != p.s.len() {
            return Err(format!("trailing characters at {}", p.pos));
        }
        Ok(v)
    }

    /// A short name of the value's JSON type.
    pub fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "boolean",
            Json::Int(_) | Json::UInt(_) | Json::Float(_) | Json::Float32(_) => "number",
            Json::Str(_) => "string",
            Json::Arr(_) => "array",
            Json::Obj(_) => "object",
        }
    }

    /// The text Elasticsearch's `Object.toString()` gives a parsed value (map/list form for
    /// containers, as Java collections print).
    pub fn to_java_string(&self) -> String {
        match self {
            Json::Null => "null".into(),
            Json::Bool(b) => b.to_string(),
            Json::Int(i) => i.to_string(),
            Json::UInt(u) => u.to_string(),
            Json::Float(f) => java_double(*f),
            Json::Float32(f) => java_float(*f),
            Json::Str(s) => s.clone(),
            Json::Arr(a) => format!("[{}]", a.iter().map(Json::to_java_string).collect::<Vec<_>>().join(", ")),
            Json::Obj(m) => format!("{{{}}}", m.iter().map(|(k, v)| format!("{k}={}", v.to_java_string())).collect::<Vec<_>>().join(", ")),
        }
    }
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_json_string())
    }
}

impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}
impl From<i64> for Json {
    fn from(i: i64) -> Json {
        Json::Int(i)
    }
}
impl From<i32> for Json {
    fn from(i: i32) -> Json {
        Json::Int(i as i64)
    }
}
impl From<u32> for Json {
    fn from(i: u32) -> Json {
        Json::Int(i as i64)
    }
}
impl From<u64> for Json {
    fn from(u: u64) -> Json {
        if u <= i64::MAX as u64 { Json::Int(u as i64) } else { Json::UInt(u) }
    }
}
impl From<usize> for Json {
    fn from(u: usize) -> Json {
        Json::from(u as u64)
    }
}
impl From<f64> for Json {
    fn from(f: f64) -> Json {
        Json::Float(f)
    }
}
impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::Str(s.to_string())
    }
}
impl From<String> for Json {
    fn from(s: String) -> Json {
        Json::Str(s)
    }
}
impl From<Vec<Json>> for Json {
    fn from(a: Vec<Json>) -> Json {
        Json::Arr(a)
    }
}
impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(o: Option<T>) -> Json {
        match o {
            Some(v) => v.into(),
            None => Json::Null,
        }
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.pos < self.s.len() && matches!(self.s[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn err<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("{what} at {}", self.pos))
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.s.get(self.pos) {
            None => self.err("unexpected end"),
            Some(b'{') => {
                self.depth += 1;
                if self.depth > 1000 {
                    return self.err("too deep");
                }
                self.pos += 1;
                let mut m = Vec::new();
                self.ws();
                if self.s.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Json::Obj(m));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if self.s.get(self.pos) != Some(&b':') {
                        return self.err("expected ':'");
                    }
                    self.pos += 1;
                    self.ws();
                    let v = self.value()?;
                    m.push((k, v));
                    self.ws();
                    match self.s.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            self.depth -= 1;
                            return Ok(Json::Obj(m));
                        }
                        _ => return self.err("expected ',' or '}'"),
                    }
                }
            }
            Some(b'[') => {
                self.depth += 1;
                if self.depth > 1000 {
                    return self.err("too deep");
                }
                self.pos += 1;
                let mut a = Vec::new();
                self.ws();
                if self.s.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Json::Arr(a));
                }
                loop {
                    self.ws();
                    a.push(self.value()?);
                    self.ws();
                    match self.s.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            self.depth -= 1;
                            return Ok(Json::Arr(a));
                        }
                        _ => return self.err("expected ',' or ']'"),
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') if self.s[self.pos..].starts_with(b"true") => {
                self.pos += 4;
                Ok(Json::Bool(true))
            }
            Some(b'f') if self.s[self.pos..].starts_with(b"false") => {
                self.pos += 5;
                Ok(Json::Bool(false))
            }
            Some(b'n') if self.s[self.pos..].starts_with(b"null") => {
                self.pos += 4;
                Ok(Json::Null)
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => self.number(),
            _ => self.err("unexpected character"),
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        let mut float = false;
        while self.pos < self.s.len() {
            match self.s[self.pos] {
                b'0'..=b'9' | b'-' | b'+' => {}
                b'.' | b'e' | b'E' => float = true,
                _ => break,
            }
            self.pos += 1;
        }
        let t = std::str::from_utf8(&self.s[start..self.pos]).map_err(|e| e.to_string())?;
        if !float {
            if let Ok(i) = t.parse::<i64>() {
                return Ok(Json::Int(i));
            }
            if let Ok(u) = t.parse::<u64>() {
                return Ok(Json::UInt(u));
            }
        }
        t.parse::<f64>().map(Json::Float).map_err(|_| format!("bad number [{t}]"))
    }

    fn string(&mut self) -> Result<String, String> {
        if self.s.get(self.pos) != Some(&b'"') {
            return self.err("expected string");
        }
        self.pos += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(&c) = self.s.get(self.pos) else { return self.err("unterminated string") };
            self.pos += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|e| e.to_string()),
                b'\\' => {
                    let Some(&e) = self.s.get(self.pos) else { return self.err("bad escape") };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'/' => out.push(b'/'),
                        b'\\' => out.push(b'\\'),
                        b'"' => out.push(b'"'),
                        b'u' => {
                            let cp = self.hex4()?;
                            let ch = if (0xD800..0xDC00).contains(&cp) && self.s[self.pos..].starts_with(b"\\u") {
                                self.pos += 2;
                                let lo = self.hex4()?;
                                char::from_u32(0x10000 + ((cp - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF))
                            } else {
                                char::from_u32(cp)
                            };
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.unwrap_or('\u{FFFD}').encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return self.err("bad escape"),
                    }
                }
                c => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.s.get(self.pos..self.pos + 4).ok_or("bad \\u escape")?;
        let h = std::str::from_utf8(h).map_err(|e| e.to_string())?;
        self.pos += 4;
        u32::from_str_radix(h, 16).map_err(|e| e.to_string())
    }
}

/// Java's `Double.toString(d)`.
pub fn java_double(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if d == 0.0 {
        return if d.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // Rust's `{:e}` prints the shortest digits that round-trip, like Java 19+. Java renders at
    // least two significant digits and picks the closest two-digit decimal when the shortest
    // has one digit (`4.9E-324`, not `5.0E-324`).
    let sci = format!("{:e}", d.abs());
    let sci = if sci.split('e').next().is_some_and(|m| m.len() == 1) { two_digits(format!("{:.1e}", d.abs()), sci) } else { sci };
    java_format(&sci, d < 0.0)
}

/// Java's `Float.toString(f)`.
pub fn java_float(f: f32) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sci = format!("{:e}", f.abs());
    let sci = if sci.split('e').next().is_some_and(|m| m.len() == 1) { two_digits(format!("{:.1e}", f.abs()), sci) } else { sci };
    java_format(&sci, f < 0.0)
}

fn two_digits(two: String, one: String) -> String {
    if two.split('e').next().is_some_and(|m| m.ends_with('0')) { one } else { two }
}

fn java_format(sci: &str, negative: bool) -> String {
    let (mant, exp) = sci.split_once('e').unwrap_or((sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let mut out = String::with_capacity(digits.len() + 8);
    if negative {
        out.push('-');
    }
    if (-3..7).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', int_len - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-exp - 1) as usize));
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        out.push('.');
        if digits.len() > 1 {
            out.push_str(&digits[1..]);
        } else {
            out.push('0');
        }
        out.push('E');
        out.push_str(&exp.to_string());
    }
    out
}

/// `Double.parseDouble` (Java's grammar: optional sign, digits, fraction, exponent, `NaN`,
/// `Infinity`, hex floats are not needed here, a trailing `d`/`f` type suffix, surrounding
/// whitespace trimmed).
pub fn parse_java_double(s: &str) -> Option<f64> {
    let t = s.trim_matches(|c: char| c <= ' ');
    if t.is_empty() {
        return None;
    }
    let body = t.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(t);
    match body.trim_start_matches(['+', '-']) {
        "NaN" => return Some(f64::NAN),
        "Infinity" => return Some(if body.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY }),
        _ => {}
    }
    let b = body.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let s = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == s {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    body.parse::<f64>().ok()
}

/// `Long.parseLong`.
pub fn parse_java_long(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.is_empty() {
        return None;
    }
    let digits = if b[0] == b'+' || b[0] == b'-' { &b[1..] } else { b };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    s.strip_prefix('+').unwrap_or(s).parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_doubles() {
        assert_eq!(java_double(1.0), "1.0");
        assert_eq!(java_double(1e10), "1.0E10");
        assert_eq!(java_double(1234567.0), "1234567.0");
        assert_eq!(java_double(12345678.0), "1.2345678E7");
        assert_eq!(java_double(0.001), "0.001");
        assert_eq!(java_double(0.0001), "1.0E-4");
        assert_eq!(java_double(-2.5), "-2.5");
        assert_eq!(java_double(f64::MIN_POSITIVE * f64::EPSILON), "4.9E-324");
        assert_eq!(java_float(0.1), "0.1");
        assert_eq!(java_float(1.0), "1.0");
    }

    #[test]
    fn parse_and_write_keep_order() {
        let j = Json::parse(r#"{"b":1,"a":[1.5,"x",null,true],"c":{"z":-3e2}}"#).unwrap();
        assert_eq!(j.to_json_string(), r#"{"b":1,"a":[1.5,"x",null,true],"c":{"z":-300.0}}"#);
        assert_eq!(j.get("a").unwrap().as_array().unwrap().len(), 4);
        assert_eq!(Json::Float(f64::INFINITY).to_json_string(), "\"Infinity\"");
        assert_eq!(Json::parse(r#""é😀""#).unwrap(), Json::Str("é😀".into()));
    }

    #[test]
    fn java_number_parsing() {
        assert_eq!(parse_java_double(" 1.5d "), Some(1.5));
        assert_eq!(parse_java_double("1e3"), Some(1000.0));
        assert_eq!(parse_java_double("-Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(parse_java_double("abc"), None);
        assert_eq!(parse_java_double("1e"), None);
        assert_eq!(parse_java_long("+12"), Some(12));
        assert_eq!(parse_java_long("1.0"), None);
    }
}
