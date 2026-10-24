//! JSON: `SELECT JSON`, `toJson()`, `INSERT JSON` and `fromJson()`.
//!
//! Cassandra decodes JSON with Jackson and converts with each marshal type's `fromJSONObject` /
//! `toJSONString` (Apache-2.0); the rules hinge on Jackson's number classes: an integer that fits
//! 32 bits is an `Integer`, then `Long`, then `BigInteger`; any number with a fraction or exponent
//! is a `Double`. So `int` takes only an `Integer`, `bigint` an `Integer` or `Long`, and
//! `timestamp` only a `Long` or a string (`1000` is rejected, `"1000"` accepted).
//!
//! ScyllaDB uses rapidjson (integers up to 64 bits, everything else `double`) with its own rules
//! and messages, collected from the image: integral doubles are accepted for integer types,
//! tuples may be shorter than their type, UDT fields are matched exactly (no quoting), and
//! numbers are re-printed in rapidjson's `dtoa` form inside messages.
//!
//! Output is the same structure for both: strings quoted with JSON escapes, blobs `"0x…"`,
//! temporal values and UUIDs as strings, `NaN` / infinities as `null`, map keys that are not
//! strings JSON-encoded into a string (`{"[1, 2]": 3}`).

use bytes::Bytes;
use num_bigint::BigInt;

use crate::format::scalar_text;
use crate::num::Decimal;
use crate::value::{encode, split_vector};
use crate::wire::{self, Collection};
use crate::{CqlType, Dialect, MarshalError, Value, float, literal::from_string, temporal};

/// A parsed JSON value, with the number classes both decoders distinguish.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// An integer token that fits 64 bits signed.
    Int(i64),
    /// ScyllaDB: an integer in `2^63 .. 2^64`.
    UInt(u64),
    /// Cassandra: an integer beyond 64 bits.
    BigInt(BigInt),
    Double(f64),
    Str(String),
    List(Vec<Json>),
    /// Members in document order; a repeated key keeps its first position and last value.
    Map(Vec<(String, Json)>),
}

const MAX_DEPTH: usize = 256;

/// Parses JSON text the way the dialect's decoder does.
pub fn parse_json(text: &str, dialect: Dialect) -> Result<Json, MarshalError> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
        dialect,
    };
    let res = p.ws().and_then(|_| p.value(0)).and_then(|v| {
        p.ws()?;
        if p.i != p.s.len() {
            Err(p.fail("The document root must not be followed by other values."))
        } else {
            Ok(v)
        }
    });
    res.map_err(|m| match dialect {
        Dialect::Cassandra => MarshalError::new(format!("Error decoding JSON string: {m}")),
        Dialect::Scylla => MarshalError::new(format!("Parsing JSON failed: {m}")),
    })
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    dialect: Dialect,
}

impl Parser<'_> {
    fn fail(&self, m: &str) -> String {
        match self.dialect {
            Dialect::Scylla => format!("{m} at {}", self.i),
            Dialect::Cassandra => {
                // Jackson's location suffix (line and 1-based column).
                let before = &self.s[..self.i.min(self.s.len())];
                let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
                let col = before.len()
                    - before
                        .iter()
                        .rposition(|&b| b == b'\n')
                        .map_or(0, |p| p + 1)
                    + 1;
                format!(
                    "{m}\n at [Source: REDACTED (`StreamReadFeature.INCLUDE_SOURCE_IN_LOCATION` disabled); line: {line}, column: {col}]"
                )
            }
        }
    }

    /// End of input inside an array / object (Jackson words it specially).
    fn eof_in(&self, what: &str) -> Option<String> {
        (self.i >= self.s.len()).then(|| match self.dialect {
            Dialect::Cassandra => self.fail(&format!(
                "Unexpected end-of-input within/between {what} entries"
            )),
            Dialect::Scylla => self.fail(if what == "Array" {
                "Missing a comma or ']' after an array element."
            } else {
                "Missing a comma or '}' after an object member."
            }),
        })
    }

    /// Jackson's `_getCharDesc` of the character at the cursor.
    fn jackson_char(&self) -> String {
        let rest = &self.s[self.i.min(self.s.len())..];
        let c = match std::str::from_utf8(&rest[..rest.len().min(4)]) {
            Ok(t) => t.chars().next(),
            Err(e) => std::str::from_utf8(&rest[..e.valid_up_to()])
                .ok()
                .and_then(|t| t.chars().next()),
        }
        .unwrap_or(char::from(rest.first().copied().unwrap_or(0)));
        let code = c as u32;
        if c.is_control() {
            format!("CTRL-CHAR, code {code}")
        } else if code > 255 {
            format!("'{c}' (code {code} / 0x{code:x})")
        } else {
            format!("'{c}' (code {code})")
        }
    }

    fn ws(&mut self) -> Result<(), String> {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err(self.fail("Nesting too deep"));
        }
        self.ws()?;
        match self.s.get(self.i) {
            None => Err(self.fail("The document is empty.")),
            Some(b'{') => {
                self.i += 1;
                let mut members: Vec<(String, Json)> = Vec::new();
                self.ws()?;
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Map(members));
                }
                loop {
                    self.ws()?;
                    if let Some(e) = self.eof_in("Object") {
                        return Err(e);
                    }
                    if self.s.get(self.i) != Some(&b'"') {
                        return Err(match self.dialect {
                            Dialect::Scylla => self.fail("Missing a name for object member."),
                            Dialect::Cassandra => self.fail(&format!(
                                "Unexpected character ({}): was expecting double-quote to start field name",
                                self.jackson_char()
                            )),
                        });
                    }
                    let k = self.string()?;
                    self.ws()?;
                    if self.s.get(self.i) != Some(&b':') {
                        return Err(self.fail("Missing a colon after a name of object member."));
                    }
                    self.i += 1;
                    let v = self.value(depth + 1)?;
                    match members.iter_mut().find(|(n, _)| *n == k) {
                        Some(slot) => slot.1 = v,
                        None => members.push((k, v)),
                    }
                    self.ws()?;
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Map(members));
                        }
                        _ => {
                            return Err(self.fail("Missing a comma or '}' after an object member."));
                        }
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws()?;
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::List(items));
                }
                loop {
                    self.ws()?;
                    if let Some(e) = self.eof_in("Array") {
                        return Err(e);
                    }
                    items.push(self.value(depth + 1)?);
                    self.ws()?;
                    if let Some(e) = self.eof_in("Array") {
                        return Err(e);
                    }
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::List(items));
                        }
                        _ => {
                            return Err(self.fail("Missing a comma or ']' after an array element."));
                        }
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.word("true", Json::Bool(true)),
            Some(b'f') => self.word("false", Json::Bool(false)),
            Some(b'n') => self.word("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.fail("Invalid value.")),
        }
    }

    fn word(&mut self, w: &str, v: Json) -> Result<Json, String> {
        if self.s[self.i..].starts_with(w.as_bytes()) {
            self.i += w.len();
            Ok(v)
        } else {
            Err(self.fail("Invalid value."))
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.s.len()
                && self.s[self.i] != b'"'
                && self.s[self.i] != b'\\'
                && self.s[self.i] >= 0x20
            {
                self.i += 1;
            }
            out.push_str(
                std::str::from_utf8(&self.s[start..self.i])
                    .map_err(|_| self.fail("Invalid encoding in string."))?,
            );
            match self.s.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = *self
                        .s
                        .get(self.i)
                        .ok_or_else(|| self.fail("Missing a closing quotation mark in string."))?;
                    self.i += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xd800..0xdc00).contains(&hi) {
                                if !self.s[self.i..].starts_with(b"\\u") {
                                    return Err(
                                        self.fail("The surrogate pair in string is invalid.")
                                    );
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return Err(
                                        self.fail("The surrogate pair in string is invalid.")
                                    );
                                }
                                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp).ok_or_else(|| {
                                self.fail("The surrogate pair in string is invalid.")
                            })?);
                        }
                        _ => return Err(self.fail("Invalid escape character in string.")),
                    }
                }
                Some(_) => return Err(self.fail("Invalid encoding in string.")),
                None => return Err(self.fail("Missing a closing quotation mark in string.")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self
            .s
            .get(self.i..self.i + 4)
            .ok_or_else(|| self.fail("Incorrect hex digit after \\u escape in string."))?;
        let v = std::str::from_utf8(h)
            .ok()
            .and_then(|h| u32::from_str_radix(h, 16).ok());
        self.i += 4;
        v.ok_or_else(|| self.fail("Incorrect hex digit after \\u escape in string."))
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.s[self.i] == b'-' {
            self.i += 1;
        }
        let int_start = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        if self.i == int_start {
            return Err(self.fail("Miss fraction part in number."));
        }
        let mut is_float = false;
        if self.s.get(self.i) == Some(&b'.') {
            is_float = true;
            self.i += 1;
            let f = self.i;
            while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                self.i += 1;
            }
            if self.i == f {
                return Err(self.fail("Miss fraction part in number."));
            }
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            is_float = true;
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            let e = self.i;
            while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                self.i += 1;
            }
            if self.i == e {
                return Err(self.fail("Miss exponent in number."));
            }
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("0");
        if !is_float {
            if let Ok(v) = text.parse::<i64>() {
                return Ok(Json::Int(v));
            }
            match self.dialect {
                Dialect::Cassandra => {
                    return Ok(Json::BigInt(
                        text.parse().map_err(|_| self.fail("Invalid number"))?,
                    ));
                }
                Dialect::Scylla => {
                    if let Ok(v) = text.parse::<u64>() {
                        return Ok(Json::UInt(v));
                    }
                }
            }
        }
        let v: f64 = text.parse().map_err(|_| self.fail("Invalid number"))?;
        if v.is_infinite() && self.dialect == Dialect::Scylla {
            self.i = start;
            return Err(self.fail("Number too big to be stored in double."));
        }
        Ok(Json::Double(v))
    }
}

impl Json {
    /// Jackson's class name, as Cassandra's messages print it.
    fn java_class(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "Boolean",
            Json::Int(v) if i32::try_from(*v).is_ok() => "Integer",
            Json::Int(_) | Json::UInt(_) => "Long",
            Json::BigInt(_) => "BigInteger",
            Json::Double(_) => "Double",
            Json::Str(_) => "String",
            Json::List(_) => "ArrayList",
            Json::Map(_) => "LinkedHashMap",
        }
    }

    /// Java's `toString` of the decoded object.
    pub fn java_string(&self) -> String {
        match self {
            Json::Null => "null".into(),
            Json::Bool(b) => b.to_string(),
            Json::Int(v) => v.to_string(),
            Json::UInt(v) => v.to_string(),
            Json::BigInt(v) => v.to_string(),
            Json::Double(v) => float::java_double(*v),
            Json::Str(s) => s.clone(),
            Json::List(items) => format!(
                "[{}]",
                items
                    .iter()
                    .map(Json::java_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Json::Map(m) => format!(
                "{{{}}}",
                m.iter()
                    .map(|(k, v)| format!("{k}={}", v.java_string()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// rapidjson's type name, as ScyllaDB's messages print it.
    fn rapidjson_type(&self) -> &'static str {
        match self {
            Json::Null => "kNullType",
            Json::Bool(false) => "kFalseType",
            Json::Bool(true) => "kTrueType",
            Json::Map(_) => "kObjectType",
            Json::List(_) => "kArrayType",
            Json::Str(_) => "kStringType",
            _ => "kNumberType",
        }
    }

    /// Compact JSON text (rapidjson's writer for numbers).
    pub fn to_text(&self) -> String {
        match self {
            Json::Null => "null".into(),
            Json::Bool(b) => b.to_string(),
            Json::Int(v) => v.to_string(),
            Json::UInt(v) => v.to_string(),
            Json::BigInt(v) => v.to_string(),
            Json::Double(v) => rapidjson_double(*v),
            Json::Str(s) => quote_json(s),
            Json::List(items) => format!(
                "[{}]",
                items
                    .iter()
                    .map(Json::to_text)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Json::Map(m) => format!(
                "{{{}}}",
                m.iter()
                    .map(|(k, v)| format!("{}:{}", quote_json(k), v.to_text()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }
}

/// rapidjson's `Writer::WriteDouble` (Grisu shortest digits, `Prettify` layout): `1.5`,
/// `100.0`, `1e21`, `1.2345678901234568e29`, `1e-7`.
pub fn rapidjson_double(x: f64) -> String {
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    if !x.is_finite() {
        return if x.is_nan() {
            "NaN".into()
        } else if x > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    let s = format!("{:e}", x.abs());
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let e: i32 = exp.parse().unwrap_or(0);
    let len = digits.len() as i32;
    let kk = e + 1;
    let k = kk - len;
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
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
    } else {
        out.push_str(&digits[..1]);
        if len > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push_str(&(kk - 1).to_string());
    }
    out
}

/// A JSON string literal with the escapes Jackson and rapidjson both produce.
pub fn quote_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The key a column gets in a `SELECT JSON` row: case-sensitive names are wrapped in double
/// quotes (`"\"myColumn\""`), as both references document.
pub fn json_column_key(name: &str) -> String {
    if name.chars().any(|c| c.is_ascii_uppercase()) || name != name.to_lowercase() {
        quote_json(&format!("\"{name}\""))
    } else {
        quote_json(name)
    }
}

/// `toJson(x)` / one value of a `SELECT JSON` row.
pub fn to_json(ty: &CqlType, b: &[u8], dialect: Dialect) -> Result<String, MarshalError> {
    let mut out = String::new();
    json_into(&mut out, ty, b, dialect)?;
    Ok(out)
}

fn json_into(
    out: &mut String,
    ty: &CqlType,
    b: &[u8],
    dialect: Dialect,
) -> Result<(), MarshalError> {
    use CqlType as T;
    if b.is_empty()
        && !matches!(ty, T::Ascii | T::Text | T::Blob | T::Custom(_))
        && !ty.is_collection()
    {
        out.push_str("\"\"");
        return Ok(());
    }
    match ty {
        T::Ascii | T::Text => out.push_str(&quote_json(&String::from_utf8_lossy(b))),
        T::Blob | T::Custom(_) => {
            out.push_str("\"0x");
            out.push_str(&wire::hex(b));
            out.push('"');
        }
        T::Float | T::Double => {
            let finite = match ty {
                T::Float => b
                    .try_into()
                    .map(|a: [u8; 4]| f32::from_be_bytes(a).is_finite())
                    .unwrap_or(true),
                _ => b
                    .try_into()
                    .map(|a: [u8; 8]| f64::from_be_bytes(a).is_finite())
                    .unwrap_or(true),
            };
            if finite {
                out.push_str(&scalar_text(ty, b, dialect)?.unwrap_or_default());
            } else {
                out.push_str("null");
            }
        }
        T::Boolean
        | T::Tinyint
        | T::Smallint
        | T::Int
        | T::Bigint
        | T::Counter
        | T::Decimal
        | T::Varint => {
            out.push_str(&scalar_text(ty, b, dialect)?.unwrap_or_default());
        }
        T::Timestamp => {
            let a: [u8; 8] = b
                .try_into()
                .map_err(|_| MarshalError::new("Expected 8 bytes for a timestamp"))?;
            out.push('"');
            out.push_str(&temporal::format_timestamp(
                i64::from_be_bytes(a),
                ' ',
                dialect,
            ));
            out.push('"');
        }
        T::Date | T::Time | T::Uuid | T::Timeuuid | T::Inet | T::Duration | T::Empty => {
            out.push_str(&quote_json(
                &scalar_text(ty, b, dialect)?.unwrap_or_default(),
            ));
        }
        T::List { elem, .. } | T::Set { elem, .. } => {
            let c = Collection::parse(b)
                .ok_or_else(|| MarshalError::new("Not enough bytes to read a list"))?;
            out.push('[');
            for (i, item) in c.items(1).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                match item.map_err(|_| MarshalError::new("Not enough bytes to read a list"))? {
                    Some(v) => json_into(out, elem, v, dialect)?,
                    None => out.push_str("null"),
                }
            }
            out.push(']');
        }
        T::Map { key, value, .. } => {
            out.push('{');
            for (i, (k, v)) in crate::value::map_entries(b)?.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let kj = to_json(key, k, dialect)?;
                if kj.starts_with('"') {
                    out.push_str(&kj);
                } else {
                    out.push_str(&quote_json(&kj));
                }
                out.push_str(": ");
                json_into(out, value, v, dialect)?;
            }
            out.push('}');
        }
        T::Tuple(ts) => {
            let fs: Vec<_> = wire::fields(b).collect();
            out.push('[');
            for (i, t) in ts.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                match fs.get(i) {
                    Some(Ok(Some(v))) => json_into(out, t, v, dialect)?,
                    Some(Err(_)) => {
                        return Err(MarshalError::new(format!(
                            "Not enough bytes to read {i}th component"
                        )));
                    }
                    _ => out.push_str("null"),
                }
            }
            out.push(']');
        }
        T::Udt(def, _) => {
            let fs: Vec<_> = wire::fields(b).collect();
            out.push('{');
            for (i, (name, t)) in def.fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let key = if dialect == Dialect::Cassandra && *name != name.to_lowercase() {
                    format!("\"{name}\"")
                } else {
                    name.clone()
                };
                out.push_str(&quote_json(&key));
                out.push_str(": ");
                match fs.get(i) {
                    Some(Ok(Some(v))) => json_into(out, t, v, dialect)?,
                    Some(Err(_)) => {
                        return Err(MarshalError::new(format!(
                            "Not enough bytes to read {i}th field {name}"
                        )));
                    }
                    _ => out.push_str("null"),
                }
            }
            out.push('}');
        }
        T::Vector { elem, dim } => {
            let parts = split_vector(elem, *dim, b).map_err(|_| {
                MarshalError::new(format!(
                    "Not enough bytes to read a {}",
                    crate::names::cql_name(ty)
                ))
            })?;
            out.push('[');
            for (i, p) in parts.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                json_into(out, elem, p, dialect)?;
            }
            out.push(']');
        }
    }
    Ok(())
}

/// `fromJson(text)`: parses the JSON text and converts it.
pub fn from_json_text(ty: &CqlType, text: &str, dialect: Dialect) -> Result<Bytes, MarshalError> {
    let j = parse_json(text, dialect)?;
    from_json(ty, &j, dialect)
}

/// Converts a decoded JSON value to a serialised value of `ty` (`fromJSONObject`). A JSON null
/// gives an empty `Bytes`; callers treat a top-level null as a CQL null.
pub fn from_json(ty: &CqlType, j: &Json, dialect: Dialect) -> Result<Bytes, MarshalError> {
    match dialect {
        Dialect::Cassandra => cassandra(ty, j),
        Dialect::Scylla => scylla(ty, j),
    }
}

fn expected(what: &str, j: &Json) -> MarshalError {
    MarshalError::new(format!(
        "Expected {what}, but got a {}: {}",
        j.java_class(),
        j.java_string()
    ))
}

fn int_bytes(ty: &CqlType, v: i64) -> Bytes {
    Bytes::from(match ty {
        CqlType::Int => (v as i32).to_be_bytes().to_vec(),
        _ => v.to_be_bytes().to_vec(),
    })
}

fn decode_if_string<'a>(
    j: &'a Json,
    owned: &'a mut Option<Json>,
    dialect: Dialect,
) -> Result<&'a Json, MarshalError> {
    if let Json::Str(s) = j {
        *owned = Some(parse_json(s, dialect)?);
        return Ok(owned.as_ref().unwrap_or(j));
    }
    Ok(j)
}

fn cassandra(ty: &CqlType, j: &Json) -> Result<Bytes, MarshalError> {
    use CqlType as T;
    let c = Dialect::Cassandra;
    match ty {
        T::Ascii | T::Text => match j {
            Json::Str(s) => from_string(ty, s, c),
            _ => Err(expected(
                if matches!(ty, T::Ascii) {
                    "an ascii string"
                } else {
                    "a UTF-8 string"
                },
                j,
            )),
        },
        T::Bigint | T::Int => match j {
            Json::Str(s) => from_string(ty, s, c),
            Json::Int(v) if matches!(ty, T::Bigint) || i32::try_from(*v).is_ok() => {
                Ok(int_bytes(ty, *v))
            }
            _ => Err(expected(
                if matches!(ty, T::Int) {
                    "an int value"
                } else {
                    "a bigint value"
                },
                j,
            )),
        },
        T::Counter => Err(MarshalError::new(
            "Counter values cannot be set through JSON",
        )),
        T::Smallint | T::Tinyint => match j {
            Json::Str(s) => from_string(ty, s, c),
            Json::Int(_) | Json::UInt(_) | Json::BigInt(_) | Json::Double(_) => {
                from_string(ty, &j.java_string(), c)
            }
            _ => Err(expected(
                if matches!(ty, T::Smallint) {
                    "a short value"
                } else {
                    "a byte value"
                },
                j,
            )),
        },
        T::Float | T::Double => {
            let f = match j {
                Json::Str(s) => return from_string(ty, s, c),
                Json::Int(v) => *v as f64,
                Json::UInt(v) => *v as f64,
                Json::BigInt(v) => crate::num::bigint_to_f64(v),
                Json::Double(v) => *v,
                _ => {
                    return Err(expected(
                        if matches!(ty, T::Float) {
                            "a float value"
                        } else {
                            "a double value"
                        },
                        j,
                    ));
                }
            };
            Ok(if matches!(ty, T::Float) {
                // `Number.floatValue`: integers round once, doubles narrow.
                let x = match j {
                    Json::Int(v) => *v as f32,
                    Json::BigInt(v) => crate::num::bigint_to_f64(v) as f32,
                    _ => f as f32,
                };
                Bytes::copy_from_slice(&x.to_be_bytes())
            } else {
                Bytes::copy_from_slice(&f.to_be_bytes())
            })
        }
        T::Decimal => from_string(ty, &j.java_string(), c).map_err(|_| {
            MarshalError::new(format!(
                "Value '{}' is not a valid representation of a decimal value",
                j.java_string()
            ))
        }),
        T::Varint => {
            let s = j.java_string();
            match crate::num::parse_varint(&s) {
                Some(v) => Ok(Bytes::from(v.to_signed_bytes_be())),
                None => Err(MarshalError::new(format!(
                    "Value '{s}' is not a valid representation of a varint value"
                ))),
            }
        }
        T::Boolean => match j {
            Json::Str(s) => from_string(ty, s, c),
            Json::Bool(b) => Ok(Bytes::from(vec![*b as u8])),
            _ => Err(expected("a boolean value", j)),
        },
        T::Timestamp => match j {
            Json::Int(v) if i32::try_from(*v).is_err() => {
                Ok(Bytes::copy_from_slice(&v.to_be_bytes()))
            }
            Json::Str(s) => from_string(ty, s, c),
            _ => Err(expected(
                "a long or a datestring representation of a timestamp value",
                j,
            )),
        },
        T::Date | T::Time | T::Uuid | T::Timeuuid | T::Inet | T::Duration => match j {
            Json::Str(s) => from_string(ty, s, c),
            _ => Err(expected(
                match ty {
                    T::Date => "a string representation of a date value",
                    T::Time => "a string representation of a time value",
                    T::Uuid => "a string representation of a uuid",
                    T::Timeuuid => "a string representation of a timeuuid",
                    T::Inet => "a string representation of an inet value",
                    _ => "a string representation of a duration",
                },
                j,
            )),
        },
        T::Blob | T::Custom(_) => {
            let invalid = |m: String| {
                MarshalError::new(format!(
                    "Value '{}' is not a valid blob representation: {m}",
                    j.java_string()
                ))
            };
            match j {
                Json::Str(s) => match s.strip_prefix("0x") {
                    Some(h) => from_string(&T::Blob, h, c).map_err(|e| invalid(e.message)),
                    None => Err(invalid(format!(
                        "String representation of blob is missing 0x prefix: {s}"
                    ))),
                },
                _ => {
                    let class = match j {
                        Json::List(_) => "java.util.ArrayList".to_string(),
                        Json::Map(_) => "java.util.LinkedHashMap".to_string(),
                        Json::BigInt(_) => "java.math.BigInteger".to_string(),
                        other => format!("java.lang.{}", other.java_class()),
                    };
                    Err(invalid(format!(
                        "class {class} cannot be cast to class java.lang.String ({class} and java.lang.String are in module java.base of loader 'bootstrap')"
                    )))
                }
            }
        }
        T::Empty => match j {
            Json::Str(s) if s.is_empty() => Ok(Bytes::new()),
            Json::Str(s) => Err(MarshalError::new(format!("'{s}' is not empty"))),
            _ => Err(MarshalError::new(format!(
                "Expected an empty string, but got: {}",
                j.java_string()
            ))),
        },
        T::List { elem, .. } | T::Set { elem, .. } | T::Vector { elem, .. } => {
            let mut owned = None;
            let j = decode_if_string(j, &mut owned, c)?;
            let is_set = matches!(ty, T::Set { .. });
            let Json::List(items) = j else {
                return Err(expected(
                    if is_set {
                        "a list (representing a set)"
                    } else {
                        "a list"
                    },
                    j,
                ));
            };
            if let T::Vector { dim, .. } = ty
                && items.len() != *dim as usize
            {
                return Err(MarshalError::new(format!(
                    "List had incorrect size: expected {dim} but given {}; {}",
                    items.len(),
                    j.java_string()
                )));
            }
            let mut vals = Vec::with_capacity(items.len());
            for it in items {
                if matches!(it, Json::Null) {
                    return Err(MarshalError::new(if is_set {
                        "Invalid null element in set"
                    } else {
                        "Invalid null element in list"
                    }));
                }
                vals.push(Value::Custom(cassandra(elem, it)?));
            }
            Ok(pack(ty, vals))
        }
        T::Map { key, value, .. } => {
            let mut owned = None;
            let j = decode_if_string(j, &mut owned, c)?;
            let Json::Map(members) = j else {
                return Err(expected("a map", j));
            };
            let mut entries = Vec::with_capacity(members.len());
            for (k, v) in members {
                if matches!(v, Json::Null) {
                    return Err(MarshalError::new("Invalid null value in map"));
                }
                entries.push((
                    Value::Custom(cassandra(key, &Json::Str(k.clone()))?),
                    Value::Custom(cassandra(value, v)?),
                ));
            }
            Ok(encode(&Value::Map(entries), ty))
        }
        T::Tuple(ts) => {
            let mut owned = None;
            let j = decode_if_string(j, &mut owned, c)?;
            let Json::List(items) = j else {
                return Err(expected("a list representation of a tuple", j));
            };
            if items.len() > ts.len() {
                return Err(MarshalError::new(format!(
                    "Tuple contains extra items (expected {}): {}",
                    ts.len(),
                    j.java_string()
                )));
            }
            if items.len() < ts.len() {
                return Err(MarshalError::new(format!(
                    "Tuple is missing items (expected {}): {}",
                    ts.len(),
                    j.java_string()
                )));
            }
            let mut out = Vec::new();
            for (t, it) in ts.iter().zip(items) {
                let v = if matches!(it, Json::Null) {
                    None
                } else {
                    Some(cassandra(t, it)?)
                };
                wire::put_item(&mut out, v.as_deref());
            }
            Ok(Bytes::from(out))
        }
        T::Udt(def, _) => {
            let mut owned = None;
            let j = decode_if_string(j, &mut owned, c)?;
            let Json::Map(members) = j else {
                return Err(expected("a map", j));
            };
            // `JsonUtils.handleCaseSensitivity`: "\"Name\"" keeps its case, other keys lowercase.
            let members: Vec<(String, &Json)> = members
                .iter()
                .map(|(k, v)| {
                    let k = if k.len() >= 2 && k.starts_with('"') && k.ends_with('"') {
                        k[1..k.len() - 1].to_string()
                    } else {
                        k.to_lowercase()
                    };
                    (k, v)
                })
                .collect();
            for (k, _) in &members {
                if def.field_index(k).is_none() {
                    return Err(MarshalError::new(format!(
                        "Unknown field '{k}' in value of user defined type {}",
                        def.name
                    )));
                }
            }
            let mut out = Vec::new();
            for (name, t) in &def.fields {
                match members
                    .iter()
                    .rev()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| *v)
                {
                    None | Some(Json::Null) => wire::put_item(&mut out, None),
                    Some(v) => wire::put_item(&mut out, Some(&cassandra(t, v)?)),
                }
            }
            Ok(Bytes::from(out))
        }
    }
}

/// Lists, sets and vectors from already-serialised elements (sets sorted and de-duplicated).
fn pack(ty: &CqlType, vals: Vec<Value>) -> Bytes {
    match ty {
        CqlType::Set { .. } => encode(&Value::Set(vals), ty),
        CqlType::Vector { .. } => encode(&Value::Vector(vals), ty),
        _ => encode(&Value::List(vals), ty),
    }
}

fn scylla_err(m: impl std::fmt::Display) -> MarshalError {
    MarshalError::scylla(m)
}

/// ScyllaDB's integer conversion: integral numbers in range, strings via the type's parser.
fn scylla_int(ty: &CqlType, j: &Json) -> Result<Bytes, MarshalError> {
    let s = Dialect::Scylla;
    let (min, max): (i128, i128) = match ty {
        CqlType::Tinyint => (i8::MIN as i128, i8::MAX as i128),
        CqlType::Smallint => (i16::MIN as i128, i16::MAX as i128),
        CqlType::Int => (i32::MIN as i128, i32::MAX as i128),
        _ => (i64::MIN as i128, i64::MAX as i128),
    };
    let v: i128 = match j {
        // An empty string is no number here (`from_string` maps it to an empty value).
        Json::Str(t) if t.is_empty() => {
            return Err(MarshalError::scylla("Invalid number format ''".to_string()));
        }
        Json::Str(t) => return from_string(ty, t, s),
        Json::Int(v) => *v as i128,
        Json::UInt(v) => *v as i128,
        Json::Double(d) => {
            if d.fract() != 0.0 {
                return Err(scylla_err(format!(
                    "Incorrect JSON floating-point value for int64 type: {} (it should not contain fractional part {})",
                    rapidjson_double(*d).trim_end_matches(".0"),
                    rapidjson_double(d.fract()).trim_end_matches(".0")
                )));
            }
            if d.abs() >= 1.8446744073709552e19 {
                return Err(scylla_err(format!(
                    "Value {} out of range",
                    rapidjson_double(*d)
                )));
            }
            *d as i128
        }
        other => {
            return Err(scylla_err(format!(
                "Incorrect JSON value for int64 type: {}",
                other.to_text()
            )));
        }
    };
    if !(min..=max).contains(&v) {
        return Err(scylla_err(format!("Value {v} out of range")));
    }
    Ok(Bytes::from(match ty {
        CqlType::Tinyint => vec![v as i8 as u8],
        CqlType::Smallint => (v as i16).to_be_bytes().to_vec(),
        CqlType::Int => (v as i32).to_be_bytes().to_vec(),
        _ => (v as i64).to_be_bytes().to_vec(),
    }))
}

/// The number's text as rapidjson re-prints it (integers exactly, doubles via `dtoa`).
fn number_text(j: &Json) -> Option<String> {
    match j {
        Json::Int(v) => Some(v.to_string()),
        Json::UInt(v) => Some(v.to_string()),
        Json::BigInt(v) => Some(v.to_string()),
        Json::Double(d) => Some(rapidjson_double(*d)),
        _ => None,
    }
}

fn scylla(ty: &CqlType, j: &Json) -> Result<Bytes, MarshalError> {
    use CqlType as T;
    let s = Dialect::Scylla;
    let string_or = |kind: &str| -> Result<Bytes, MarshalError> {
        match j {
            Json::Str(t) => from_string(ty, t, s),
            other => Err(scylla_err(format!(
                "{kind} must be represented as string in JSON, instead got '{}'",
                other.to_text()
            ))),
        }
    };
    match ty {
        T::Ascii | T::Text => match j {
            Json::Str(t) => from_string(ty, t, s),
            other => Err(MarshalError::new(format!(
                "rjson::to_string_view expects a string value, got: {}",
                other.rapidjson_type()
            ))),
        },
        T::Tinyint | T::Smallint | T::Int | T::Bigint | T::Counter | T::Timestamp => {
            match (ty, j) {
                (T::Timestamp, Json::Str(t)) => from_string(ty, t, s),
                (T::Timestamp, _) => scylla_int(&T::Bigint, j),
                _ => scylla_int(ty, j),
            }
        }
        T::Varint => match j {
            Json::Str(t) => from_string(ty, t, s),
            Json::Int(_) | Json::UInt(_) | Json::BigInt(_) => {
                from_string(ty, &number_text(j).unwrap_or_default(), s)
            }
            Json::Double(d) => {
                let text = rapidjson_double(*d);
                let t = text.strip_suffix(".0").unwrap_or(&text);
                from_string(ty, t, s)
                    .map_err(|_| scylla_err(format!("unable to make int from '{t}'")))
            }
            other => Err(scylla_err(format!(
                "unable to make int from '{}'",
                other.to_text()
            ))),
        },
        T::Decimal => match j {
            Json::Str(t) => from_string(ty, t, s),
            other => match number_text(other) {
                Some(t) => from_string(ty, &t, s),
                None => Err(scylla_err(format!(
                    "{} must be represented as numeric or string in JSON",
                    other.to_text()
                ))),
            },
        },
        T::Float | T::Double => {
            let f = match j {
                Json::Str(t) => return from_string(ty, t, s),
                Json::Int(v) => *v as f64,
                Json::UInt(v) => *v as f64,
                Json::Double(d) => *d,
                _ => {
                    return Err(scylla_err(
                        "JSON value must be represented as double or string",
                    ));
                }
            };
            Ok(if matches!(ty, T::Float) {
                Bytes::copy_from_slice(&(f as f32).to_be_bytes())
            } else {
                Bytes::copy_from_slice(&f.to_be_bytes())
            })
        }
        T::Boolean => match j {
            Json::Bool(b) => Ok(Bytes::from(vec![*b as u8])),
            Json::Str(t) if t.eq_ignore_ascii_case("true") => Ok(Bytes::from_static(&[1])),
            Json::Str(t) if t.eq_ignore_ascii_case("false") => Ok(Bytes::from_static(&[0])),
            other => Err(scylla_err(format!(
                "Invalid JSON object {}",
                other.to_text()
            ))),
        },
        T::Date => string_or("simple_date_type"),
        T::Time => string_or("time_type"),
        T::Uuid => string_or("uuid_type"),
        T::Timeuuid => string_or("timeuuid_type"),
        T::Inet => string_or("inet_addr_type"),
        T::Duration => string_or("duration_type"),
        T::Blob | T::Custom(_) => match j {
            Json::Str(t) => match t.strip_prefix("0x") {
                Some(h) => from_string(&T::Blob, h, s),
                None => Err(scylla_err("Blob JSON strings must start with 0x")),
            },
            other => Err(scylla_err(format!(
                "bytes_type must be represented as string in JSON, instead got '{}'",
                other.to_text()
            ))),
        },
        T::Empty => Ok(Bytes::new()),
        // ScyllaDB does not decode JSON held in strings for compound types.
        T::List { elem, .. } | T::Set { elem, .. } | T::Vector { elem, .. } => {
            let Json::List(items) = j else {
                let kind = match ty {
                    T::List { .. } => "list_type",
                    T::Set { .. } => "set_type",
                    _ => "vector_type",
                };
                return Err(scylla_err(format!(
                    "{kind} must be represented as JSON Array"
                )));
            };
            if let T::Vector { dim, .. } = ty {
                if items.len() > *dim as usize {
                    return Err(scylla_err(format!(
                        "Too many values ({}) for vector with size {dim}",
                        items.len()
                    )));
                }
                if items.len() < *dim as usize {
                    return Err(scylla_err(format!(
                        "Too few values ({}) for vector with size {dim}",
                        items.len()
                    )));
                }
            }
            let mut vals = Vec::with_capacity(items.len());
            for it in items {
                vals.push(Value::Custom(scylla(elem, it)?));
            }
            Ok(pack(ty, vals))
        }
        T::Map { key, value, .. } => {
            let Json::Map(members) = j else {
                return Err(scylla_err("map_type must be represented as JSON Object"));
            };
            let mut entries = Vec::with_capacity(members.len());
            for (k, v) in members {
                let kb = if key.is_collection() || key.is_tuple() || key.is_udt() || key.is_vector()
                {
                    from_json_text(key, k, s)?
                } else if matches!(**key, T::Blob) {
                    // Blob keys are JSON blob strings (`0x…`).
                    scylla(key, &Json::Str(k.clone()))?
                } else {
                    from_string(key, k, s)?
                };
                entries.push((Value::Custom(kb), Value::Custom(scylla(value, v)?)));
            }
            Ok(encode(&Value::Map(entries), ty))
        }
        T::Tuple(ts) => {
            let Json::List(items) = j else {
                return Err(scylla_err("tuple_type must be represented as JSON Array"));
            };
            if items.len() > ts.len() {
                return Err(scylla_err(format!(
                    "Too many values ({}) for tuple with size {}",
                    items.len(),
                    ts.len()
                )));
            }
            let mut out = Vec::new();
            for (t, it) in ts.iter().zip(items) {
                let v = if matches!(it, Json::Null) {
                    None
                } else {
                    Some(scylla(t, it)?)
                };
                wire::put_item(&mut out, v.as_deref());
            }
            Ok(Bytes::from(out))
        }
        T::Udt(def, _) => {
            let Json::Map(members) = j else {
                return Err(scylla_err("user_type must be represented as JSON Object"));
            };
            for (k, _) in members {
                if def.field_index(k).is_none() {
                    return Err(scylla_err(format!(
                        "Extraneous field definition for user type {}: {k}",
                        def.name
                    )));
                }
            }
            let mut out = Vec::new();
            for (name, t) in &def.fields {
                match members.iter().find(|(k, _)| k == name).map(|(_, v)| v) {
                    None | Some(Json::Null) => wire::put_item(&mut out, None),
                    Some(v) => wire::put_item(&mut out, Some(&scylla(t, v)?)),
                }
            }
            Ok(Bytes::from(out))
        }
    }
}

/// `Decimal` from a JSON number, exposed for callers converting numbers themselves.
pub fn json_number_decimal(j: &Json) -> Option<Decimal> {
    Decimal::parse(&number_text(j)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const C: Dialect = Dialect::Cassandra;
    const S: Dialect = Dialect::Scylla;

    fn fj(ty: &CqlType, text: &str, d: Dialect) -> Result<Bytes, String> {
        from_json_text(ty, text, d).map_err(|e| e.message)
    }

    fn tj(ty: &CqlType, v: Value, d: Dialect) -> String {
        to_json(ty, &encode(&v, ty), d).unwrap()
    }

    fn addr() -> CqlType {
        CqlType::Udt(
            Arc::new(crate::UdtDef::new(
                "ks",
                "addr",
                vec![
                    ("street".into(), CqlType::Text),
                    ("Zip".into(), CqlType::Int),
                    ("select".into(), CqlType::Text),
                ],
            )),
            true,
        )
    }

    #[test]
    fn parser() {
        assert_eq!(
            parse_json(
                r#"{"a": [1, 2.5, "x\n\u00e9\ud83d\ude00", true, null], "a": 3}"#,
                C
            )
            .unwrap(),
            Json::Map(vec![("a".into(), Json::Int(3))])
        );
        assert_eq!(
            parse_json("12345678901234567890", C).unwrap(),
            Json::BigInt("12345678901234567890".parse().unwrap())
        );
        assert_eq!(
            parse_json("12345678901234567890", S).unwrap(),
            Json::UInt(12_345_678_901_234_567_890)
        );
        assert_eq!(
            parse_json("123456789012345678901234567890", S).unwrap(),
            Json::Double(1.2345678901234568e29)
        );
        assert_eq!(parse_json("1e400", C).unwrap(), Json::Double(f64::INFINITY));
        assert_eq!(parse_json("-0", C).unwrap(), Json::Int(0));
        assert!(
            parse_json("1e400", S)
                .unwrap_err()
                .message
                .starts_with("Parsing JSON failed: Number too big to be stored in double.")
        );
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\" 1}",
            "tru",
            "\"x",
            "01x",
            "1.",
            "1e",
            "[1] 2",
            "\"\\x\"",
            "\"\\ud800\"",
            "-",
        ] {
            assert!(parse_json(bad, C).is_err(), "{bad}");
            assert!(parse_json(bad, S).is_err(), "{bad}");
        }
        assert!(parse_json(&"[".repeat(1000), C).is_err());
        assert_eq!(
            Json::Map(vec![(
                "k".into(),
                Json::List(vec![Json::Int(1), Json::Double(2.0)])
            )])
            .java_string(),
            "{k=[1, 2.0]}"
        );
        assert_eq!(
            Json::Map(vec![("k".into(), Json::Str("v".into()))]).to_text(),
            r#"{"k":"v"}"#
        );
    }

    #[test]
    fn rapidjson_numbers() {
        assert_eq!(rapidjson_double(1.5), "1.5");
        assert_eq!(rapidjson_double(100.0), "100.0");
        assert_eq!(
            rapidjson_double(1.2345678901234568e29),
            "1.2345678901234568e29"
        );
        assert_eq!(rapidjson_double(1e21), "1e21");
        assert_eq!(rapidjson_double(1e20), "100000000000000000000.0");
        assert_eq!(rapidjson_double(0.0015), "0.0015");
        assert_eq!(rapidjson_double(1e-7), "1e-7");
        assert_eq!(rapidjson_double(-2.5e-10), "-2.5e-10");
        assert_eq!(rapidjson_double(0.0), "0.0");
        assert_eq!(quote_json("a\"b\\c\n\u{1}é/"), r#""a\"b\\c\n\u0001é/""#);
        assert_eq!(json_column_key("myColumn"), r#""\"myColumn\"""#);
        assert_eq!(json_column_key("ttl(b)"), r#""ttl(b)""#);
    }

    #[test]
    fn to_json_both_dialects() {
        use CqlType as T;
        // SELECT JSON / toJson on both images.
        for (t, v, c, s) in [
            (
                T::Float,
                Value::Float(123456789.0),
                "1.23456792E8",
                "1.2345679e+08",
            ),
            (T::Double, Value::Double(1e7), "1.0E7", "10000000"),
            (T::Float, Value::Float(f32::NAN), "null", "null"),
            (T::Double, Value::Double(f64::NEG_INFINITY), "null", "null"),
            (
                T::Decimal,
                Value::Decimal(Decimal::parse("1.10").unwrap()),
                "1.10",
                "1.1",
            ),
            (
                T::Varint,
                Value::Varint("12345678901234567890".parse().unwrap()),
                "12345678901234567890",
                "12345678901234567890",
            ),
            (
                T::Timestamp,
                Value::Timestamp(1_792_497_600_123),
                "\"2026-10-20 12:00:00.123Z\"",
                "\"2026-10-20 12:00:00.123Z\"",
            ),
            (
                T::Timestamp,
                Value::Timestamp(i64::MAX),
                "\"+292278994-08-17 07:12:55.807Z\"",
                "\"292278994-08-17 07:12:55.807Z\"",
            ),
            (
                T::Date,
                Value::Date((1 << 31) + 20_746),
                "\"2026-10-20\"",
                "\"2026-10-20\"",
            ),
            (
                T::Time,
                Value::Time(3_723_000_000_000),
                "\"01:02:03.000000000\"",
                "\"01:02:03.000000000\"",
            ),
            (
                T::Inet,
                Value::Inet("::1".parse().unwrap()),
                "\"0:0:0:0:0:0:0:1\"",
                "\"::1\"",
            ),
            (
                T::Blob,
                Value::Blob(Bytes::from_static(&[0xca, 0xfe])),
                "\"0xcafe\"",
                "\"0xcafe\"",
            ),
            (
                T::Text,
                Value::Text("héllo \"q\"".into()),
                r#""héllo \"q\"""#,
                r#""héllo \"q\"""#,
            ),
            (T::Boolean, Value::Boolean(true), "true", "true"),
            (
                T::Duration,
                Value::Duration(crate::Duration::new(14, 25, 18_367_008_009_010)),
                "\"1y2mo25d5h6m7s8ms9us10ns\"",
                "\"1y2mo25d5h6m7s8ms9us10ns\"",
            ),
            (
                T::Bigint,
                Value::Bigint(-9_000_000_000),
                "-9000000000",
                "-9000000000",
            ),
        ] {
            assert_eq!(tj(&t, v.clone(), C), c, "{t:?}");
            assert_eq!(tj(&t, v, S), s, "{t:?}");
        }
        assert_eq!(to_json(&T::Int, &[], C).unwrap(), "\"\"");
        let m = T::map(T::Int, T::Text, false);
        let v = Value::Map(vec![
            (Value::Int(1), Value::Text("a\"b".into())),
            (Value::Int(2), Value::Text("c\\d".into())),
        ]);
        assert_eq!(tj(&m, v.clone(), C), r#"{"1": "a\"b", "2": "c\\d"}"#);
        assert_eq!(tj(&m, v, S), r#"{"1": "a\"b", "2": "c\\d"}"#);
        let mf = T::map(T::list(T::Int, true), T::Int, false);
        let v = Value::Map(vec![(
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::Int(3),
        )]);
        assert_eq!(tj(&mf, v, C), r#"{"[1, 2]": 3}"#);
        let bm = T::map(T::Boolean, T::Int, false);
        assert_eq!(
            tj(
                &bm,
                Value::Map(vec![(Value::Boolean(true), Value::Int(1))]),
                S
            ),
            r#"{"true": 1}"#
        );
        let dm = T::map(T::Double, T::Int, false);
        assert_eq!(
            tj(
                &dm,
                Value::Map(vec![(Value::Double(1.5), Value::Int(1))]),
                S
            ),
            r#"{"1.5": 1}"#
        );
        let tt = T::Tuple(vec![T::Int, T::Text, T::Boolean]);
        assert_eq!(
            tj(&tt, Value::Tuple(vec![Value::Int(1), Value::Null]), C),
            "[1, null, null]"
        );
        let a = addr();
        let v = Value::Udt(vec![Value::Text("s".into()), Value::Int(5)]);
        assert_eq!(
            tj(&a, v.clone(), C),
            r#"{"street": "s", "\"Zip\"": 5, "select": null}"#
        );
        assert_eq!(tj(&a, v, S), r#"{"street": "s", "Zip": 5, "select": null}"#);
        let vf = T::vector(T::Float, 2);
        assert_eq!(
            tj(
                &vf,
                Value::Vector(vec![Value::Float(1.5), Value::Float(2.0)]),
                S
            ),
            "[1.5, 2]"
        );
        assert_eq!(
            tj(
                &vf,
                Value::Vector(vec![Value::Float(1.5), Value::Float(2.0)]),
                C
            ),
            "[1.5, 2.0]"
        );
        let st = T::set(T::Inet, false);
        assert_eq!(
            tj(
                &st,
                Value::Set(vec![
                    Value::Inet("1.2.3.4".parse().unwrap()),
                    Value::Inet("::2".parse().unwrap())
                ]),
                S
            ),
            r#"["::2", "1.2.3.4"]"#
        );
        let l = T::list(T::Int, false);
        assert_eq!(tj(&l, Value::List(vec![]), C), "[]");
        assert!(to_json(&l, &[0, 0, 0, 1], C).is_err());
        assert!(to_json(&vf, &[0], C).is_err());
        assert!(to_json(&T::Timestamp, &[0], C).is_err());
    }

    #[test]
    fn from_json_cassandra() {
        use CqlType as T;
        // INSERT JSON on cassandra:5.0.9 (messages after "Error decoding JSON value for <col>: ").
        for (t, text, msg) in [
            (
                T::Timestamp,
                "1000",
                "Expected a long or a datestring representation of a timestamp value, but got a Integer: 1000",
            ),
            (
                T::Timestamp,
                "1.5",
                "Expected a long or a datestring representation of a timestamp value, but got a Double: 1.5",
            ),
            (
                T::Int,
                "1.5",
                "Expected an int value, but got a Double: 1.5",
            ),
            (
                T::Int,
                "2147483648",
                "Expected an int value, but got a Long: 2147483648",
            ),
            (
                T::Int,
                "100.0",
                "Expected an int value, but got a Double: 100.0",
            ),
            (
                T::Int,
                "1e2",
                "Expected an int value, but got a Double: 100.0",
            ),
            (T::Int, "\"abc\"", "Unable to make int from 'abc'"),
            (
                T::Bigint,
                "1.0",
                "Expected a bigint value, but got a Double: 1.0",
            ),
            (
                T::Bigint,
                "9223372036854775808",
                "Expected a bigint value, but got a BigInteger: 9223372036854775808",
            ),
            (
                T::Boolean,
                "1",
                "Expected a boolean value, but got a Integer: 1",
            ),
            (
                T::Blob,
                "\"cafe\"",
                "Value 'cafe' is not a valid blob representation: String representation of blob is missing 0x prefix: cafe",
            ),
            (T::Tinyint, "300", "Unable to make byte from '300'"),
            (
                T::Date,
                "5",
                "Expected a string representation of a date value, but got a Integer: 5",
            ),
            (
                T::Text,
                "5",
                "Expected a UTF-8 string, but got a Integer: 5",
            ),
            (
                T::Uuid,
                "5",
                "Expected a string representation of a uuid, but got a Integer: 5",
            ),
            (
                T::Decimal,
                "\"abc\"",
                "Value 'abc' is not a valid representation of a decimal value",
            ),
            (
                T::Decimal,
                "1e400",
                "Value 'Infinity' is not a valid representation of a decimal value",
            ),
            (
                T::Float,
                "true",
                "Expected a float value, but got a Boolean: true",
            ),
            (
                T::Varint,
                "1.5",
                "Value '1.5' is not a valid representation of a varint value",
            ),
            (
                T::list(T::Int, false),
                "[1, null]",
                "Invalid null element in list",
            ),
            (
                T::set(T::Int, false),
                "[null]",
                "Invalid null element in set",
            ),
            (
                T::map(T::Int, T::Text, false),
                r#"{"x": "a"}"#,
                "Unable to make int from 'x'",
            ),
            (
                T::map(T::Int, T::Text, false),
                r#"{"1": null}"#,
                "Invalid null value in map",
            ),
            (
                T::map(T::Int, T::Text, false),
                "[1]",
                "Expected a map, but got a ArrayList: [1]",
            ),
            (
                T::Tuple(vec![T::Int, T::Text]),
                "[1]",
                "Tuple is missing items (expected 2): [1]",
            ),
            (
                T::Tuple(vec![T::Int, T::Text]),
                r#"[1, "a", 2]"#,
                "Tuple contains extra items (expected 2): [1, a, 2]",
            ),
            (
                T::Tuple(vec![T::Int]),
                "5",
                "Expected a list representation of a tuple, but got a Integer: 5",
            ),
            (
                addr(),
                r#"{"nope": 1}"#,
                "Unknown field 'nope' in value of user defined type addr",
            ),
            (
                T::vector(T::Float, 2),
                "[1, 2, 3]",
                "List had incorrect size: expected 2 but given 3; [1, 2, 3]",
            ),
            (
                T::vector(T::Float, 2),
                "[1, null]",
                "Invalid null element in list",
            ),
            (
                T::list(T::Int, false),
                "{}",
                "Expected a list, but got a LinkedHashMap: {}",
            ),
            (
                T::set(T::Int, false),
                "1",
                "Expected a list (representing a set), but got a Integer: 1",
            ),
            (
                T::Duration,
                "1",
                "Expected a string representation of a duration, but got a Integer: 1",
            ),
            (T::Empty, "1", "Expected an empty string, but got: 1"),
            (T::Empty, "\"x\"", "'x' is not empty"),
        ] {
            assert_eq!(fj(&t, text, C).unwrap_err(), msg, "{t:?} {text}");
        }
        for (t, text, want) in [
            (T::Int, "\"12\"", 12i32.to_be_bytes().to_vec()),
            (T::Int, "-0", vec![0, 0, 0, 0]),
            (T::Bigint, "\"123\"", 123i64.to_be_bytes().to_vec()),
            (T::Bigint, "5", 5i64.to_be_bytes().to_vec()),
            (T::Float, "\"1.5\"", 1.5f32.to_be_bytes().to_vec()),
            (T::Float, "1", 1.0f32.to_be_bytes().to_vec()),
            (T::Double, "2", 2.0f64.to_be_bytes().to_vec()),
            (T::Boolean, "\"TRUE\"", vec![1]),
            (T::Blob, "\"0xCAFE\"", vec![0xca, 0xfe]),
            (
                T::Timestamp,
                "\"2026-10-20 12:00:00\"",
                1_792_497_600_000i64.to_be_bytes().to_vec(),
            ),
            (
                T::Timestamp,
                "10000000000",
                10_000_000_000i64.to_be_bytes().to_vec(),
            ),
            (T::Varint, "\"12\"", vec![12]),
            (T::Smallint, "1.0", vec![]),
        ] {
            match fj(&t, text, C) {
                Ok(b) => assert_eq!(b.to_vec(), want, "{t:?} {text}"),
                Err(e) => assert!(want.is_empty(), "{t:?} {text}: {e}"),
            }
        }
        // 12345678901234567890.123… is a Double to Jackson.
        let d = fj(&T::Decimal, "12345678901234567890.12345678901234567890", C).unwrap();
        assert_eq!(
            Decimal::from_bytes(&d).unwrap().to_java_string(),
            "1.2345678901234567E+19"
        );
        // Collections: strings hold JSON, sets sort and de-duplicate, quoted UDT keys keep case.
        let l = fj(&T::list(T::Int, false), "\"[1,2]\"", C).unwrap();
        assert_eq!(to_json(&T::list(T::Int, false), &l, C).unwrap(), "[1, 2]");
        let s = fj(&T::set(T::Text, false), r#"["b", "a", "a"]"#, C).unwrap();
        assert_eq!(
            to_json(&T::set(T::Text, false), &s, C).unwrap(),
            r#"["a", "b"]"#
        );
        let mf = T::map(T::list(T::Int, true), T::Int, false);
        assert_eq!(
            to_json(&mf, &fj(&mf, r#"{"[1, 2]": 3}"#, C).unwrap(), C).unwrap(),
            r#"{"[1, 2]": 3}"#
        );
        let a = fj(&addr(), r#"{"street": "s", "\"Zip\"": 5}"#, C).unwrap();
        assert_eq!(
            to_json(&addr(), &a, C).unwrap(),
            r#"{"street": "s", "\"Zip\"": 5, "select": null}"#
        );
        let a = fj(&addr(), r#"{"STREET": "s"}"#, C).unwrap();
        assert_eq!(
            to_json(&addr(), &a, C).unwrap(),
            r#"{"street": "s", "\"Zip\"": null, "select": null}"#
        );
        let t = fj(&T::Tuple(vec![T::Int, T::Text]), "[null, \"x\"]", C).unwrap();
        assert_eq!(
            to_json(&T::Tuple(vec![T::Int, T::Text]), &t, C).unwrap(),
            "[null, \"x\"]"
        );
        assert!(fj(&T::Counter, "1", C).is_err());
        assert_eq!(
            fj(&T::list(T::Int, false), "\"[1,\"", C).unwrap_err(),
            "Error decoding JSON string: Unexpected end-of-input within/between Array entries\n at [Source: REDACTED (`StreamReadFeature.INCLUDE_SOURCE_IN_LOCATION` disabled); line: 1, column: 4]"
        );
        assert_eq!(
            fj(&T::Blob, "5", C).unwrap_err(),
            "Value '5' is not a valid blob representation: class java.lang.Integer cannot be cast to class java.lang.String (java.lang.Integer and java.lang.String are in module java.base of loader 'bootstrap')"
        );
        for (t, text, msg) in [
            (
                T::Tuple(vec![T::Int, T::Text]),
                r#"[1, "a", 3]"#,
                "Tuple contains extra items (expected 2): [1, a, 3]",
            ),
            (
                T::vector(T::Float, 2),
                "[1]",
                "List had incorrect size: expected 2 but given 1; [1]",
            ),
            (
                T::list(T::Int, false),
                "5",
                "Expected a list, but got a Integer: 5",
            ),
            (
                T::map(T::Int, T::Int, false),
                "5",
                "Expected a map, but got a Integer: 5",
            ),
            (addr(), "5", "Expected a map, but got a Integer: 5"),
            (
                T::Inet,
                "5",
                "Expected a string representation of an inet value, but got a Integer: 5",
            ),
            (
                T::Time,
                "5",
                "Expected a string representation of a time value, but got a Integer: 5",
            ),
            (
                T::Timeuuid,
                "5",
                "Expected a string representation of a timeuuid, but got a Integer: 5",
            ),
            (
                T::Decimal,
                "true",
                "Value 'true' is not a valid representation of a decimal value",
            ),
            (
                T::list(T::Int, false),
                "[true]",
                "Expected an int value, but got a Boolean: true",
            ),
            (
                T::Varint,
                "true",
                "Value 'true' is not a valid representation of a varint value",
            ),
            (
                T::set(T::Int, false),
                "[1, null]",
                "Invalid null element in set",
            ),
            (
                T::Int,
                "1e400",
                "Expected an int value, but got a Double: Infinity",
            ),
        ] {
            assert_eq!(fj(&t, text, C).unwrap_err(), msg, "{t:?} {text}");
        }
        assert!(
            fj(&T::Blob, "[1]", C)
                .unwrap_err()
                .contains("class java.util.ArrayList cannot be cast")
        );
        assert!(
            fj(&T::list(T::Int, false), "\"{\\\"a\\\"\"", C)
                .unwrap_err()
                .starts_with("Error decoding JSON string: ")
        );
        assert!(
            fj(&T::Blob, "\"0xzz\"", C)
                .unwrap_err()
                .starts_with("Value '0xzz' is not a valid blob representation: cannot parse")
        );
    }

    #[test]
    fn from_json_scylla() {
        use CqlType as T;
        // INSERT JSON on scylladb/scylla:2026.3.3.
        for (t, text, msg) in [
            (
                T::Varint,
                "123456789012345678901234567890",
                "marshaling error: unable to make int from '1.2345678901234568e29'",
            ),
            (
                T::Varint,
                "1.5",
                "marshaling error: unable to make int from '1.5'",
            ),
            (
                T::Bigint,
                "1.5",
                "marshaling error: Incorrect JSON floating-point value for int64 type: 1.5 (it should not contain fractional part 0.5)",
            ),
            (
                T::Timestamp,
                "1.5",
                "marshaling error: Incorrect JSON floating-point value for int64 type: 1.5 (it should not contain fractional part 0.5)",
            ),
            (
                T::Int,
                "2147483648",
                "marshaling error: Value 2147483648 out of range",
            ),
            (
                T::Tinyint,
                "300",
                "marshaling error: Value 300 out of range",
            ),
            (
                T::Bigint,
                "9223372036854775808",
                "marshaling error: Value 9223372036854775808 out of range",
            ),
            (T::Boolean, "1", "marshaling error: Invalid JSON object 1"),
            (
                T::Boolean,
                "\"yes\"",
                "marshaling error: Invalid JSON object \"yes\"",
            ),
            (
                T::Blob,
                "\"cafe\"",
                "marshaling error: Blob JSON strings must start with 0x",
            ),
            (T::Blob, "\"0xzz\"", "Non-hex characters in zz"),
            (
                addr(),
                r#"{"street": "s", "\"Zip\"": 5}"#,
                "marshaling error: Extraneous field definition for user type addr: \"Zip\"",
            ),
            (
                addr(),
                r#"{"nope": 1}"#,
                "marshaling error: Extraneous field definition for user type addr: nope",
            ),
            (
                T::list(T::Int, false),
                "[1, null]",
                "marshaling error: Incorrect JSON value for int64 type: null",
            ),
            (
                T::Date,
                "5",
                "marshaling error: simple_date_type must be represented as string in JSON, instead got '5'",
            ),
            (
                T::vector(T::Float, 2),
                "[1, 2, 3]",
                "marshaling error: Too many values (3) for vector with size 2",
            ),
            (
                T::Text,
                "5",
                "rjson::to_string_view expects a string value, got: kNumberType",
            ),
            (
                T::Uuid,
                "5",
                "marshaling error: uuid_type must be represented as string in JSON, instead got '5'",
            ),
            (
                T::Uuid,
                "\"zzz\"",
                "marshaling error: Cannot parse uuid from 'zzz'",
            ),
            (
                T::Timeuuid,
                "\"550e8400-e29b-41d4-a716-446655440000\"",
                "marshaling error: Unsupported UUID version (4)",
            ),
            (
                T::Decimal,
                "\"abc\"",
                "marshaling error: unable to make BigDecimal from 'abc'",
            ),
            (
                T::Float,
                "true",
                "marshaling error: JSON value must be represented as double or string",
            ),
            (
                T::map(T::Int, T::Text, false),
                r#"{"x": "a"}"#,
                "marshaling error: Invalid number format 'x'",
            ),
            (
                T::Int,
                "\"abc\"",
                "marshaling error: Invalid number format 'abc'",
            ),
            (
                T::Int,
                "\" 5\"",
                "marshaling error: Invalid number format ' 5'",
            ),
            (
                T::Int,
                "\"1.0\"",
                "marshaling error: Invalid number format '1.0'",
            ),
            (
                T::Double,
                "\"0x10\"",
                "marshaling error: Invalid number format '0x10'",
            ),
            (
                T::Float,
                "\"1e40\"",
                "marshaling error: Invalid number format '1e40'",
            ),
            (
                T::Inet,
                "\"1.2.3\"",
                "marshaling error: Failed to parse inet_addr from '1.2.3'",
            ),
            (
                T::Duration,
                "\"xyz\"",
                "marshaling error: Unable to convert 'xyz' to a duration",
            ),
        ] {
            assert_eq!(fj(&t, text, S).unwrap_err(), msg, "{t:?} {text}");
        }
        for (t, text, want) in [
            (T::Bigint, "1.0", 1i64.to_be_bytes().to_vec()),
            (T::Int, "1e2", 100i32.to_be_bytes().to_vec()),
            (T::Int, "-0", vec![0, 0, 0, 0]),
            (T::Int, "\"+5\"", 5i32.to_be_bytes().to_vec()),
            (T::Timestamp, "1000", 1000i64.to_be_bytes().to_vec()),
            (
                T::Varint,
                "12345678901234567890",
                crate::num::parse_varint("12345678901234567890")
                    .unwrap()
                    .to_signed_bytes_be(),
            ),
            (T::Varint, "\"-0012\"", vec![0xf6]),
            (T::Boolean, "\"True\"", vec![1]),
            (
                T::Float,
                "\"Infinity\"",
                f32::INFINITY.to_be_bytes().to_vec(),
            ),
            (T::Float, "1", 1.0f32.to_be_bytes().to_vec()),
            (T::Double, "\"nan\"", f64::NAN.to_be_bytes().to_vec()),
            (
                T::Uuid,
                "\"550E8400-E29B-41D4-A716-446655440000\"",
                crate::timeuuid::parse_uuid("550e8400-e29b-41d4-a716-446655440000")
                    .unwrap()
                    .to_vec(),
            ),
        ] {
            let got = fj(&t, text, S).unwrap_or_else(|e| panic!("{t:?} {text}: {e}"));
            if matches!(t, T::Double) {
                assert!(f64::from_be_bytes(got[..].try_into().unwrap()).is_nan());
            } else {
                assert_eq!(got.to_vec(), want, "{t:?} {text}");
            }
        }
        let dec = |text: &str| {
            Decimal::from_bytes(&fj(&T::Decimal, text, S).unwrap())
                .unwrap()
                .to_scylla_string()
        };
        assert_eq!(
            dec("12345678901234567890.12345678901234567890"),
            "12345678901234567000"
        );
        assert_eq!(dec("\"1e5\""), "100000");
        assert_eq!(dec("\"1.5e-3\""), "0.0015");
        assert_eq!(dec("1.5e-3"), "0.0015");
        // Tuples may be short; UDT keys match exactly.
        let tt = T::Tuple(vec![T::Int, T::Text]);
        assert_eq!(
            to_json(&tt, &fj(&tt, "[1]", S).unwrap(), S).unwrap(),
            "[1, null]"
        );
        let a = fj(&addr(), r#"{"street": "st", "Zip": 1}"#, S).unwrap();
        assert_eq!(
            to_json(&addr(), &a, S).unwrap(),
            r#"{"street": "st", "Zip": 1, "select": null}"#
        );
        let mm = T::map(T::Tuple(vec![T::Int, T::Int]), T::Text, false);
        assert_eq!(
            to_json(&mm, &fj(&mm, r#"{"[1, 2]": "z"}"#, S).unwrap(), S).unwrap(),
            r#"{"[1, 2]": "z"}"#
        );
        for (t, text, msg) in [
            (
                T::vector(T::Float, 2),
                "[1]",
                "Too few values (1) for vector with size 2",
            ),
            (
                T::Tuple(vec![T::Int, T::Text]),
                r#"[1, "a", 3]"#,
                "Too many values (3) for tuple with size 2",
            ),
            (
                T::list(T::Int, false),
                "5",
                "list_type must be represented as JSON Array",
            ),
            (
                T::list(T::Int, false),
                "\"[1,\"",
                "list_type must be represented as JSON Array",
            ),
            (
                T::list(T::Int, false),
                "\"[1]\"",
                "list_type must be represented as JSON Array",
            ),
            (
                T::list(T::Int, false),
                "[true]",
                "Incorrect JSON value for int64 type: true",
            ),
            (
                T::set(T::Int, false),
                "[1, null]",
                "Incorrect JSON value for int64 type: null",
            ),
            (
                T::Tuple(vec![T::Int, T::Text]),
                r#""[1, \"a\"]""#,
                "tuple_type must be represented as JSON Array",
            ),
            (
                T::map(T::Int, T::Int, false),
                "5",
                "map_type must be represented as JSON Object",
            ),
            (addr(), "5", "user_type must be represented as JSON Object"),
            (
                T::Inet,
                "5",
                "inet_addr_type must be represented as string in JSON, instead got '5'",
            ),
            (
                T::Time,
                "5",
                "time_type must be represented as string in JSON, instead got '5'",
            ),
            (
                T::Timeuuid,
                "5",
                "timeuuid_type must be represented as string in JSON, instead got '5'",
            ),
            (
                T::Duration,
                "5",
                "duration_type must be represented as string in JSON, instead got '5'",
            ),
            (
                T::Blob,
                "5",
                "bytes_type must be represented as string in JSON, instead got '5'",
            ),
            (
                T::Decimal,
                "true",
                "true must be represented as numeric or string in JSON",
            ),
            (T::Varint, "true", "unable to make int from 'true'"),
            (T::Int, "true", "Incorrect JSON value for int64 type: true"),
        ] {
            assert_eq!(
                fj(&t, text, S).unwrap_err(),
                format!("marshaling error: {msg}"),
                "{t:?} {text}"
            );
        }
        assert_eq!(
            fj(&T::Int, "1e400", S).unwrap_err(),
            "Parsing JSON failed: Number too big to be stored in double. at 0"
        );
        assert!(
            fj(&T::Text, "\"x", S)
                .unwrap_err()
                .starts_with("Parsing JSON failed: ")
        );
        assert_eq!(
            json_number_decimal(&Json::Double(1.5))
                .unwrap()
                .to_java_string(),
            "1.5"
        );
    }

    /// Probed on ScyllaDB: `fromJson('""')` is no int; blob map keys are `0x…` JSON strings.
    #[test]
    fn scylla_empty_ints_and_blob_keys() {
        let sc = Dialect::Scylla;
        let j = |t: &str| parse_json(t, sc).unwrap();
        assert_eq!(
            from_json(&CqlType::Int, &j("\"\""), sc)
                .unwrap_err()
                .message,
            "marshaling error: Invalid number format ''"
        );
        let m = CqlType::map(CqlType::Blob, CqlType::Int, false);
        let b = from_json(&m, &j(r#"{"0x12ab3e": 1}"#), sc).unwrap();
        assert_eq!(
            crate::map_entries(&b).unwrap(),
            vec![(&[0x12u8, 0xab, 0x3e][..], &1i32.to_be_bytes()[..])]
        );
        assert_eq!(
            from_json(&m, &j(r#"{"12ab3e": 1}"#), sc)
                .unwrap_err()
                .message,
            "marshaling error: Blob JSON strings must start with 0x"
        );
    }
}
