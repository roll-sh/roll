//! The value model shared by test files, request bodies, responses and the stash.
//!
//! Elasticsearch's Java runner works on `Map<String, Object>` trees produced by Jackson from YAML
//! (test files) and JSON (responses). Its comparison rules depend on the Java classes involved:
//! integers and floating point numbers are different classes, so a nested `1` never equals `1.0`,
//! while top-level `match` compares any two numbers by their double value. [`Value`] keeps that
//! distinction (`Int` vs `Float`) and nothing else: Java's `Integer`/`Long`/`BigInteger` split
//! never matters because Jackson picks the same class for the same magnitude on both sides.
//!
//! Maps keep insertion order (`IndexMap`) so reports and `_arbitrary_key_` are deterministic and
//! request bodies go out the way the test wrote them; equality ignores order like `Map.equals`.
//!
//! [`Value::java_string`] renders `Object.toString()` the way the Java runner does, because that
//! string is what `${stash}` interpolation, query parameters and `catch: /regex/` work on.

use std::fmt::{self, Write as _};

use indexmap::IndexMap;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

/// An ordered JSON object.
pub type Map = IndexMap<String, Value>;

/// A JSON/YAML value with Java's number classes collapsed to integer vs floating point.
#[derive(Clone, Debug, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    /// Any integer (Java `Integer`, `Long` or `BigInteger` within 128 bits).
    Int(i128),
    /// A floating point number (Java `Double`).
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Map(Map),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Map> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The value as a double if it is a number (Java `Number.doubleValue()`).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => i64::try_from(*i).ok(),
            _ => None,
        }
    }

    pub fn is_number(&self) -> bool {
        matches!(self, Value::Int(_) | Value::Float(_))
    }

    /// Looks up a key of a map.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_map().and_then(|m| m.get(key))
    }

    /// Java's simple class name, for messages that mirror the Java runner's.
    pub fn java_class(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "Boolean",
            Value::Int(i) => {
                if i32::try_from(*i).is_ok() {
                    "Integer"
                } else if i64::try_from(*i).is_ok() {
                    "Long"
                } else {
                    "BigInteger"
                }
            }
            Value::Float(_) => "Double",
            Value::Str(_) => "String",
            Value::List(_) => "ArrayList",
            Value::Map(_) => "LinkedHashMap",
        }
    }

    /// `Object.toString()` as the Java runner would produce it.
    pub fn java_string(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            _ => {
                let mut out = String::new();
                self.write_java(&mut out);
                out
            }
        }
    }

    fn write_java(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Value::Float(f) => out.push_str(&java_double(*f)),
            Value::Str(s) => out.push_str(s),
            Value::List(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    item.write_java(out);
                }
                out.push(']');
            }
            Value::Map(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(k);
                    out.push('=');
                    v.write_java(out);
                }
                out.push('}');
            }
        }
    }

    /// Compact JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "null".into())
    }

    /// Parses one JSON document.
    pub fn from_json(bytes: &[u8]) -> Result<Value, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Parses a stream of concatenated or newline-delimited JSON documents (an NDJSON body).
    pub fn from_json_stream(text: &str) -> Result<Vec<Value>, serde_json::Error> {
        let mut out = Vec::new();
        for v in serde_json::Deserializer::from_str(text).into_iter::<Value>() {
            out.push(v?);
        }
        Ok(out)
    }
}

/// `Double.toString(d)`: the shortest representation that round-trips, in plain notation for
/// magnitudes in `[1e-3, 1e7)` and computerized scientific notation (`1.0E10`) otherwise.
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
    // `{:e}` gives the shortest round-trip digits: "-1.2345e-7".
    let sci = format!("{d:e}");
    let (mantissa, exp) = sci.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("exponent digits");
    let (neg, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    let abs = d.abs();
    if (1e-3..1e7).contains(&abs) {
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
        let _ = write!(out, "E{exp}");
    }
    out
}

impl PartialEq for Value {
    /// Java `equals` semantics: maps ignore order, integers never equal floats.
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            // Double.equals compares bits, so NaN equals NaN and 0.0 differs from -0.0.
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| v == w))
            }
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_json())
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Value {
        Value::Int(i as i128)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Value {
        Value::Float(f)
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Null => s.serialize_unit(),
            Value::Bool(b) => s.serialize_bool(*b),
            Value::Int(i) => {
                if let Ok(v) = i64::try_from(*i) {
                    s.serialize_i64(v)
                } else if let Ok(v) = u64::try_from(*i) {
                    s.serialize_u64(v)
                } else {
                    s.serialize_i128(*i)
                }
            }
            Value::Float(f) => {
                if f.is_finite() {
                    s.serialize_f64(*f)
                } else {
                    // Jackson writes non-finite doubles as strings ("NaN"); JSON has no literal.
                    s.serialize_str(&java_double(*f))
                }
            }
            Value::Str(v) => s.serialize_str(v),
            Value::List(items) => {
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Value::Map(map) => {
                let mut m = s.serialize_map(Some(map.len()))?;
                for (k, v) in map {
                    m.serialize_entry(k, v)?;
                }
                m.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
        d.deserialize_any(ValueVisitor)
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Int(v as i128))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Int(v as i128))
    }

    fn visit_i128<E>(self, v: i128) -> Result<Value, E> {
        Ok(Value::Int(v))
    }

    fn visit_u128<E: de::Error>(self, v: u128) -> Result<Value, E> {
        Ok(i128::try_from(v).map(Value::Int).unwrap_or(Value::Float(v as f64)))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(Value::Float(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::Str(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::Str(v))
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        Value::deserialize(d)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        // Never trust a size hint from the input: grow as elements actually arrive.
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::List(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut map = Map::new();
        // Jackson's STRICT_DUPLICATE_DETECTION, on in every Elasticsearch XContent parser: a
        // response (or body) that repeats a field is invalid, not "last one wins". The check
        // runs before the value is parsed, as Jackson reports it at the field name.
        while let Some(k) = access.next_key::<String>()? {
            match map.entry(k) {
                indexmap::map::Entry::Occupied(e) => return Err(de::Error::custom(format_args!("Duplicate field '{}'", e.key()))),
                indexmap::map::Entry::Vacant(e) => {
                    e.insert(access.next_value::<Value>()?);
                }
            }
        }
        Ok(Value::Map(map))
    }
}

/// Builds a [`Value`] from JSON-like literal syntax in tests.
#[cfg(test)]
pub(crate) fn json(text: &str) -> Value {
    Value::from_json(text.as_bytes()).expect("valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_double_formatting() {
        // Expected strings are what Java 21's Double.toString prints.
        let cases = [
            (1.0, "1.0"),
            (-1.0, "-1.0"),
            (0.1, "0.1"),
            (100.0, "100.0"),
            (1234567.0, "1234567.0"),
            (1.0e7, "1.0E7"),
            (12345678.9, "1.23456789E7"),
            (0.001, "0.001"),
            (0.0001, "1.0E-4"),
            (1.5e-10, "1.5E-10"),
            (3.4028234663852886e38, "3.4028234663852886E38"),
            (0.30000000000000004, "0.30000000000000004"),
            (2.5, "2.5"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
        ];
        for (d, want) in cases {
            assert_eq!(java_double(d), want, "{d}");
        }
    }

    #[test]
    fn java_to_string() {
        let v = json(r#"{"a":[1,2.0,"x",null,true],"b":{"c":{}}}"#);
        assert_eq!(v.java_string(), "{a=[1, 2.0, x, null, true], b={c={}}}");
        assert_eq!(Value::Str("plain".into()).java_string(), "plain");
    }

    #[test]
    fn equality_follows_java() {
        assert_eq!(json(r#"{"a":1,"b":2}"#), json(r#"{"b":2,"a":1}"#));
        assert_ne!(json("1"), json("1.0"));
        assert_ne!(json("[1,2]"), json("[2,1]"));
        assert_ne!(json(r#"{"a":1}"#), json(r#"{"a":1,"b":null}"#));
        assert_eq!(json("18446744073709551615"), Value::Int(18446744073709551615));
    }

    #[test]
    fn json_round_trip_keeps_order_and_numbers() {
        let text = r#"{"z":1,"a":[1.5,-3,18446744073709551615],"m":{"y":null,"b":false}}"#;
        assert_eq!(json(text).to_json(), text);
    }

    #[test]
    fn floats_parse_correctly_rounded() {
        // serde_json's default fast path is off by one ulp on these (seen in Elasticsearch and
        // OpenSearch responses); Java's parser, and so the expected values, round correctly.
        for (text, bits) in [
            ("1.1258999068426289E14", 112589990684262.89f64.to_bits()),
            ("-91.70509831248421", (-91.70509831248421f64).to_bits()),
            ("1.0451733580394421E38", 1.0451733580394421e38f64.to_bits()),
            ("3.9662131341174245", 3.9662131341174245f64.to_bits()),
        ] {
            assert_eq!(json(text).as_f64().unwrap().to_bits(), bits, "{text}");
        }
    }

    #[test]
    fn ndjson_stream() {
        let v = Value::from_json_stream("{\"index\":{}}\n{\"f\":1}\n").unwrap();
        assert_eq!(v.len(), 2);
        assert!(Value::from_json_stream("{\"a\":").is_err());
        assert!(Value::from_json_stream("  \n").unwrap().is_empty());
    }
}
