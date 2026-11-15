//! PPL's types and values, as the OpenSearch SQL plugin's Calcite engine reports them.
//!
//! A column has one [`Ty`] (the name the response's `schema` prints: `int`, `bigint`,
//! `string`, `timestamp`, ...) and its cells are [`Val`]s. Integral types share one `i64`
//! representation and the floating types one `f64`: the column type decides how a cell prints
//! (a `float` cell prints like Java's `Float.toString`). Objects and arrays of objects stay
//! JSON ([`Val::Json`]) since PPL only passes them through or reads paths out of them.
//!
//! Timestamps print as `yyyy-MM-dd HH:mm:ss` with the fraction's trailing zeros dropped, the
//! way the plugin's `ExprTimestampValue` renders them.

use crate::json::{Number, Value};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use std::cmp::Ordering;

/// A column type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    /// The type of a `null` literal (Calcite's `NULL`).
    Undefined,
    Bool,
    Byte,
    Short,
    Int,
    Long,
    Float,
    Double,
    Str,
    Timestamp,
    Date,
    Time,
    Ip,
    Struct,
    Array,
    Binary,
    GeoPoint,
}

impl Ty {
    /// The `schema` name.
    pub fn name(self) -> &'static str {
        match self {
            Ty::Undefined => "undefined",
            Ty::Bool => "boolean",
            Ty::Byte => "tinyint",
            Ty::Short => "smallint",
            Ty::Int => "int",
            Ty::Long => "bigint",
            Ty::Float => "float",
            Ty::Double => "double",
            Ty::Str => "string",
            Ty::Timestamp => "timestamp",
            Ty::Date => "date",
            Ty::Time => "time",
            Ty::Ip => "ip",
            Ty::Struct => "struct",
            Ty::Array => "array",
            Ty::Binary => "binary",
            Ty::GeoPoint => "geo_point",
        }
    }

    /// The `ExprCoreType` name error messages use (`INTEGER`, `STRING`, ...).
    pub fn upper(self) -> &'static str {
        match self {
            Ty::Undefined => "UNDEFINED",
            Ty::Bool => "BOOLEAN",
            Ty::Byte => "BYTE",
            Ty::Short => "SHORT",
            Ty::Int => "INTEGER",
            Ty::Long => "LONG",
            Ty::Float => "FLOAT",
            Ty::Double => "DOUBLE",
            Ty::Str => "STRING",
            Ty::Timestamp => "TIMESTAMP",
            Ty::Date => "DATE",
            Ty::Time => "TIME",
            Ty::Ip => "IP",
            Ty::Struct => "STRUCT",
            Ty::Array => "ARRAY",
            Ty::Binary => "BINARY",
            Ty::GeoPoint => "GEO_POINT",
        }
    }

    /// Calcite's SQL type name (`BIGINT`, `VARCHAR`, ...), in its validation messages.
    pub fn sql(self) -> &'static str {
        match self {
            Ty::Byte => "TINYINT",
            Ty::Short => "SMALLINT",
            Ty::Long => "BIGINT",
            Ty::Float => "REAL",
            Ty::Str => "VARCHAR",
            other => other.upper(),
        }
    }

    /// The type a mapping type reads as (`OpenSearchDataType`).
    pub fn of_mapping(t: &str) -> Ty {
        match t {
            "boolean" => Ty::Bool,
            "byte" => Ty::Byte,
            "short" => Ty::Short,
            "integer" => Ty::Int,
            "long" | "unsigned_long" => Ty::Long,
            "float" | "half_float" => Ty::Float,
            "double" | "scaled_float" => Ty::Double,
            "keyword" | "text" | "match_only_text" | "wildcard" | "constant_keyword" | "flat_object" => Ty::Str,
            "date" | "date_nanos" => Ty::Timestamp,
            "ip" => Ty::Ip,
            "object" => Ty::Struct,
            "nested" => Ty::Array,
            "binary" => Ty::Binary,
            "geo_point" => Ty::GeoPoint,
            _ => Ty::Undefined,
        }
    }

    pub fn is_integral(self) -> bool {
        matches!(self, Ty::Byte | Ty::Short | Ty::Int | Ty::Long)
    }

    pub fn is_numeric(self) -> bool {
        self.is_integral() || matches!(self, Ty::Float | Ty::Double)
    }

    /// The wider of two numeric types (Calcite's least restrictive type).
    pub fn widen(a: Ty, b: Ty) -> Ty {
        let rank = |t: Ty| match t {
            Ty::Byte => 1,
            Ty::Short => 2,
            Ty::Int => 3,
            Ty::Long => 4,
            Ty::Float => 5,
            Ty::Double => 6,
            _ => 0,
        };
        match (a, b) {
            (Ty::Undefined, t) | (t, Ty::Undefined) => t,
            _ if rank(a) >= rank(b) => a,
            _ => b,
        }
    }
}

/// One cell.
#[derive(Clone, Debug, Default)]
pub enum Val {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Ts(NaiveDateTime),
    Date(NaiveDate),
    Time(NaiveTime),
    /// A multi-value (an array in `_source`).
    Arr(Vec<Val>),
    /// An object (or anything structured the engine passes through).
    Json(Value),
}

impl Val {
    pub fn is_null(&self) -> bool {
        matches!(self, Val::Null)
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Int(i) => Some(*i as f64),
            Val::Float(f) => Some(*f),
            Val::Str(s) => s.trim().parse().ok(),
            Val::Bool(b) => Some(f64::from(u8::from(*b))),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Val::Int(i) => Some(*i),
            Val::Float(f) if f.is_finite() => Some(*f as i64),
            Val::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Val::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The text the value converts to (`CAST(x AS STRING)`, string concatenation).
    pub fn to_text(&self, ty: Ty) -> Option<String> {
        Some(match self {
            Val::Null => return None,
            Val::Bool(b) => b.to_string(),
            Val::Int(i) => i.to_string(),
            Val::Float(f) if ty == Ty::Float => crate::json::write::java_float(*f as f32),
            Val::Float(f) => crate::json::write::java_double(*f),
            Val::Str(s) => s.clone(),
            Val::Ts(t) => format_ts(t),
            Val::Date(d) => d.format("%Y-%m-%d").to_string(),
            Val::Time(t) => format_time(t),
            Val::Arr(a) => Value::Arr(a.iter().map(|v| v.to_json(Ty::Undefined)).collect()).to_json(),
            Val::Json(v) => v.to_json(),
        })
    }

    /// The cell as the response prints it.
    pub fn to_json(&self, ty: Ty) -> Value {
        match self {
            Val::Null => Value::Null,
            Val::Bool(b) => Value::Bool(*b),
            Val::Int(i) => Value::Num(Number::Int(*i)),
            Val::Float(f) if ty == Ty::Float => Value::Num(Number::Float(crate::json::write::java_float_value(*f as f32))),
            Val::Float(f) => Value::Num(Number::Float(*f)),
            Val::Str(s) => Value::Str(s.clone()),
            Val::Ts(t) => Value::Str(format_ts(t)),
            Val::Date(d) => Value::Str(d.format("%Y-%m-%d").to_string()),
            Val::Time(t) => Value::Str(format_time(t)),
            Val::Arr(a) => Value::Arr(a.iter().map(|v| v.to_json(elem_ty(ty))).collect()),
            Val::Json(v) => v.clone(),
        }
    }
}

/// How an array's elements print: a column of arrays carries its elements' type.
fn elem_ty(ty: Ty) -> Ty {
    if ty == Ty::Array { Ty::Undefined } else { ty }
}

/// `yyyy-MM-dd HH:mm:ss[.fraction]`, trailing zeros of the fraction dropped.
pub fn format_ts(t: &NaiveDateTime) -> String {
    let mut s = t.format("%Y-%m-%d %H:%M:%S").to_string();
    push_fraction(&mut s, t.nanosecond());
    s
}

pub fn format_time(t: &NaiveTime) -> String {
    let mut s = t.format("%H:%M:%S").to_string();
    push_fraction(&mut s, t.nanosecond());
    s
}

fn push_fraction(s: &mut String, nanos: u32) {
    if nanos == 0 {
        return;
    }
    let frac = format!("{:09}", nanos % 1_000_000_000);
    s.push('.');
    s.push_str(frac.trim_end_matches('0'));
}

/// Parses the timestamp texts PPL accepts in literals and comparisons (`yyyy-MM-dd
/// HH:mm:ss[.fraction]`, a date alone, ISO `T` forms with an optional `Z`).
pub fn parse_ts(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    let s = s.strip_suffix('Z').unwrap_or(s);
    for f in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, f) {
            return Some(t);
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| d.and_hms_opt(0, 0, 0).expect("midnight"))
}

pub fn ts_millis(t: &NaiveDateTime) -> i64 {
    t.and_utc().timestamp_millis()
}

pub fn ts_from_millis(ms: i64) -> Option<NaiveDateTime> {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.naive_utc())
}

/// Total order for sorting (nulls handled by the caller): numbers numerically, then by kind.
pub fn cmp(a: &Val, b: &Val) -> Ordering {
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => x.cmp(y),
        (Val::Int(_) | Val::Float(_), Val::Int(_) | Val::Float(_)) => a.as_f64().unwrap_or(0.0).total_cmp(&b.as_f64().unwrap_or(0.0)),
        (Val::Str(x), Val::Str(y)) => x.cmp(y),
        (Val::Bool(x), Val::Bool(y)) => x.cmp(y),
        (Val::Ts(x), Val::Ts(y)) => x.cmp(y),
        (Val::Date(x), Val::Date(y)) => x.cmp(y),
        (Val::Time(x), Val::Time(y)) => x.cmp(y),
        (Val::Ts(x), Val::Str(s)) => parse_ts(s).map_or(Ordering::Greater, |y| x.cmp(&y)),
        (Val::Str(s), Val::Ts(y)) => parse_ts(s).map_or(Ordering::Less, |x| x.cmp(y)),
        (Val::Ts(x), Val::Date(d)) => x.cmp(&d.and_hms_opt(0, 0, 0).expect("midnight")),
        (Val::Date(d), Val::Ts(y)) => d.and_hms_opt(0, 0, 0).expect("midnight").cmp(y),
        (Val::Arr(x), Val::Arr(y)) => {
            for (p, q) in x.iter().zip(y) {
                let o = cmp(p, q);
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        (Val::Null, Val::Null) => Ordering::Equal,
        (Val::Null, _) => Ordering::Less,
        (_, Val::Null) => Ordering::Greater,
        _ => rank(a).cmp(&rank(b)).then_with(|| a.to_text(Ty::Undefined).cmp(&b.to_text(Ty::Undefined))),
    }
}

fn rank(v: &Val) -> u8 {
    match v {
        Val::Null => 0,
        Val::Bool(_) => 1,
        Val::Int(_) | Val::Float(_) => 2,
        Val::Str(_) => 3,
        Val::Date(_) => 4,
        Val::Ts(_) => 5,
        Val::Time(_) => 6,
        Val::Arr(_) => 7,
        Val::Json(_) => 8,
    }
}

/// Equality as grouping and `dedup` see it.
pub fn same(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Json(x), Val::Json(y)) => x == y,
        (Val::Null, Val::Null) => true,
        (Val::Null, _) | (_, Val::Null) => false,
        _ => cmp(a, b) == Ordering::Equal && rank(a) == rank(b),
    }
}

/// A hashable key for grouping.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    Null,
    Bool(bool),
    Int(i64),
    Float(u64),
    Str(String),
    Ts(NaiveDateTime),
    Date(NaiveDate),
    Time(NaiveTime),
    Other(String),
}

impl Key {
    pub fn of(v: &Val) -> Key {
        match v {
            Val::Null => Key::Null,
            Val::Bool(b) => Key::Bool(*b),
            Val::Int(i) => Key::Int(*i),
            Val::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Key::Int(*f as i64),
            Val::Float(f) => Key::Float(f.to_bits()),
            Val::Str(s) => Key::Str(s.clone()),
            Val::Ts(t) => Key::Ts(*t),
            Val::Date(d) => Key::Date(*d),
            Val::Time(t) => Key::Time(*t),
            Val::Arr(_) | Val::Json(_) => Key::Other(v.to_text(Ty::Undefined).unwrap_or_default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_print_without_trailing_zeros() {
        let t = parse_ts("2024-01-02T03:04:05.120Z").unwrap();
        assert_eq!(format_ts(&t), "2024-01-02 03:04:05.12");
        assert_eq!(format_ts(&parse_ts("2024-01-03").unwrap()), "2024-01-03 00:00:00");
    }

    #[test]
    fn floats_print_like_java_floats() {
        assert_eq!(Val::Float(2.2f32 as f64).to_json(Ty::Float).to_json(), "2.2");
        assert_eq!(Val::Float(2.0).to_json(Ty::Double).to_json(), "2.0");
    }
}
