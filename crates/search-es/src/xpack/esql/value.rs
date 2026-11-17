//! ES|QL values, types, scalar functions and aggregations.
//!
//! Values are small enums evaluated row by row; multi-valued fields are `V::Multi`.
//! Arithmetic follows ES|QL: `integer` op `integer` stays `integer` (overflow → `null` with a
//! warning), `long` widens, any `double` makes a `double`, division by zero is `null`.
//! Dates are epoch milliseconds printed as ISO-8601 instants.

use crate::error::{EsError, EsResult};
use crate::json::Value;
use search_aggs::metrics::cardinality::{Hll, precision_from_threshold};
use search_aggs::metrics::tdigest::TDigest;
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    Null,
    Boolean,
    Integer,
    Long,
    UnsignedLong,
    Double,
    Keyword,
    Text,
    Date,
    DateNanos,
    Ip,
    Version,
    GeoPoint,
    CartesianPoint,
    GeoShape,
    CartesianShape,
    /// Span literals: `1 day` (calendar) and `1 hour` (fixed).
    DatePeriod,
    TimeDuration,
    /// `METADATA _source`: the document's source (held as its JSON text).
    Source,
    /// Pre-aggregated metrics, held as Elasticsearch prints them (`{"min":1.0,...}`).
    AggregateMetricDouble,
    /// `time_series_metric: counter` fields of time series indices: numbers that only the
    /// time series functions (and conversions) read.
    CounterLong,
    CounterInteger,
    CounterDouble,
    /// Histogram-like and vector fields (held as their printed JSON, vectors as doubles).
    Histogram,
    ExponentialHistogram,
    TDigest,
    DenseVector,
    /// `METADATA _tsid` (base64 text).
    Tsid,
    /// A `flattened` field: its leaves by dotted path, held as the JSON text Elasticsearch
    /// prints (`{"a.c":["1","3"],"b":"x"}`).
    Flattened,
    /// Grid cells (`TO_GEOHASH`, ...), held as Elasticsearch's long encodings and printed as
    /// their addresses ([`super::spatial::grid_address`]).
    Geohash,
    Geotile,
    Geohex,
    Unsupported,
}

impl DataType {
    pub fn name(self) -> &'static str {
        match self {
            DataType::Null => "null",
            DataType::Boolean => "boolean",
            DataType::Integer => "integer",
            DataType::Long => "long",
            DataType::UnsignedLong => "unsigned_long",
            DataType::Double => "double",
            DataType::Keyword => "keyword",
            DataType::Text => "text",
            DataType::Date => "date",
            DataType::DateNanos => "date_nanos",
            DataType::Ip => "ip",
            DataType::Version => "version",
            DataType::GeoPoint => "geo_point",
            DataType::CartesianPoint => "cartesian_point",
            DataType::GeoShape => "geo_shape",
            DataType::CartesianShape => "cartesian_shape",
            DataType::DatePeriod => "date_period",
            DataType::TimeDuration => "time_duration",
            DataType::Source => "_source",
            DataType::AggregateMetricDouble => "aggregate_metric_double",
            DataType::CounterLong => "counter_long",
            DataType::CounterInteger => "counter_integer",
            DataType::CounterDouble => "counter_double",
            DataType::Histogram => "histogram",
            DataType::ExponentialHistogram => "exponential_histogram",
            DataType::TDigest => "tdigest",
            DataType::DenseVector => "dense_vector",
            DataType::Tsid => "_tsid",
            DataType::Flattened => "flattened",
            DataType::Geohash => "geohash",
            DataType::Geotile => "geotile",
            DataType::Geohex => "geohex",
            DataType::Unsupported => "unsupported",
        }
    }

    /// The ES|QL type of an index field type.
    pub fn of_mapping(t: &str) -> DataType {
        match t {
            "boolean" => DataType::Boolean,
            "byte" | "short" | "integer" => DataType::Integer,
            "long" => DataType::Long,
            "unsigned_long" => DataType::UnsignedLong,
            "float" | "half_float" | "double" | "scaled_float" => DataType::Double,
            "keyword" | "constant_keyword" | "wildcard" => DataType::Keyword,
            "text" | "match_only_text" | "semantic_text" => DataType::Text,
            "date" => DataType::Date,
            "date_nanos" => DataType::DateNanos,
            "ip" => DataType::Ip,
            "version" => DataType::Version,
            "geo_point" => DataType::GeoPoint,
            "point" => DataType::CartesianPoint,
            "geo_shape" => DataType::GeoShape,
            "shape" => DataType::CartesianShape,
            "aggregate_metric_double" => DataType::AggregateMetricDouble,
            "histogram" => DataType::Histogram,
            "exponential_histogram" => DataType::ExponentialHistogram,
            "tdigest" => DataType::TDigest,
            "dense_vector" => DataType::DenseVector,
            "flattened" => DataType::Flattened,
            _ => DataType::Unsupported,
        }
    }

    /// The counter type of a numeric field type (`time_series_metric: counter`).
    pub fn counter_of(self) -> DataType {
        match self {
            DataType::Long => DataType::CounterLong,
            DataType::Integer => DataType::CounterInteger,
            DataType::Double => DataType::CounterDouble,
            t => t,
        }
    }

    pub fn is_counter(self) -> bool {
        matches!(self, DataType::CounterLong | DataType::CounterInteger | DataType::CounterDouble)
    }

    /// The types `SORT` and `STATS ... BY` refuse (`cannot sort on ...`).
    pub fn unsortable(self) -> bool {
        self.is_counter() || self.is_grid() || matches!(self, DataType::AggregateMetricDouble | DataType::Histogram | DataType::ExponentialHistogram | DataType::TDigest | DataType::DenseVector | DataType::Tsid | DataType::Source | DataType::Flattened)
    }

    pub fn is_grid(self) -> bool {
        matches!(self, DataType::Geohash | DataType::Geotile | DataType::Geohex)
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, DataType::Integer | DataType::Long | DataType::UnsignedLong | DataType::Double)
    }

    pub fn is_string(self) -> bool {
        matches!(self, DataType::Keyword | DataType::Text)
    }

    /// The common type of two numeric types.
    pub fn widen(a: DataType, b: DataType) -> DataType {
        use DataType::*;
        match (a, b) {
            (Null, x) | (x, Null) => x,
            (Double, _) | (_, Double) => Double,
            (UnsignedLong, _) | (_, UnsignedLong) => UnsignedLong,
            (Long, _) | (_, Long) => Long,
            _ => Integer,
        }
    }

    pub fn parse_name(s: &str) -> Option<DataType> {
        Some(match s {
            "boolean" | "bool" => DataType::Boolean,
            "integer" | "int" => DataType::Integer,
            "long" => DataType::Long,
            "unsigned_long" => DataType::UnsignedLong,
            "double" => DataType::Double,
            "keyword" | "string" => DataType::Keyword,
            "text" => DataType::Text,
            "datetime" | "date" => DataType::Date,
            "date_nanos" => DataType::DateNanos,
            "ip" => DataType::Ip,
            "version" => DataType::Version,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum V {
    #[default]
    Null,
    Bool(bool),
    Int(i32),
    Long(i64),
    /// An `unsigned_long` above `i64::MAX` (those up to it are [`V::Long`]s).
    ULong(u64),
    Double(f64),
    Str(String),
    /// Epoch milliseconds.
    Date(i64),
    Ip(String),
    Multi(Vec<V>),
}

impl V {
    pub fn is_null(&self) -> bool {
        matches!(self, V::Null)
    }

    pub fn type_of(&self) -> DataType {
        match self {
            V::Null => DataType::Null,
            V::Bool(_) => DataType::Boolean,
            V::Int(_) => DataType::Integer,
            V::Long(_) => DataType::Long,
            V::ULong(_) => DataType::UnsignedLong,
            V::Double(_) => DataType::Double,
            V::Str(_) => DataType::Keyword,
            V::Date(_) => DataType::Date,
            V::Ip(_) => DataType::Ip,
            V::Multi(v) => v.first().map_or(DataType::Null, V::type_of),
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            V::Int(i) => Some(*i as f64),
            V::Long(l) | V::Date(l) => Some(*l as f64),
            V::ULong(u) => Some(*u as f64),
            V::Double(d) => Some(*d),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            V::Int(i) => Some(*i as i64),
            V::Long(l) | V::Date(l) => Some(*l),
            V::Double(d) if d.fract() == 0.0 && d.abs() < 9.2e18 => Some(*d as i64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            V::Str(s) | V::Ip(s) => Some(s),
            _ => None,
        }
    }

    pub fn truthy(&self) -> Option<bool> {
        match self {
            V::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The single values (a multi-value's elements, or itself).
    pub fn values(&self) -> Vec<&V> {
        match self {
            V::Multi(v) => v.iter().collect(),
            V::Null => Vec::new(),
            other => vec![other],
        }
    }

    /// As the JSON response prints it for a column of type `ty`.
    pub fn to_json(&self, ty: DataType) -> Value {
        match self {
            V::Null => Value::Null,
            V::Bool(b) => Value::Bool(*b),
            V::Int(i) => Value::from(*i as i64),
            V::Long(l) => {
                if ty == DataType::Double {
                    Value::from(*l as f64)
                } else if ty.is_grid() {
                    Value::from(super::spatial::grid_address(*l, ty))
                } else if ty == DataType::Date {
                    Value::from(crate::time::java_instant_string(*l))
                } else {
                    Value::from(*l)
                }
            }
            V::ULong(u) if ty == DataType::Double => Value::from(*u as f64),
            V::ULong(u) => Value::from(*u),
            // Infinite doubles print like Java's `Double.toString` (NaN never reaches output).
            V::Double(d) => {
                if d.is_finite() {
                    Value::from(*d)
                } else if d.is_nan() {
                    Value::Null
                } else {
                    Value::from(if *d > 0.0 { "Infinity" } else { "-Infinity" })
                }
            }
            V::Str(s) if matches!(ty, DataType::Source | DataType::Flattened) => crate::json::parse(s.as_bytes()).ok().flatten().unwrap_or(Value::Null),
            V::Str(s) | V::Ip(s) => Value::from(s.clone()),
            V::Date(ms) => Value::from(date_string(*ms)),
            V::Multi(v) => Value::Arr(v.iter().map(|x| x.to_json(ty)).collect()),
        }
    }

    /// As text (`TO_STRING`, txt/csv output, string concatenation).
    pub fn to_text(&self) -> String {
        match self {
            V::Null => "null".into(),
            V::Bool(b) => b.to_string(),
            V::Int(i) => i.to_string(),
            V::Long(l) => l.to_string(),
            V::ULong(u) => u.to_string(),
            V::Double(d) => crate::json::write::java_double(*d),
            V::Str(s) | V::Ip(s) => s.clone(),
            V::Date(ms) => date_string(*ms),
            V::Multi(v) => format!("[{}]", v.iter().map(V::to_text).collect::<Vec<_>>().join(", ")),
        }
    }
}

/// Orders values of one column (numbers numerically, strings by code point, IPs by address).
pub fn cmp(a: &V, b: &V) -> Ordering {
    match (a, b) {
        (V::Null, V::Null) => Ordering::Equal,
        (V::Null, _) => Ordering::Greater,
        (_, V::Null) => Ordering::Less,
        (V::Ip(x), V::Ip(y)) => match (x.parse::<std::net::IpAddr>(), y.parse::<std::net::IpAddr>()) {
            (Ok(p), Ok(q)) => ip_bytes(p).cmp(&ip_bytes(q)),
            _ => x.cmp(y),
        },
        (V::Bool(x), V::Bool(y)) => x.cmp(y),
        (V::Multi(x), _) => x.first().map_or(Ordering::Greater, |f| cmp(f, b)),
        (_, V::Multi(y)) => y.first().map_or(Ordering::Less, |f| cmp(a, f)),
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => {
                if let (Some(i), Some(j)) = (int_of(a), int_of(b)) {
                    i.cmp(&j)
                } else {
                    x.total_cmp(&y)
                }
            }
            _ => a.to_text().cmp(&b.to_text()),
        },
    }
}

fn int_of(v: &V) -> Option<i128> {
    match v {
        V::Int(i) => Some(*i as i128),
        V::Long(l) | V::Date(l) => Some(*l as i128),
        V::ULong(u) => Some(*u as i128),
        _ => None,
    }
}

fn ip_bytes(ip: std::net::IpAddr) -> [u8; 16] {
    match ip {
        std::net::IpAddr::V4(v) => v.to_ipv6_mapped().octets(),
        std::net::IpAddr::V6(v) => v.octets(),
    }
}

/// Equality as `==` sees it (`null` → `None`).
pub fn eq(a: &V, b: &V) -> Option<bool> {
    if a.is_null() || b.is_null() {
        return None;
    }
    Some(match (a, b) {
        (V::Multi(_), _) | (_, V::Multi(_)) => return None,
        (V::Str(x), V::Str(y)) => x == y,
        (V::Ip(x), V::Ip(y)) | (V::Ip(x), V::Str(y)) | (V::Str(x), V::Ip(y)) => {
            x == y || matches!((x.parse::<std::net::IpAddr>(), y.parse::<std::net::IpAddr>()), (Ok(p), Ok(q)) if p == q)
        }
        (V::Bool(x), V::Bool(y)) => x == y,
        (V::Date(x), V::Str(s)) | (V::Str(s), V::Date(x)) => parse_date(s).is_some_and(|d| d == *x),
        // Java's primitive `==`, as `EqualsDoublesEvaluator`: `-0.0 == 0.0`, NaN equals nothing.
        (V::Double(x), y) | (y, V::Double(x)) if y.as_f64().is_some() => *x == y.as_f64().expect("numeric"),
        _ => cmp(a, b) == Ordering::Equal,
    })
}

/// The order `<`, `<=`, `>` and `>=` see: doubles compare with Java's primitive operators
/// (`-0.0` is not below `0.0`; NaN is unordered, so every operator is false), everything else
/// by [`cmp`].
pub fn ordered(a: &V, b: &V) -> Option<Ordering> {
    if (matches!(a, V::Double(_)) || matches!(b, V::Double(_)))
        && let (Some(x), Some(y)) = (a.as_f64(), b.as_f64())
    {
        return x.partial_cmp(&y);
    }
    Some(cmp(a, b))
}

pub fn parse_date(s: &str) -> Option<i64> {
    let f = crate::time::DateFormatter::new("strict_date_optional_time||epoch_millis").ok()?;
    f.parse_millis(s).ok()
}

/// Warnings collected while evaluating (Elasticsearch sends them as `Warning` headers).
#[derive(Default)]
pub struct Warnings {
    pub list: Vec<String>,
}

impl Warnings {
    pub fn add(&mut self, w: String) {
        if self.list.len() < 20 && !self.list.contains(&w) {
            self.list.push(w);
        }
    }
}

/// Arithmetic with ES|QL's rules: `integer` and `long` overflow, and division or remainder by
/// zero, are failures (`Err`: the Java exception text for the warning; the value is `null`), as
/// are multi-valued operands. Doubles that overflow to infinity are failures too.
pub fn arith(op: super::parser::BinOp, a: &V, b: &V, ty: DataType) -> Result<V, String> {
    use super::parser::BinOp::*;
    if matches!(a, V::Multi(_)) || matches!(b, V::Multi(_)) {
        return Err("java.lang.IllegalArgumentException: single-value function encountered multi-value".into());
    }
    if a.is_null() || b.is_null() {
        return Ok(V::Null);
    }
    let by_zero = || "java.lang.ArithmeticException: / by zero".to_string();
    match ty {
        DataType::Double => {
            let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) else { return Ok(V::Null) };
            let r = match op {
                Add => x + y,
                Sub => x - y,
                Mul => x * y,
                Div | Mod if y == 0.0 => return Err(by_zero()),
                Div => x / y,
                Mod => x % y,
                _ => return Ok(V::Null),
            };
            if r.is_finite() {
                Ok(V::Double(r))
            } else {
                Err(format!("java.lang.ArithmeticException: not a finite double number: {}", if r.is_nan() { "NaN" } else if r > 0.0 { "Infinity" } else { "-Infinity" }))
            }
        }
        DataType::Integer | DataType::Long | DataType::UnsignedLong => {
            let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) else { return Ok(V::Null) };
            if matches!(op, Div | Mod) && y == 0 {
                return Err(by_zero());
            }
            let overflow = || format!("java.lang.ArithmeticException: {} overflow", if ty == DataType::Integer { "integer" } else { "long" });
            if ty == DataType::Integer {
                let (x, y) = (x as i32, y as i32);
                let r = match op {
                    Add => x.checked_add(y),
                    Sub => x.checked_sub(y),
                    Mul => x.checked_mul(y),
                    Div => x.checked_div(y),
                    Mod => x.checked_rem(y),
                    _ => return Ok(V::Null),
                };
                return r.map(V::Int).ok_or_else(overflow);
            }
            let r = match op {
                Add => x.checked_add(y),
                Sub => x.checked_sub(y),
                Mul => x.checked_mul(y),
                Div => x.checked_div(y),
                Mod => x.checked_rem(y),
                _ => return Ok(V::Null),
            };
            r.map(V::Long).ok_or_else(overflow)
        }
        _ => Ok(V::Null),
    }
}

/// A time unit of a span literal (`1 hour`) and its length in milliseconds (months and longer
/// are calendar units, approximated by their mean length only where a fixed span is needed).
pub fn time_unit(u: &str) -> Option<i64> {
    Some(match u.to_ascii_lowercase().as_str() {
        "millisecond" | "milliseconds" | "ms" => 1,
        "second" | "seconds" | "sec" | "s" => 1000,
        "minute" | "minutes" | "min" => 60_000,
        "hour" | "hours" | "h" => 3_600_000,
        "day" | "days" | "d" => 86_400_000,
        "week" | "weeks" | "w" => 604_800_000,
        "month" | "months" | "mo" => 2_629_746_000,
        "quarter" | "quarters" | "q" => 7_889_238_000,
        "year" | "years" | "yr" | "y" => 31_556_952_000,
        _ => return None,
    })
}

/// A date shifted by a span literal (`@timestamp + 1 day`); calendar units move the calendar.
pub fn shift_date(ms: i64, span: &str, sign: i64) -> Option<i64> {
    use chrono::{Datelike, Months};
    let (n, unit) = span.split_once(' ')?;
    let n: i64 = n.parse().ok()?;
    let len = time_unit(unit)?;
    let months = match unit {
        "month" | "months" | "mo" => Some(n),
        "quarter" | "quarters" | "q" => Some(3 * n),
        "year" | "years" | "yr" | "y" => Some(12 * n),
        _ => None,
    };
    match months {
        Some(m) => {
            let d = chrono::DateTime::from_timestamp_millis(ms)?;
            let m = m * sign;
            let shifted = if m >= 0 { d.checked_add_months(Months::new(m as u32))? } else { d.checked_sub_months(Months::new((-m) as u32))? };
            let _ = shifted.year();
            Some(shifted.timestamp_millis())
        }
        None => ms.checked_add(sign * n * len),
    }
}

/// Converts a field value read from an index (`fields` API JSON) to a value of `ty`.
pub fn from_field(v: &Value, ty: DataType) -> V {
    from_field_in(v, ty, false)
}

/// [`from_field`]; `array_order`: the index keeps arrays in insertion order (strict columnar
/// index modes), so multi-values aren't put in doc-values order.
/// An `aggregate_metric_double` value as Elasticsearch prints it: the present sub-metrics in
/// `min, max, sum, value_count` order, doubles in Java's form, the count an integer.
pub fn agg_metric_text(min: Option<f64>, max: Option<f64>, sum: Option<f64>, count: Option<i64>) -> Option<String> {
    if min.is_none() && max.is_none() && sum.is_none() && count.is_none() {
        return None;
    }
    let mut parts = Vec::with_capacity(4);
    for (k, x) in [("min", min), ("max", max), ("sum", sum)] {
        if let Some(x) = x {
            parts.push(format!("\"{k}\":{}", crate::json::write::java_double(x)));
        }
    }
    if let Some(c) = count {
        parts.push(format!("\"value_count\":{c}"));
    }
    Some(format!("{{{}}}", parts.join(",")))
}

/// The sub-metrics of a printed `aggregate_metric_double`: `[min, max, sum, value_count]`.
pub fn agg_metric_parts(text: &str) -> [Option<f64>; 4] {
    let mut out = [None; 4];
    if let Ok(Some(Value::Obj(m))) = crate::json::parse(text.as_bytes()) {
        for (i, k) in ["min", "max", "sum", "value_count"].iter().enumerate() {
            out[i] = m.get(*k).and_then(Value::as_f64);
        }
    }
    out
}

/// A `flattened` field's value as Elasticsearch's block loader reads it: every leaf by its
/// dotted path (keys sorted), each value as text, several sorted and deduplicated like the
/// keyword doc values they come from.
pub fn flattened_text(v: &Value) -> Option<String> {
    fn walk(prefix: &str, v: &Value, out: &mut std::collections::BTreeMap<String, Vec<String>>) {
        match v {
            Value::Obj(m) => {
                for (k, x) in m {
                    let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    walk(&p, x, out);
                }
            }
            Value::Arr(a) => a.iter().for_each(|x| walk(prefix, x, out)),
            Value::Null => {}
            leaf => out.entry(prefix.to_string()).or_default().push(leaf.to_es_string()),
        }
    }
    let mut leaves = std::collections::BTreeMap::new();
    walk("", v, &mut leaves);
    if leaves.is_empty() {
        return None;
    }
    let mut m = crate::json::Map::new();
    for (k, mut vs) in leaves {
        vs.sort();
        vs.dedup();
        let val = if vs.len() == 1 { Value::Str(vs.pop().expect("one")) } else { Value::Arr(vs.into_iter().map(Value::Str).collect()) };
        m.insert(k, val);
    }
    Some(Value::Obj(m).to_json())
}

pub fn from_field_in(v: &Value, ty: DataType, array_order: bool) -> V {
    match (ty, v) {
        (DataType::Flattened, Value::Arr(a)) if a.len() == 1 && matches!(a[0], Value::Obj(_)) => return flattened_text(&a[0]).map_or(V::Null, V::Str),
        (DataType::Flattened, Value::Obj(_)) => return flattened_text(v).map_or(V::Null, V::Str),
        (DataType::AggregateMetricDouble, Value::Arr(a)) if a.len() == 1 => return from_field_in(&a[0], ty, array_order),
        (DataType::AggregateMetricDouble, Value::Obj(m)) => {
            let f = |k: &str| m.get(k).and_then(Value::as_f64);
            return agg_metric_text(f("min"), f("max"), f("sum"), m.get("value_count").and_then(Value::as_i64)).map_or(V::Null, V::Str);
        }
        (DataType::Histogram | DataType::ExponentialHistogram | DataType::TDigest, Value::Arr(a)) if a.len() == 1 => return from_field_in(&a[0], ty, array_order),
        (DataType::Histogram | DataType::ExponentialHistogram | DataType::TDigest, Value::Obj(_)) => return V::Str(v.to_json()),
        // A vector keeps its dimensions in order (never sorted or deduplicated).
        (DataType::DenseVector, Value::Arr(a)) => {
            let vals: Vec<V> = a.iter().filter_map(Value::as_f64).map(V::Double).collect();
            return match vals.len() {
                0 => V::Null,
                1 => vals.into_iter().next().expect("one"),
                _ => V::Multi(vals),
            };
        }
        _ => {}
    }
    let ty = match ty {
        DataType::CounterLong => DataType::Long,
        DataType::CounterInteger => DataType::Integer,
        DataType::CounterDouble => DataType::Double,
        t => t,
    };
    // `[x, y]` is one point, not two values.
    if matches!(ty, DataType::GeoPoint | DataType::CartesianPoint)
        && let Value::Arr(a) = v
        && (2..=3).contains(&a.len())
        && a.first().is_some_and(|x| matches!(x, Value::Num(_)))
    {
        return super::spatial::from_value(v, ty).map_or(V::Null, V::Str);
    }
    match v {
        Value::Null => V::Null,
        Value::Arr(a) => {
            let mut vals: Vec<V> = a.iter().map(|x| from_field(x, ty)).filter(|x| !x.is_null()).collect();
            // Values come from doc values: sorted, and deduplicated for the sorted-set types.
            match ty {
                _ if array_order => {}
                DataType::Keyword | DataType::Ip | DataType::Version => {
                    vals.sort_by(|a, b| super::functions::cmp_typed(a, b, ty));
                    vals.dedup();
                }
                DataType::Boolean | DataType::Integer | DataType::Long | DataType::UnsignedLong | DataType::Double | DataType::Date | DataType::DateNanos => vals.sort_by(cmp),
                _ => {}
            }
            match vals.len() {
                0 => V::Null,
                1 => vals.into_iter().next().expect("one"),
                _ => V::Multi(vals),
            }
        }
        _ => match ty {
            DataType::Boolean => match v {
                Value::Bool(b) => V::Bool(*b),
                Value::Str(s) => V::Bool(s == "true"),
                _ => V::Null,
            },
            DataType::Integer => v.as_i64().map(|i| V::Int(i as i32)).unwrap_or(V::Null),
            DataType::UnsignedLong => match v {
                Value::Num(crate::json::Number::UInt(u)) => V::ULong(*u),
                Value::Str(t) => match t.parse::<u64>() {
                    Ok(u) if u > i64::MAX as u64 => V::ULong(u),
                    Ok(u) => V::Long(u as i64),
                    Err(_) => v.as_f64().map(|d| V::Long(d as i64)).unwrap_or(V::Null),
                },
                _ => v.as_i64().map(V::Long).or_else(|| v.as_f64().map(|d| V::Long(d as i64))).unwrap_or(V::Null),
            },
            DataType::Long => v.as_i64().map(V::Long).or_else(|| v.as_f64().map(|d| V::Long(d as i64))).unwrap_or(V::Null),
            DataType::Double => v.as_f64().map(V::Double).unwrap_or(V::Null),
            DataType::Date | DataType::DateNanos => match v {
                Value::Str(s) => s.parse::<i64>().ok().or_else(|| parse_date(s)).map(V::Date).unwrap_or(V::Null),
                other => other.as_i64().map(V::Date).unwrap_or(V::Null),
            },
            DataType::Ip => V::Ip(v.to_es_string()),
            t if super::spatial::is_spatial(t) => super::spatial::from_value(v, t).map_or(V::Null, V::Str),
            DataType::Unsupported | DataType::Null => V::Null,
            _ => V::Str(v.to_es_string()),
        },
    }
}

/// `LIKE` / `RLIKE` patterns.
pub fn like(text: &str, pattern: &str, rlike: bool) -> bool {
    if rlike {
        return regex::Regex::new(&format!("^(?:{pattern})$")).map(|r| r.is_match(text)).unwrap_or(false);
    }
    // `*` any run, `?` one character, `\` escapes.
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    fn m(p: &[char], t: &[char]) -> bool {
        let (mut pi, mut ti) = (0, 0);
        let (mut star, mut mark) = (None, 0);
        while ti < t.len() {
            if pi < p.len() && p[pi] == '\\' && pi + 1 < p.len() && p[pi + 1] == t[ti] {
                pi += 2;
                ti += 1;
            } else if pi < p.len() && (p[pi] == '?' || (p[pi] == t[ti] && p[pi] != '*')) {
                pi += 1;
                ti += 1;
            } else if pi < p.len() && p[pi] == '*' {
                star = Some(pi);
                mark = ti;
                pi += 1;
            } else if let Some(s) = star {
                pi = s + 1;
                mark += 1;
                ti = mark;
            } else {
                return false;
            }
        }
        while pi < p.len() && p[pi] == '*' {
            pi += 1;
        }
        pi == p.len()
    }
    m(&p, &t)
}

fn round_half_up(x: f64, digits: i64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    // Java's Math.round semantics on the scaled value (half up), through decimal text so 2.675
    // style representation errors round like BigDecimal.valueOf(x).setScale(.., HALF_UP).
    let s = format!("{x}");
    if let Ok(d) = s.parse::<f64>() {
        let factor = 10f64.powi(digits.clamp(-300, 300) as i32);
        let scaled = d * factor;
        let r = if scaled >= 0.0 { (scaled + 0.5).floor() } else { -((-scaled + 0.5).floor()) };
        let out = r / factor;
        if out.is_finite() {
            return out;
        }
    }
    x
}

/// A scalar function call; `None` when the name isn't a scalar function.
pub fn scalar(name: &str, args: &[V], arg_types: &[DataType]) -> Option<EsResult<V>> {
    let a0 = args.first().cloned().unwrap_or(V::Null);
    let num = |v: &V| v.as_f64();
    let str_arg = |i: usize| args.get(i).map(V::to_text);
    let null_if_any = args.iter().any(V::is_null);
    let r: V = match name {
        "round" => {
            let digits = args.get(1).and_then(V::as_i64).unwrap_or(0);
            match a0 {
                V::Null => V::Null,
                V::Double(d) => V::Double(round_half_up(d, digits)),
                V::Int(i) if digits >= 0 => V::Int(i),
                V::Long(l) if digits >= 0 => V::Long(l),
                other => other.as_f64().map(|d| V::Long(round_half_up(d, digits) as i64)).unwrap_or(V::Null),
            }
        }
        "abs" => match a0 {
            V::Int(i) => i.checked_abs().map(V::Int).unwrap_or(V::Null),
            V::Long(l) => l.checked_abs().map(V::Long).unwrap_or(V::Null),
            V::Double(d) => V::Double(d.abs()),
            _ => V::Null,
        },
        "ceil" | "floor" => match a0 {
            V::Double(d) => V::Double(if name == "ceil" { d.ceil() } else { d.floor() }),
            other => other,
        },
        "sqrt" | "log10" | "exp" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "cbrt" | "signum" => match num(&a0) {
            None => V::Null,
            Some(x) => {
                let r = match name {
                    "sqrt" => x.sqrt(),
                    "log10" => x.log10(),
                    "exp" => x.exp(),
                    "sin" => x.sin(),
                    "cos" => x.cos(),
                    "tan" => x.tan(),
                    "asin" => x.asin(),
                    "acos" => x.acos(),
                    "atan" => x.atan(),
                    "sinh" => x.sinh(),
                    "cosh" => x.cosh(),
                    "tanh" => x.tanh(),
                    "cbrt" => x.cbrt(),
                    _ => x.signum(),
                };
                if r.is_finite() { V::Double(r) } else { V::Null }
            }
        },
        "log" => match (args.len(), num(&a0), args.get(1).and_then(num)) {
            (1, Some(x), _) if x > 0.0 => V::Double(x.ln()),
            (2, Some(b), Some(x)) if x > 0.0 && b > 0.0 && b != 1.0 => V::Double(x.ln() / b.ln()),
            _ => V::Null,
        },
        "pow" => match (num(&a0), args.get(1).and_then(num)) {
            (Some(b), Some(e)) => {
                let r = b.powf(e);
                if r.is_finite() { V::Double(r) } else { V::Null }
            }
            _ => V::Null,
        },
        "atan2" => match (num(&a0), args.get(1).and_then(num)) {
            (Some(y), Some(x)) => V::Double(y.atan2(x)),
            _ => V::Null,
        },
        "pi" => V::Double(std::f64::consts::PI),
        "e" => V::Double(std::f64::consts::E),
        "tau" => V::Double(std::f64::consts::TAU),
        "length" => str_arg(0).filter(|_| !a0.is_null()).map(|s| V::Int(s.chars().count() as i32)).unwrap_or(V::Null),
        "bit_length" => str_arg(0).filter(|_| !a0.is_null()).map(|s| V::Int((s.len() * 8) as i32)).unwrap_or(V::Null),
        "byte_length" => str_arg(0).filter(|_| !a0.is_null()).map(|s| V::Int(s.len() as i32)).unwrap_or(V::Null),
        "concat" => {
            if null_if_any {
                V::Null
            } else {
                V::Str(args.iter().map(V::to_text).collect())
            }
        }
        "to_upper" | "to_lower" | "trim" | "ltrim" | "rtrim" | "reverse" => match &a0 {
            V::Null => V::Null,
            v => {
                let s = v.to_text();
                V::Str(match name {
                    "to_upper" => s.to_uppercase(),
                    "to_lower" => s.to_lowercase(),
                    "trim" => s.trim().to_string(),
                    "ltrim" => s.trim_start().to_string(),
                    "rtrim" => s.trim_end().to_string(),
                    _ => s.chars().rev().collect(),
                })
            }
        },
        "substring" => {
            if null_if_any {
                V::Null
            } else {
                let s: Vec<char> = a0.to_text().chars().collect();
                let start = args.get(1).and_then(V::as_i64).unwrap_or(1);
                let start = if start < 0 { (s.len() as i64 + start).max(0) } else { (start - 1).max(0) } as usize;
                let len = args.get(2).and_then(V::as_i64).map(|l| l.max(0) as usize).unwrap_or(s.len());
                V::Str(s.iter().skip(start).take(len).collect())
            }
        }
        "left" | "right" => {
            if null_if_any {
                V::Null
            } else {
                let s: Vec<char> = a0.to_text().chars().collect();
                let n = args.get(1).and_then(V::as_i64).unwrap_or(0).max(0) as usize;
                V::Str(if name == "left" { s.iter().take(n).collect() } else { s[s.len().saturating_sub(n)..].iter().collect() })
            }
        }
        "replace" => {
            if null_if_any {
                V::Null
            } else {
                match regex::Regex::new(&args[1].to_text()) {
                    Ok(re) => V::Str(re.replace_all(&a0.to_text(), args[2].to_text().replace('$', "$$").as_str()).into_owned()),
                    Err(e) => return Some(Err(EsError::illegal_argument(format!("Invalid regex pattern: {e}")))),
                }
            }
        }
        "starts_with" | "ends_with" => {
            if null_if_any {
                V::Null
            } else {
                let (s, p) = (a0.to_text(), args[1].to_text());
                V::Bool(if name == "starts_with" { s.starts_with(&p) } else { s.ends_with(&p) })
            }
        }
        "locate" => {
            if null_if_any {
                V::Null
            } else {
                let (s, p) = (a0.to_text(), args[1].to_text());
                let from = args.get(2).and_then(V::as_i64).unwrap_or(1).max(1) as usize - 1;
                let chars: Vec<char> = s.chars().collect();
                let hay: String = chars.iter().skip(from).collect();
                V::Int(hay.find(&p).map(|b| hay[..b].chars().count() + from + 1).unwrap_or(0) as i32)
            }
        }
        "repeat" => {
            if null_if_any {
                V::Null
            } else {
                let n = args.get(1).and_then(V::as_i64).unwrap_or(0);
                if n < 0 {
                    V::Null
                } else {
                    V::Str(a0.to_text().repeat((n as usize).min(1 << 20)))
                }
            }
        }
        "split" => {
            if null_if_any {
                V::Null
            } else {
                let parts: Vec<V> = a0.to_text().split(args[1].to_text().as_str()).map(|p| V::Str(p.to_string())).collect();
                if parts.len() == 1 { parts.into_iter().next().expect("one") } else { V::Multi(parts) }
            }
        }
        "coalesce" => args.iter().find(|v| !v.is_null()).cloned().unwrap_or(V::Null),
        "greatest" | "least" => {
            if null_if_any {
                V::Null
            } else {
                let mut all: Vec<&V> = args.iter().flat_map(V::values).collect();
                all.sort_by(|a, b| cmp(a, b));
                let pick = if name == "greatest" { all.last() } else { all.first() };
                pick.map(|v| (*v).clone()).unwrap_or(V::Null)
            }
        }
        "to_string" | "to_str" => match &a0 {
            V::Null => V::Null,
            V::Multi(v) => V::Multi(v.iter().map(|x| V::Str(x.to_text())).collect()),
            v => V::Str(v.to_text()),
        },
        "to_long" | "to_integer" | "to_int" | "to_double" | "to_dbl" | "to_boolean" | "to_bool" | "to_datetime" | "to_dt" | "to_ip" | "to_version"
        | "to_unsigned_long" => {
            let target = match name {
                "to_long" => DataType::Long,
                "to_integer" | "to_int" => DataType::Integer,
                "to_double" | "to_dbl" => DataType::Double,
                "to_boolean" | "to_bool" => DataType::Boolean,
                "to_datetime" | "to_dt" => DataType::Date,
                "to_ip" => DataType::Ip,
                "to_unsigned_long" => DataType::UnsignedLong,
                _ => DataType::Version,
            };
            convert(&a0, target)
        }
        "date_trunc" => {
            // DATE_TRUNC(interval, date): fixed intervals and calendar units.
            let (Some(unit), Some(ms)) = (args.first().map(V::to_text), args.get(1).and_then(V::as_i64)) else { return Some(Ok(V::Null)) };
            match date_trunc(&unit, ms) {
                Some(t) => V::Date(t),
                None => return Some(Err(EsError::illegal_argument(format!("Unsupported interval [{unit}]")))),
            }
        }
        "date_extract" => {
            let (Some(part), Some(ms)) = (args.first().map(V::to_text), args.get(1).and_then(V::as_i64)) else { return Some(Ok(V::Null)) };
            let dt = chrono::DateTime::from_timestamp_millis(ms).map(|d| d.naive_utc());
            let Some(dt) = dt else { return Some(Ok(V::Null)) };
            use chrono::{Datelike, Timelike};
            V::Long(match part.to_ascii_lowercase().as_str() {
                "year" => dt.year() as i64,
                "month_of_year" => dt.month() as i64,
                "day_of_month" => dt.day() as i64,
                "day_of_week" => dt.weekday().number_from_monday() as i64,
                "day_of_year" => dt.ordinal() as i64,
                "hour_of_day" => dt.hour() as i64,
                "minute_of_hour" => dt.minute() as i64,
                "second_of_minute" => dt.second() as i64,
                "milli_of_second" => (dt.nanosecond() / 1_000_000) as i64,
                other => return Some(Err(EsError::illegal_argument(format!("No enum constant java.time.temporal.ChronoField.{}", other.to_ascii_uppercase())))),
            })
        }
        "date_format" => {
            let (fmt, ms) = if args.len() == 2 { (args[0].to_text(), args[1].as_i64()) } else { ("strict_date_optional_time".to_string(), a0.as_i64()) };
            let Some(ms) = ms else { return Some(Ok(V::Null)) };
            match crate::time::DateFormatter::new(&fmt) {
                Ok(f) => V::Str(f.format_millis(ms)),
                Err(e) => return Some(Err(EsError::illegal_argument(e))),
            }
        }
        "date_parse" => {
            let (fmt, s) = if args.len() == 2 { (args[0].to_text(), &args[1]) } else { ("strict_date_optional_time".to_string(), &a0) };
            if s.is_null() {
                V::Null
            } else {
                match crate::time::DateFormatter::new(&fmt).ok().and_then(|f| f.parse_millis(&s.to_text()).ok()) {
                    Some(ms) => V::Date(ms),
                    None => V::Null,
                }
            }
        }
        "date_diff" => {
            let (Some(unit), Some(a), Some(b)) = (args.first().map(V::to_text), args.get(1).and_then(V::as_i64), args.get(2).and_then(V::as_i64)) else { return Some(Ok(V::Null)) };
            let d = b - a;
            V::Int(match unit.as_str() {
                "millisecond" | "milliseconds" | "ms" => d,
                "second" | "seconds" | "s" => d / 1000,
                "minute" | "minutes" | "mi" | "n" => d / 60_000,
                "hour" | "hours" | "hh" => d / 3_600_000,
                "day" | "days" | "dd" | "d" => d / 86_400_000,
                "week" | "weeks" | "wk" | "ww" => d / 604_800_000,
                other => return Some(Err(EsError::illegal_argument(format!("A value of [YEAR, QUARTER, MONTH, DAYOFYEAR, DAY, WEEK, WEEKDAY, HOUR, MINUTE, SECOND, MILLISECOND, MICROSECOND, NANOSECOND] or their aliases is required; received [{other}]")))),
            } as i32)
        }
        "now" => V::Date(crate::time::now_millis()),
        "mv_count" => match &a0 {
            V::Null => V::Null,
            V::Multi(v) => V::Int(v.len() as i32),
            _ => V::Int(1),
        },
        "mv_min" | "mv_max" => {
            let mut vals: Vec<&V> = a0.values();
            vals.sort_by(|a, b| cmp(a, b));
            (if name == "mv_min" { vals.first() } else { vals.last() }).map(|v| (*v).clone()).unwrap_or(V::Null)
        }
        "mv_first" => a0.values().first().map(|v| (*v).clone()).unwrap_or(V::Null),
        "mv_last" => a0.values().last().map(|v| (*v).clone()).unwrap_or(V::Null),
        "mv_sum" | "mv_avg" | "mv_median" => {
            let vals: Vec<f64> = a0.values().iter().filter_map(|v| v.as_f64()).collect();
            if vals.is_empty() {
                V::Null
            } else if name == "mv_sum" {
                match arg_types.first() {
                    Some(DataType::Double) | None => V::Double(vals.iter().sum()),
                    Some(DataType::Integer) => V::Int(vals.iter().sum::<f64>() as i32),
                    _ => V::Long(vals.iter().sum::<f64>() as i64),
                }
            } else if name == "mv_avg" {
                V::Double(vals.iter().sum::<f64>() / vals.len() as f64)
            } else {
                let mut s = vals.clone();
                s.sort_by(f64::total_cmp);
                let m = s.len() / 2;
                V::Double(if s.len() % 2 == 1 { s[m] } else { (s[m - 1] + s[m]) / 2.0 })
            }
        }
        "mv_concat" => {
            if a0.is_null() {
                V::Null
            } else {
                V::Str(a0.values().iter().map(|v| v.to_text()).collect::<Vec<_>>().join(&str_arg(1).unwrap_or_default()))
            }
        }
        "mv_dedupe" => match &a0 {
            V::Multi(v) => {
                let mut out: Vec<V> = Vec::new();
                for x in v {
                    if !out.contains(x) {
                        out.push(x.clone());
                    }
                }
                if out.len() == 1 { out.pop().expect("one") } else { V::Multi(out) }
            }
            other => other.clone(),
        },
        "cidr_match" => {
            let Some(ip) = a0.as_str().and_then(|s| s.parse::<std::net::IpAddr>().ok()) else { return Some(Ok(V::Null)) };
            V::Bool(args[1..].iter().flat_map(V::values).any(|c| cidr_contains(&c.to_text(), ip)))
        }
        _ => return None,
    };
    Some(Ok(r))
}

pub fn cidr_contains(cidr: &str, ip: std::net::IpAddr) -> bool {
    let (net, bits) = cidr.split_once('/').unwrap_or((cidr, ""));
    let Ok(net) = net.parse::<std::net::IpAddr>() else { return false };
    let (a, b) = (ip_bytes(net), ip_bytes(ip));
    let total = if net.is_ipv4() { 96 + bits.parse::<u32>().unwrap_or(32) } else { bits.parse::<u32>().unwrap_or(128) };
    let total = total.min(128) as usize;
    for i in 0..total {
        let (x, y) = (a[i / 8] >> (7 - i % 8) & 1, b[i / 8] >> (7 - i % 8) & 1);
        if x != y {
            return false;
        }
    }
    true
}

pub fn date_trunc(unit: &str, ms: i64) -> Option<i64> {
    use chrono::{Datelike, NaiveDate, TimeZone, Utc};
    let u = unit.trim().to_ascii_lowercase();
    let (n, word) = match u.split_once(' ') {
        Some((n, w)) => (n.parse::<i64>().ok()?, w.to_string()),
        None => {
            let digits: String = u.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() { (1, u.clone()) } else { (digits.parse().ok()?, u[digits.len()..].to_string()) }
        }
    };
    let fixed = |unit_ms: i64| Some(ms.div_euclid(unit_ms * n) * unit_ms * n);
    let w = word.trim_end_matches('s');
    match w {
        "millisecond" | "m" if w == "millisecond" => fixed(1),
        "ms" => fixed(1),
        "second" | "sec" => fixed(1000),
        "minute" | "min" | "m" => fixed(60_000),
        "hour" | "h" => fixed(3_600_000),
        "day" | "d" => fixed(86_400_000),
        "week" | "w" => {
            let d = chrono::DateTime::from_timestamp_millis(ms)?.date_naive();
            let monday = d - chrono::Duration::days(d.weekday().num_days_from_monday() as i64);
            Some(Utc.from_utc_datetime(&monday.and_hms_opt(0, 0, 0)?).timestamp_millis())
        }
        "month" | "quarter" | "year" => {
            let d = chrono::DateTime::from_timestamp_millis(ms)?.date_naive();
            let months = match w {
                "month" => n,
                "quarter" => 3 * n,
                _ => 12 * n,
            };
            let total = (d.year() as i64) * 12 + d.month0() as i64;
            let t = total.div_euclid(months) * months;
            let nd = NaiveDate::from_ymd_opt(t.div_euclid(12) as i32, (t.rem_euclid(12) + 1) as u32, 1)?;
            Some(Utc.from_utc_datetime(&nd.and_hms_opt(0, 0, 0)?).timestamp_millis())
        }
        _ => None,
    }
}

/// Type conversion functions (`TO_LONG`, `::long`, ...).
pub fn convert(v: &V, target: DataType) -> V {
    match v {
        V::Null => V::Null,
        V::Multi(vs) => {
            let out: Vec<V> = vs.iter().map(|x| convert(x, target)).filter(|x| !x.is_null()).collect();
            match out.len() {
                0 => V::Null,
                1 => out.into_iter().next().expect("one"),
                _ => V::Multi(out),
            }
        }
        _ => match target {
            DataType::AggregateMetricDouble => match v {
                V::Str(s) => V::Str(s.clone()),
                other => other.as_f64().and_then(|x| agg_metric_text(Some(x), Some(x), Some(x), Some(1))).map_or(V::Null, V::Str),
            },
            DataType::Long | DataType::UnsignedLong => match v {
                // Doubles round like `Math.round` (half up).
                V::Str(s) => s.trim().parse::<i64>().ok().or_else(|| s.trim().parse::<f64>().ok().filter(|d| d.abs() < 9.2e18).map(|d| (d + 0.5).floor() as i64)).map(V::Long).unwrap_or(V::Null),
                V::Bool(b) => V::Long(*b as i64),
                V::Double(d) if d.is_finite() && d.abs() < 9.2e18 => V::Long((d + 0.5).floor() as i64),
                other => other.as_i64().map(V::Long).unwrap_or(V::Null),
            },
            DataType::Integer => match convert(v, DataType::Long) {
                V::Long(l) => i32::try_from(l).map(V::Int).unwrap_or(V::Null),
                _ => V::Null,
            },
            DataType::Double => match v {
                V::Str(s) => s.trim().parse::<f64>().map(V::Double).unwrap_or(V::Null),
                V::Bool(b) => V::Double(*b as i64 as f64),
                other => other.as_f64().map(V::Double).unwrap_or(V::Null),
            },
            DataType::Boolean => match v {
                V::Bool(b) => V::Bool(*b),
                V::Str(s) => V::Bool(s.eq_ignore_ascii_case("true")),
                other => other.as_f64().map(|d| V::Bool(d != 0.0)).unwrap_or(V::Null),
            },
            DataType::Date | DataType::DateNanos => match v {
                V::Str(s) => parse_date(s).map(V::Date).unwrap_or(V::Null),
                other => other.as_i64().map(V::Date).unwrap_or(V::Null),
            },
            DataType::Ip => match v.to_text().parse::<std::net::IpAddr>() {
                Ok(ip) => V::Ip(ip.to_string()),
                Err(_) => V::Null,
            },
            DataType::Keyword | DataType::Text | DataType::Version => V::Str(v.to_text()),
            _ => V::Null,
        },
    }
}

/// The result type of a scalar function, given its argument types.
pub fn scalar_type(name: &str, args: &[DataType]) -> Option<DataType> {
    let a0 = args.first().copied().unwrap_or(DataType::Null);
    Some(match name {
        "round" | "abs" | "ceil" | "floor" | "mv_min" | "mv_max" | "mv_first" | "mv_last" | "mv_dedupe" | "coalesce" | "greatest" | "least" => {
            if name == "coalesce" || name == "greatest" || name == "least" {
                args.iter().copied().find(|t| *t != DataType::Null).unwrap_or(DataType::Null)
            } else {
                a0
            }
        }
        "sqrt" | "log10" | "log" | "exp" | "pow" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "atan2" | "sinh" | "cosh" | "tanh" | "cbrt"
        | "signum" | "pi" | "e" | "tau" | "mv_avg" | "mv_median" | "to_double" | "to_dbl" => DataType::Double,
        "mv_sum" => if a0 == DataType::Double { DataType::Double } else { a0 },
        "length" | "bit_length" | "byte_length" | "locate" | "mv_count" | "date_diff" | "to_integer" | "to_int" => DataType::Integer,
        "concat" | "to_upper" | "to_lower" | "trim" | "ltrim" | "rtrim" | "reverse" | "substring" | "left" | "right" | "replace" | "repeat" | "split"
        | "to_string" | "to_str" | "mv_concat" | "date_format" => DataType::Keyword,
        "starts_with" | "ends_with" | "cidr_match" | "to_boolean" | "to_bool" => DataType::Boolean,
        "to_long" | "date_extract" => DataType::Long,
        "to_unsigned_long" => DataType::UnsignedLong,
        "to_datetime" | "to_dt" | "date_trunc" | "date_parse" | "now" => DataType::Date,
        "to_ip" => DataType::Ip,
        "to_version" => DataType::Version,
        _ => return None,
    })
}

/// `COUNT_DISTINCT`'s default precision threshold (`CountDistinct.DEFAULT_PRECISION`).
const DEFAULT_DISTINCT_THRESHOLD: u64 = 3000;

/// A value's `COUNT_DISTINCT` hash, as `HllStates.collect` computes it: whole numbers and
/// dates through `BitMixer.mix64`, doubles through `mix64(doubleToLongBits)` (so `-0.0` and
/// `0.0` differ), and byte strings — keywords, text, the encoded version and the 16-byte IP —
/// through `mix64(MurmurHash3.hash128(…, 0).h1)`: `collect(BytesRef)` hands `h1` to the
/// `collect(long)` overload, which mixes it again (unlike the `cardinality` aggregation).
fn distinct_hash(x: &V, ty: DataType) -> Option<u64> {
    use search_aggs::util::mix64;
    let hash_bytes = |b: &[u8]| mix64(search_aggs::metrics::cardinality::hash_bytes(b));
    Some(match x {
        V::Null | V::Multi(_) => return None,
        V::Bool(b) => mix64(*b as u64),
        V::Int(i) => mix64(*i as i64 as u64),
        V::Long(i) => mix64(*i as u64),
        V::ULong(u) => mix64(*u),
        V::Date(ms) if ty == DataType::DateNanos => mix64(ms.wrapping_mul(1_000_000) as u64),
        V::Date(ms) => mix64(*ms as u64),
        // `doubleToLongBits` collapses every NaN to the canonical one.
        V::Double(d) => mix64(if d.is_nan() { f64::NAN.to_bits() } else { d.to_bits() }),
        V::Ip(s) => match s.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(a)) => hash_bytes(&a.to_ipv6_mapped().octets()),
            Ok(std::net::IpAddr::V6(a)) => hash_bytes(&a.octets()),
            Err(_) => hash_bytes(s.as_bytes()),
        },
        V::Str(s) if ty == DataType::Version => match search_aggs::version_string::encode(s) {
            Ok(b) => hash_bytes(&b),
            Err(_) => hash_bytes(s.as_bytes()),
        },
        V::Str(s) => hash_bytes(s.as_bytes()),
    })
}

pub const AGGS: &[&str] = &[
    "count", "count_distinct", "sum", "avg", "min", "max", "median", "percentile", "values", "std_dev", "median_absolute_deviation", "top", "first", "last", "weighted_avg",
    "variance", "present", "absent", "sample",
    // Time series accumulators `TS` rewrites to (the timestamp travels as the weight).
    "$first", "$last", "$rate", "$irate", "$increase", "$delta", "$idelta", "$deriv",
    // A `TS` grouping key: a series' distinct values in time order.
    "$values",
];

/// An aggregation's accumulated state.
#[derive(Clone, Debug)]
pub enum Acc {
    Count(i64),
    /// `SUM`/`MIN`/`MAX`/`COUNT`/`AVG` over `aggregate_metric_double` values: the sub-metric
    /// each combines (`[min, max, sum, value_count]`, `None` until one is seen).
    Amd { part: [Option<f64>; 4] },
    /// A time series' `(timestamp, value)` points, for the functions over time.
    Points(Vec<(i64, V)>),
    /// `COUNT_DISTINCT`: Elasticsearch's HyperLogLog++ (`HllStates`), so estimates match it
    /// to the unit and memory stays bounded by the precision whatever the cardinality.
    Distinct { hll: Box<Hll>, ty: DataType },
    /// Doubles are summed with Kahan compensation (`float`, `comp`), like Elasticsearch's
    /// `CompensatedSum`.
    Sum { int: i64, float: f64, comp: f64, any: bool, overflow: bool },
    /// `AVG` is `SUM / COUNT`: whole numbers summed exactly, doubles compensated.
    Avg { int: i128, float: f64, comp: f64, n: i64 },
    MinMax(Option<V>),
    Collect(Vec<V>),
    /// `WEIGHTED_AVG`: sums of `value * weight` and of weights.
    Weighted(f64, f64),
    /// `PRESENT` / `ABSENT`: rows seen and whether any had a value.
    Present(i64, bool),
}

impl Acc {
    pub fn new(name: &str) -> Acc {
        match name {
            "count" => Acc::Count(0),
            "$amd_sum" | "$amd_min" | "$amd_max" | "$amd_count" | "$amd_avg" => Acc::Amd { part: [None; 4] },
            "$first" | "$last" | "$rate" | "$irate" | "$increase" | "$delta" | "$idelta" | "$deriv" | "$values" => Acc::Points(Vec::new()),
            "count_distinct" => Acc::Distinct { hll: Box::new(Hll::new(precision_from_threshold(DEFAULT_DISTINCT_THRESHOLD))), ty: DataType::Null },
            "sum" => Acc::Sum { int: 0, float: 0.0, comp: 0.0, any: false, overflow: false },
            "avg" => Acc::Avg { int: 0, float: 0.0, comp: 0.0, n: 0 },
            "min" | "max" => Acc::MinMax(None),
            "weighted_avg" => Acc::Weighted(0.0, 0.0),
            "present" | "absent" => Acc::Present(0, false),
            _ => Acc::Collect(Vec::new()),
        }
    }

    /// The accumulator of an aggregation over a column of type `arg_ty` with constant
    /// parameters `extra` (`COUNT_DISTINCT`'s precision threshold).
    pub fn for_spec(name: &str, arg_ty: DataType, extra: &[V]) -> Acc {
        if name == "count_distinct" {
            // `HyperLogLogPlusPlus.precisionFromThreshold`, which caps at 18 bits.
            let threshold = extra.first().and_then(V::as_i64).map_or(DEFAULT_DISTINCT_THRESHOLD, |t| t.max(0) as u64);
            return Acc::Distinct { hll: Box::new(Hll::new(precision_from_threshold(threshold))), ty: arg_ty };
        }
        Acc::new(name)
    }

    pub fn add(&mut self, name: &str, v: &V, star: bool) {
        match self {
            // Points come with their timestamps (`add_weighted`).
            Acc::Points(_) => {}
            Acc::Amd { part } => {
                for x in v.values() {
                    let V::Str(t) = x else { continue };
                    let p = agg_metric_parts(t);
                    let merge = |a: Option<f64>, b: Option<f64>, f: fn(f64, f64) -> f64| match (a, b) {
                        (Some(a), Some(b)) => Some(f(a, b)),
                        (a, b) => a.or(b),
                    };
                    part[0] = merge(part[0], p[0], f64::min);
                    part[1] = merge(part[1], p[1], f64::max);
                    part[2] = merge(part[2], p[2], |a, b| a + b);
                    part[3] = merge(part[3], p[3], |a, b| a + b);
                }
            }
            Acc::Count(n) => {
                if star {
                    *n += 1;
                } else {
                    *n += v.values().len() as i64;
                }
            }
            Acc::Distinct { hll, ty } => {
                for x in v.values() {
                    if let Some(h) = distinct_hash(x, *ty) {
                        hll.collect(h);
                    }
                }
            }
            Acc::Sum { int, float, comp, any, overflow } => {
                for x in v.values() {
                    *any = true;
                    match x {
                        V::Double(d) => kahan(float, comp, *d),
                        other => {
                            if let Some(i) = other.as_i64() {
                                match int.checked_add(i) {
                                    Some(s) => *int = s,
                                    None => *overflow = true,
                                }
                            }
                        }
                    }
                }
            }
            Acc::Avg { int, float, comp, n } => {
                for x in v.values() {
                    match x {
                        V::Double(d) => kahan(float, comp, *d),
                        other => match other.as_i64() {
                            Some(i) => *int += i as i128,
                            None => continue,
                        },
                    }
                    *n += 1;
                }
            }
            Acc::MinMax(cur) => {
                for x in v.values() {
                    let better = match cur {
                        None => true,
                        Some(c) => {
                            let o = cmp(x, c);
                            if name == "min" { o == Ordering::Less } else { o == Ordering::Greater }
                        }
                    };
                    if better {
                        *cur = Some(x.clone());
                    }
                }
            }
            Acc::Collect(all) => {
                for x in v.values() {
                    all.push(x.clone());
                }
            }
            Acc::Weighted(..) => {}
            Acc::Present(rows, any) => {
                *rows += 1;
                *any |= !v.is_null();
            }
        }
    }

    /// `WEIGHTED_AVG(value, weight)`'s row (multi-values: every pair, like Elasticsearch).
    pub fn add_weighted(&mut self, v: &V, weight: &V) {
        if let Acc::Points(points) = self {
            if let (Some(t), false) = (weight.as_i64(), v.is_null()) {
                points.push((t, v.clone()));
            }
            return;
        }
        if let Acc::Weighted(sum, total) = self {
            let ws = weight.values();
            for (i, x) in v.values().iter().enumerate() {
                let w = ws.get(i).or(ws.first()).and_then(|w| w.as_f64());
                if let (Some(x), Some(w)) = (x.as_f64(), w) {
                    *sum += x * w;
                    *total += w;
                }
            }
        }
    }

    pub fn finish(&self, name: &str, ty: DataType, extra: &[V]) -> V {
        match self {
            Acc::Count(n) => V::Long(*n),
            Acc::Points(points) => time_series_value(name, points),
            Acc::Amd { part } => match name {
                "$amd_min" => part[0].map_or(V::Null, V::Double),
                "$amd_max" => part[1].map_or(V::Null, V::Double),
                "$amd_sum" => part[2].map_or(V::Null, V::Double),
                "$amd_count" => V::Long(part[3].unwrap_or(0.0) as i64),
                _ => match (part[2], part[3]) {
                    (Some(s), Some(n)) if n > 0.0 => V::Double(s / n),
                    _ => V::Null,
                },
            },
            Acc::Distinct { hll, .. } => V::Long(hll.cardinality() as i64),
            Acc::Sum { int, float, any, overflow, .. } => {
                if !any || *overflow {
                    V::Null
                } else if ty == DataType::Double {
                    V::Double(*float + *int as f64)
                } else {
                    V::Long(*int)
                }
            }
            Acc::Avg { int, float, n, .. } => {
                if *n == 0 {
                    V::Null
                } else {
                    V::Double((*float + *int as f64) / *n as f64)
                }
            }
            Acc::MinMax(v) => v.clone().unwrap_or(V::Null),
            Acc::Weighted(sum, total) => {
                if *total == 0.0 {
                    V::Null
                } else {
                    V::Double(sum / total)
                }
            }
            Acc::Present(rows, any) => {
                if *rows == 0 {
                    V::Null
                } else {
                    V::Bool(if name == "present" { *any } else { !*any })
                }
            }
            Acc::Collect(all) => match name {
                "values" => {
                    // Distinct values in order of first appearance, as Elasticsearch's hash of
                    // the values hands them out (`ValuesBytesRefAggregator` / `ValuesLongAggregator`).
                    let mut seen: std::collections::HashSet<super::exec::GroupKey> = std::collections::HashSet::with_capacity(all.len().min(1024));
                    let mut out: Vec<V> = Vec::new();
                    for x in all {
                        if seen.insert(super::exec::GroupKey::of(x)) {
                            out.push(x.clone());
                        }
                    }
                    match out.len() {
                        0 => V::Null,
                        1 => out.pop().expect("one"),
                        _ => V::Multi(out),
                    }
                }
                "median" | "percentile" | "median_absolute_deviation" => {
                    // A t-digest as `QuantileStates` builds it (compression 1000, the hybrid
                    // digest: exact up to 20000 values, then merging), fed in arrival order.
                    let pct = match name {
                        "percentile" => match extra.first().and_then(V::as_f64) {
                            // Out of range percentiles are null (`percentileParam`).
                            Some(p) if (0.0..=100.0).contains(&p) => p,
                            _ => return V::Null,
                        },
                        _ => 50.0,
                    };
                    let mut d = TDigest::new(QUANTILE_COMPRESSION, false);
                    for x in all {
                        if let Some(f) = x.as_f64() {
                            // ES|QL doubles are finite.
                            let _ = d.add(f);
                        }
                    }
                    if d.size() == 0 {
                        return V::Null;
                    }
                    if name == "median_absolute_deviation" {
                        // `InternalMedianAbsoluteDeviation.computeMedianAbsoluteDeviation`.
                        let median = d.quantile(0.5);
                        let mut dev = d.empty_like();
                        for (mean, count) in d.centroids() {
                            let _ = dev.add_weighted((median - mean).abs(), count);
                        }
                        return V::Double(dev.quantile(0.5));
                    }
                    V::Double(d.quantile(pct / 100.0))
                }
                "std_dev" | "variance" => {
                    // Welford's online algorithm in arrival order (`WelfordAlgorithm`).
                    let (mut n, mut mean, mut m2) = (0u64, 0.0f64, 0.0f64);
                    for x in all.iter().filter_map(V::as_f64) {
                        n += 1;
                        let delta = x - mean;
                        mean += delta / n as f64;
                        m2 += delta * (x - mean);
                    }
                    if n == 0 {
                        return V::Null;
                    }
                    let var = m2 / n as f64;
                    V::Double(if name == "variance" { var } else { var.sqrt() })
                }
                "top" => {
                    let n = extra.first().and_then(V::as_i64).unwrap_or(1).max(0) as usize;
                    let desc = extra.get(1).map(V::to_text).is_none_or(|o| !o.eq_ignore_ascii_case("asc"));
                    let mut xs = all.clone();
                    xs.sort_by(|a, b| if desc { cmp(b, a) } else { cmp(a, b) });
                    xs.truncate(n);
                    match xs.len() {
                        0 => V::Null,
                        1 => xs.pop().expect("one"),
                        _ => V::Multi(xs),
                    }
                }
                "sample" => {
                    // A uniform sample without replacement, in random order (Elasticsearch keeps
                    // the values with the smallest random keys and emits them by key).
                    use rand::seq::SliceRandom;
                    let n = extra.first().and_then(V::as_i64).unwrap_or(1).max(0) as usize;
                    let mut xs = all.clone();
                    let (picked, _) = xs.partial_shuffle(&mut rand::thread_rng(), n);
                    let mut picked = picked.to_vec();
                    match picked.len() {
                        0 => V::Null,
                        1 => picked.pop().expect("one"),
                        _ => V::Multi(picked),
                    }
                }
                "first" => all.first().cloned().unwrap_or(V::Null),
                _ => all.last().cloned().unwrap_or(V::Null),
            },
        }
    }
}

/// `QuantileStates.DEFAULT_COMPRESSION`.
const QUANTILE_COMPRESSION: f64 = 1000.0;

/// One step of Kahan summation (`CompensatedSum.add`).
fn kahan(sum: &mut f64, comp: &mut f64, x: f64) {
    if !x.is_finite() || !sum.is_finite() {
        *sum += x;
        return;
    }
    let corrected = x - *comp;
    let next = *sum + corrected;
    *comp = (next - *sum) - corrected;
    *sum = next;
}

/// The result type of an aggregation over `arg`.
/// A time series function over one series' points (in any order): `$first`/`$last` by
/// timestamp, counter `$increase`/`$rate`/`$irate` (a drop is a reset), gauge `$delta`/
/// `$idelta`, and `$deriv` (least-squares slope per second).
fn time_series_value(name: &str, points: &[(i64, V)]) -> V {
    let mut p: Vec<(i64, &V)> = points.iter().map(|(t, v)| (*t, v)).collect();
    p.sort_by_key(|(t, _)| *t);
    let num = |v: &V| v.as_f64();
    match name {
        "$first" => p.first().map_or(V::Null, |(_, v)| (*v).clone()),
        "$last" => p.last().map_or(V::Null, |(_, v)| (*v).clone()),
        "$values" => {
            let mut out: Vec<V> = Vec::new();
            for x in p.iter().flat_map(|(_, v)| v.values()) {
                if !out.iter().any(|o| super::exec::GroupKey::of(o) == super::exec::GroupKey::of(x)) {
                    out.push(x.clone());
                }
            }
            match out.len() {
                0 => V::Null,
                1 => out.pop().expect("one"),
                _ => V::Multi(out),
            }
        }
        _ => {
            let xs: Vec<(i64, f64)> = p.iter().filter_map(|(t, v)| Some((*t, num(v)?))).collect();
            if xs.len() < 2 {
                return V::Null;
            }
            let (t0, v0) = xs[0];
            let (tn, vn) = xs[xs.len() - 1];
            let (tp, vp) = xs[xs.len() - 2];
            let secs = |a: i64, b: i64| (b - a) as f64 / 1000.0;
            let increase = || xs.windows(2).map(|w| if w[1].1 >= w[0].1 { w[1].1 - w[0].1 } else { w[1].1 }).sum::<f64>();
            let out = match name {
                "$increase" => increase(),
                "$rate" if tn > t0 => increase() / secs(t0, tn),
                "$irate" if tn > tp => (if vn >= vp { vn - vp } else { vn }) / secs(tp, tn),
                "$delta" => vn - v0,
                "$idelta" => vn - vp,
                "$deriv" => {
                    let n = xs.len() as f64;
                    let (mx, my) = xs.iter().fold((0.0, 0.0), |(a, b), (t, v)| (a + secs(t0, *t), b + v));
                    let (mx, my) = (mx / n, my / n);
                    let (num, den) = xs.iter().fold((0.0, 0.0), |(a, b), (t, v)| {
                        let dx = secs(t0, *t) - mx;
                        (a + dx * (v - my), b + dx * dx)
                    });
                    if den == 0.0 {
                        return V::Null;
                    }
                    num / den
                }
                _ => return V::Null,
            };
            V::Double(out)
        }
    }
}

pub fn agg_type(name: &str, arg: DataType) -> DataType {
    match name {
        "$first" | "$last" => match arg {
            DataType::CounterLong => DataType::Long,
            DataType::CounterInteger => DataType::Integer,
            DataType::CounterDouble => DataType::Double,
            t => t,
        },
        "$rate" | "$irate" | "$increase" | "$delta" | "$idelta" | "$deriv" => DataType::Double,
        "count" | "count_distinct" => DataType::Long,
        "avg" | "median" | "percentile" | "std_dev" | "median_absolute_deviation" | "weighted_avg" | "variance" => DataType::Double,
        "present" | "absent" => DataType::Boolean,
        "sum" => {
            if arg == DataType::Double {
                DataType::Double
            } else {
                DataType::Long
            }
        }
        _ => arg,
    }
}

/// A date as ES|QL prints it: `strict_date_optional_time` with milliseconds, always.
pub fn date_string(ms: i64) -> String {
    use std::sync::OnceLock;
    static F: OnceLock<crate::time::DateFormatter> = OnceLock::new();
    F.get_or_init(|| crate::time::DateFormatter::new("strict_date_optional_time").expect("built-in format")).format_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `COUNT_DISTINCT` estimates captured from Elasticsearch 9.5.5 over the same values.
    #[test]
    fn count_distinct_is_elasticsearchs_hll() {
        fn cd(ty: DataType, extra: &[V], vals: impl Iterator<Item = V>) -> V {
            let mut a = Acc::for_spec("count_distinct", ty, extra);
            for v in vals {
                a.add("count_distinct", &v, false);
            }
            a.finish("count_distinct", DataType::Long, extra)
        }
        let kw = |i: usize| V::Str(format!("k{}", i % 5000));
        assert_eq!(cd(DataType::Keyword, &[], (0..20000).map(kw)), V::Long(5019));
        assert_eq!(cd(DataType::Keyword, &[V::Int(100)], (0..20000).map(kw)), V::Long(5101));
        assert_eq!(cd(DataType::Long, &[], (0..20000).map(|i| V::Long(i % 7000))), V::Long(6906));
        assert_eq!(cd(DataType::Long, &[V::Int(40000)], (0..20000).map(|i| V::Long(i % 7000))), V::Long(7000));
        assert_eq!(cd(DataType::Keyword, &[], (0..3500).map(|i| V::Str(i.to_string()))), V::Long(3521));
        assert_eq!(cd(DataType::Ip, &[], (0..20000).map(|i| V::Ip(format!("10.0.{}.{}", i / 256 % 256, i % 256)))), V::Long(20097));
        assert_eq!(cd(DataType::Version, &[], (0..20000).map(|i| V::Str(format!("1.{}.0", i % 4000)))), V::Long(4028));
        // Doubles hash their bits: -0.0 and 0.0 are two values; nulls count nothing.
        assert_eq!(cd(DataType::Double, &[], [V::Multi(vec![V::Double(0.0), V::Double(-0.0), V::Double(1.0)]), V::Null].into_iter()), V::Long(3));
        assert_eq!(cd(DataType::Long, &[], std::iter::empty()), V::Long(0));
    }

    fn agg(name: &str, vals: &[V], extra: &[V]) -> V {
        let mut a = Acc::new(name);
        for v in vals {
            a.add(name, v, false);
        }
        a.finish(name, agg_type(name, DataType::Double), extra)
    }

    #[test]
    fn values_keep_first_appearance_order() {
        let s = |x: &str| V::Str(x.into());
        assert_eq!(agg("values", &[V::Multi(vec![s("c"), s("a")]), s("b"), s("a")], &[]), V::Multi(vec![s("c"), s("a"), s("b")]));
        assert_eq!(agg("values", &[V::Long(3), V::Long(1), V::Long(3)], &[]), V::Multi(vec![V::Long(3), V::Long(1)]));
        assert_eq!(agg("values", &[V::Null], &[]), V::Null);
        assert_eq!(agg("values", &[V::Int(4), V::Int(4)], &[]), V::Int(4));
    }

    #[test]
    fn percentiles_follow_the_hybrid_t_digest() {
        let xs: Vec<V> = [1.0, 2.0, 3.0, 4.0].iter().map(|d| V::Double(*d)).collect();
        // Up to 20000 values the digest is exact (sorted values, linear interpolation).
        assert_eq!(agg("percentile", &xs, &[V::Int(50)]), V::Double(2.5));
        assert_eq!(agg("median", &xs, &[]), V::Double(2.5));
        assert_eq!(agg("percentile", &xs, &[V::Double(100.0)]), V::Double(4.0));
        // Out of range percentiles are null, not clamped.
        assert_eq!(agg("percentile", &xs, &[V::Int(150)]), V::Null);
        assert_eq!(agg("percentile", &xs, &[V::Int(-1)]), V::Null);
        assert_eq!(agg("median", &[V::Null], &[]), V::Null);
        assert_eq!(agg("median_absolute_deviation", &xs, &[]), V::Double(1.0));
        // Beyond that it merges, so large inputs are approximate like Elasticsearch's.
        let many: Vec<V> = (0..50_000).map(|i| V::Long((i * 7919) % 100_003)).collect();
        let V::Double(p) = agg("percentile", &many, &[V::Int(50)]) else { panic!() };
        assert!((p - 50_000.0).abs() < 200.0, "{p}");
    }

    #[test]
    fn std_dev_and_variance_use_welford() {
        let xs: Vec<V> = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0].iter().map(|d| V::Double(*d)).collect();
        assert_eq!(agg("std_dev", &xs, &[]), V::Double(2.0));
        assert_eq!(agg("variance", &xs, &[]), V::Double(4.0));
        assert_eq!(agg("variance", &[V::Multi(vec![V::Int(1), V::Int(3)])], &[]), V::Double(1.0));
        assert_eq!(agg("std_dev", &[], &[]), V::Null);
    }

    #[test]
    fn rounding_like_java() {
        assert_eq!(round_half_up(3.450233, 1), 3.5);
        assert_eq!(round_half_up(2.764889, 1), 2.8);
        assert_eq!(round_half_up(1.232382, 1), 1.2);
        assert_eq!(round_half_up(-1.5, 0), -2.0);
        assert_eq!(scalar("round", &[V::Double(0.725448), V::Int(1)], &[]).unwrap().unwrap(), V::Double(0.7));
    }

    #[test]
    fn functions() {
        assert_eq!(scalar("concat", &[V::Str("a".into()), V::Int(1)], &[]).unwrap().unwrap(), V::Str("a1".into()));
        assert_eq!(scalar("substring", &[V::Str("hello".into()), V::Int(2), V::Int(3)], &[]).unwrap().unwrap(), V::Str("ell".into()));
        assert_eq!(scalar("length", &[V::Null], &[]).unwrap().unwrap(), V::Null);
        assert_eq!(scalar("cidr_match", &[V::Ip("172.21.3.15".into()), V::Str("172.21.0.0/16".into())], &[]).unwrap().unwrap(), V::Bool(true));
        assert_eq!(scalar("date_trunc", &[V::Str("1 hour".into()), V::Date(1_698_064_503_360)], &[]).unwrap().unwrap(), V::Date(1_698_062_400_000));
        assert!(like("Connected to 10.1.0.3", "Connected*", false));
        assert!(!like("Disconnected", "Connected*", false));
        assert_eq!(convert(&V::Str("12".into()), DataType::Long), V::Long(12));
    }

    #[test]
    fn integer_overflow_is_null() {
        use super::super::parser::BinOp;
        assert_eq!(arith(BinOp::Add, &V::Int(i32::MAX), &V::Int(1), DataType::Integer), Err("java.lang.ArithmeticException: integer overflow".into()));
        assert_eq!(arith(BinOp::Add, &V::Long(i64::MAX), &V::Int(1), DataType::Long), Err("java.lang.ArithmeticException: long overflow".into()));
        assert_eq!(arith(BinOp::Div, &V::Int(7), &V::Int(2), DataType::Integer), Ok(V::Int(3)));
        assert_eq!(arith(BinOp::Div, &V::Int(7), &V::Int(0), DataType::Integer), Err("java.lang.ArithmeticException: / by zero".into()));
        assert_eq!(arith(BinOp::Div, &V::Double(7.0), &V::Int(0), DataType::Double), Err("java.lang.ArithmeticException: / by zero".into()));
        assert_eq!(arith(BinOp::Div, &V::Long(3450233), &V::Double(1000000.0), DataType::Double), Ok(V::Double(3.450233)));
        assert!(arith(BinOp::Add, &V::Multi(vec![V::Int(1), V::Int(2)]), &V::Int(1), DataType::Integer).is_err());
    }

    #[test]
    fn aggregations() {
        let mut a = Acc::new("avg");
        for v in [V::Int(1), V::Int(2), V::Null] {
            a.add("avg", &v, false);
        }
        assert_eq!(a.finish("avg", DataType::Double, &[]), V::Double(1.5));
        let mut c = Acc::new("count");
        c.add("count", &V::Null, true);
        c.add("count", &V::Null, false);
        assert_eq!(c.finish("count", DataType::Long, &[]), V::Long(1));
        let mut m = Acc::new("median");
        for v in [V::Int(1), V::Int(3), V::Int(2), V::Int(10)] {
            m.add("median", &v, false);
        }
        assert_eq!(m.finish("median", DataType::Double, &[]), V::Double(2.5));
    }
}
