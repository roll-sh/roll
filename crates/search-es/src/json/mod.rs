//! JSON values as Elasticsearch sees them.
//!
//! Elasticsearch parses request bodies with Jackson into ordered maps and prints responses in a
//! defined field order, so roll uses its own value type instead of `serde_json::Value`: objects
//! keep insertion order (`IndexMap`), and numbers keep Jackson's distinction between integers
//! (`int`/`long`/`BigInteger`) and doubles, which is visible when a value is echoed back
//! (`1.0` stays `1.0`, `1e300` prints as `1.0E300`, as `Double.toString` does).
//!
//! - [`parse`] is a byte-level parser that reproduces Jackson's error messages, locations and
//!   leniencies (comments, duplicate-key detection, trailing content ignored), so
//!   `x_content_parse_exception` reasons match Elasticsearch's text.
//! - [`write`] renders compact or Jackson-style pretty JSON.

pub mod locate;
pub mod parse;
pub mod write;

use indexmap::IndexMap;
use std::fmt;

pub use parse::{JsonError, parse, parse_object};

/// An ordered JSON object.
pub type Map = IndexMap<String, Value>;

/// A JSON number, keeping the integer/floating distinction Jackson makes.
#[derive(Clone, Debug)]
pub enum Number {
    /// Integers that fit an `i64`.
    Int(i64),
    /// Integers in `(i64::MAX, u64::MAX]`.
    UInt(u64),
    /// Integers beyond `u64` (Jackson's `BigInteger`), as their decimal digits.
    Big(Box<str>),
    /// Anything with a fraction or an exponent.
    Float(f64),
}

impl Number {
    pub fn as_f64(&self) -> f64 {
        match self {
            Number::Int(i) => *i as f64,
            Number::UInt(u) => *u as f64,
            Number::Big(s) => s.parse().unwrap_or(f64::NAN),
            Number::Float(f) => *f,
        }
    }

    /// The value as an `i64` if it is an integer in range.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Number::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn is_integer(&self) -> bool {
        !matches!(self, Number::Float(_))
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Number::Int(a), Number::Int(b)) => a == b,
            (Number::UInt(a), Number::UInt(b)) => a == b,
            (Number::Big(a), Number::Big(b)) => a == b,
            (Number::Float(a), Number::Float(b)) => a.to_bits() == b.to_bits() || a == b,
            _ => false,
        }
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Number::Int(i) => write!(f, "{i}"),
            Number::UInt(u) => write!(f, "{u}"),
            Number::Big(s) => f.write_str(s),
            Number::Float(x) => f.write_str(&write::java_double(*x)),
        }
    }
}

/// A parsed JSON value.
///
/// `Clone` and `PartialEq` are written by hand: values nest up to the parser's 1000 levels
/// (Jackson's limit, which Elasticsearch accepts in `_meta` and settings), and the derived
/// impls go through several `IndexMap`/iterator frames per level, which overflows a 2 MiB
/// worker stack in debug builds. These recurse with one small frame per level.
#[derive(Debug, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Num(Number),
    Str(String),
    Arr(Vec<Value>),
    Obj(Map),
}

impl Clone for Value {
    fn clone(&self) -> Self {
        match self {
            Value::Null => Value::Null,
            Value::Bool(b) => Value::Bool(*b),
            Value::Num(n) => Value::Num(n.clone()),
            Value::Str(s) => Value::Str(s.clone()),
            Value::Arr(a) => {
                let mut out = Vec::with_capacity(a.len());
                for v in a {
                    out.push(clone_child(v));
                }
                Value::Arr(out)
            }
            Value::Obj(m) => {
                let mut out = Map::with_capacity(m.len());
                for (k, v) in m {
                    out.insert(k.clone(), clone_child(v));
                }
                Value::Obj(out)
            }
        }
    }
}

/// Containers recurse through the stack guard; scalars don't need it.
#[inline]
fn clone_child(v: &Value) -> Value {
    match v {
        Value::Obj(_) | Value::Arr(_) => crate::stack::maybe_grow(|| v.clone()),
        _ => v.clone(),
    }
}

#[inline]
fn eq_child(a: &Value, b: &Value) -> bool {
    match a {
        Value::Obj(_) | Value::Arr(_) => crate::stack::maybe_grow(|| a.eq(b)),
        _ => a.eq(b),
    }
}

impl PartialEq for Value {
    /// Objects compare as maps (key order doesn't matter), as `IndexMap`'s equality does.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Arr(a), Value::Arr(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                for (x, y) in a.iter().zip(b) {
                    if !eq_child(x, y) {
                        return false;
                    }
                }
                true
            }
            (Value::Obj(a), Value::Obj(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                for (k, x) in a {
                    match b.get(k) {
                        Some(y) if eq_child(x, y) => {}
                        _ => return false,
                    }
                }
                true
            }
            _ => false,
        }
    }
}

impl Value {
    pub fn object() -> Value {
        Value::Obj(Map::new())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Obj(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Obj(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<&Number> {
        match self {
            Value::Num(n) => Some(n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.as_number().and_then(Number::as_i64)
    }

    pub fn as_f64(&self) -> Option<f64> {
        self.as_number().map(Number::as_f64)
    }

    /// `self[key]` for objects, `None` otherwise.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object().and_then(|m| m.get(key))
    }

    /// Follows a dotted path of object keys.
    pub fn pointer(&self, path: &[&str]) -> Option<&Value> {
        let mut v = self;
        for p in path {
            v = v.get(p)?;
        }
        Some(v)
    }

    /// How Elasticsearch's `XContentMapValues.nodeStringValue` / `Object.toString()` prints a
    /// scalar: strings as is, numbers and booleans in Java's format, `null` as `"null"`.
    pub fn to_es_string(&self) -> String {
        match self {
            Value::Null => "null".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Num(n) => n.to_string(),
            Value::Str(s) => s.clone(),
            Value::Arr(a) => {
                let items: Vec<String> = a.iter().map(Value::to_es_string).collect();
                format!("[{}]", items.join(", "))
            }
            Value::Obj(m) => java_map_string(m),
        }
    }

    /// Compact JSON.
    pub fn to_json(&self) -> String {
        let mut out = Vec::new();
        write::write_json(&mut out, self, false);
        // The writer only emits UTF-8.
        String::from_utf8(out).unwrap_or_default()
    }

    pub fn to_pretty_json(&self) -> String {
        let mut out = Vec::new();
        write::write_json(&mut out, self, true);
        String::from_utf8(out).unwrap_or_default()
    }

    /// The JSON type name Elasticsearch uses in "expected X but got Y" style messages.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Value::Null => "VALUE_NULL",
            Value::Bool(_) => "VALUE_BOOLEAN",
            Value::Num(n) if n.is_integer() => "VALUE_NUMBER_INT",
            Value::Num(_) => "VALUE_NUMBER_FLOAT",
            Value::Str(_) => "VALUE_STRING",
            Value::Arr(_) => "START_ARRAY",
            Value::Obj(_) => "START_OBJECT",
        }
    }
}

/// Java's `String.hashCode()` (over UTF-16 code units).
pub fn java_string_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    h
}

/// The iteration order of a `java.util.HashMap` created with `initial_capacity` after `keys`
/// were inserted in order (no removals): indexes into `keys`.
///
/// Elasticsearch prints several maps straight from `HashMap`s (nested settings, field
/// mappings), so reproducing their bucket order makes responses byte-identical. Entries are
/// ordered by bucket in the final table, then by insertion (resizes keep relative order).
pub fn java_hash_order<S: AsRef<str>>(keys: &[S], initial_capacity: usize) -> Vec<usize> {
    let mut cap = initial_capacity.max(1).next_power_of_two();
    for n in 1..=keys.len() {
        while n > cap * 3 / 4 {
            cap *= 2;
        }
    }
    let mask = (cap - 1) as u32;
    let bucket = |k: &str| {
        let h = java_string_hash(k) as u32;
        (h ^ (h >> 16)) & mask
    };
    let mut idx: Vec<usize> = (0..keys.len()).collect();
    idx.sort_by_key(|&i| (bucket(keys[i].as_ref()), i));
    idx
}

/// Reorders an object's entries the way a `HashMap` with `initial_capacity` would iterate
/// them (inserted in their current order).
pub fn java_hash_map(m: Map, initial_capacity: usize) -> Map {
    let keys: Vec<&String> = m.keys().collect();
    let order = java_hash_order(&keys, initial_capacity);
    let mut entries: Vec<Option<(String, Value)>> = m.into_iter().map(Some).collect();
    let mut out = Map::with_capacity(entries.len());
    for i in order {
        if let Some((k, v)) = entries[i].take() {
            out.insert(k, v);
        }
    }
    out
}

/// Reorders an object's entries the way Elasticsearch's `Maps.newMapWithExpectedSize(n)` (a
/// `HashMap` sized for its `n` entries) iterates them.
pub fn java_hash_map_expected(m: Map) -> Map {
    let n = m.len();
    // `HashMap.calculateHashMapCapacity`: ceil(n / 0.75).
    let cap = (n * 4).div_ceil(3).max(1);
    java_hash_map(m, cap)
}

/// Java's `AbstractMap.toString()`: `{a=1, b=[x, y]}`.
pub fn java_map_string(m: &Map) -> String {
    let mut s = String::from("{");
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(k);
        s.push('=');
        s.push_str(&v.to_es_string());
    }
    s.push('}');
    s
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_json())
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}
impl From<&String> for Value {
    fn from(s: &String) -> Self {
        Value::Str(s.clone())
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Num(Number::Int(i))
    }
}
impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Value::Num(Number::Int(i as i64))
    }
}
impl From<u32> for Value {
    fn from(i: u32) -> Self {
        Value::Num(Number::Int(i as i64))
    }
}
impl From<usize> for Value {
    fn from(i: usize) -> Self {
        Value::Num(Number::Int(i as i64))
    }
}
impl From<u64> for Value {
    fn from(i: u64) -> Self {
        if i <= i64::MAX as u64 { Value::Num(Number::Int(i as i64)) } else { Value::Num(Number::UInt(i)) }
    }
}
impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Num(Number::Float(f))
    }
}
impl From<Map> for Value {
    fn from(m: Map) -> Self {
        Value::Obj(m)
    }
}
impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(v: Vec<T>) -> Self {
        Value::Arr(v.into_iter().map(Into::into).collect())
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(v) => v.into(),
            None => Value::Null,
        }
    }
}

/// Builds an ordered object: `obj! { "a" => 1, "b" => "x" }`.
#[macro_export]
macro_rules! obj {
    () => { $crate::json::Value::Obj($crate::json::Map::new()) };
    ($($k:expr => $v:expr),+ $(,)?) => {{
        let mut m = $crate::json::Map::new();
        $( m.insert(($k).to_string(), $crate::json::Value::from($v)); )+
        $crate::json::Value::Obj(m)
    }};
}

/// Builds an array: `arr![1, "x"]`.
#[macro_export]
macro_rules! arr {
    () => { $crate::json::Value::Arr(Vec::new()) };
    ($($v:expr),+ $(,)?) => { $crate::json::Value::Arr(vec![$($crate::json::Value::from($v)),+]) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macros_keep_order() {
        let v = obj! { "b" => 1, "a" => "x", "c" => arr![true, Value::Null] };
        assert_eq!(v.to_json(), r#"{"b":1,"a":"x","c":[true,null]}"#);
    }

    #[test]
    fn hash_map_order_like_java() {
        assert_eq!(java_string_hash("hello"), 99162322);
        assert_eq!(java_string_hash("é"), 233);
        let keys = ["creation_date", "number_of_replicas", "number_of_shards", "provided_name", "routing", "uuid", "version"];
        let order: Vec<&str> = java_hash_order(&keys, 4).into_iter().map(|i| keys[i]).collect();
        assert_eq!(order, ["routing", "number_of_shards", "provided_name", "creation_date", "number_of_replicas", "uuid", "version"]);
        let keys = ["title", "title.raw"];
        assert_eq!(java_hash_order(&keys, 16), vec![1, 0]);
    }

    #[test]
    fn es_strings() {
        assert_eq!(Value::from(1.0).to_es_string(), "1.0");
        assert_eq!(Value::from(1e300).to_es_string(), "1.0E300");
        let v = obj! { "a" => 1, "b" => arr!["x", "y"] };
        assert_eq!(v.to_es_string(), "{a=1, b=[x, y]}");
    }
}
