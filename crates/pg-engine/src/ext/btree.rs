//! btree_gin and btree_gist (`contrib/btree_gin`, `contrib/btree_gist`): GIN and GiST
//! operator classes for the btree-orderable built-in types, and btree_gist's distance
//! functions. Their indexes are read as every index here is (the executor checks the
//! conditions on the rows), so the classes' support functions only exist to be named; the
//! SQL-callable functions are real.

use super::native::Native;
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::FnCtx;
use crate::types::{DataType, Value};

fn support_only(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::error::internal("index support function called outside an index"))
}

fn out_of_range(what: &str) -> Error {
    Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, format!("{what} out of range"))
}

/// `|a - b|` of two integers, PostgreSQL's overflow errors (the minimum's absolute value
/// overflows too).
fn int_dist(a: i64, b: i64, min: i64, max: i64, what: &str) -> Result<i64> {
    let r = a.checked_sub(b).filter(|r| *r >= min && *r <= max && *r != min).ok_or_else(|| out_of_range(what))?;
    Ok(r.abs())
}

/// float4_dist / float8_dist: `|a - b|`, computed in the type's precision as `r` (NaNs
/// equal each other and are farthest from the rest; an overflow is an error).
fn float_dist(a: f64, b: f64, r: f64) -> Result<f64> {
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow"));
    }
    Ok(if r.is_nan() {
        if a.is_nan() != b.is_nan() { f64::INFINITY } else { 0.0 }
    } else {
        r.abs()
    })
}

/// A built-in binary operator applied to two values of `t`.
fn builtin_op(op: &str, t: &DataType, a: &Value, b: &Value, c: &FnCtx) -> Result<Value> {
    let r = crate::sql::func::resolve_binop(op, t, t)?;
    (r.def.imp)(&[a.clone(), b.clone()], c)
}

/// `abs_interval`: a negative interval negated (interval_um).
fn abs_interval(v: Value, c: &FnCtx) -> Result<Value> {
    let zero = Value::Interval(Default::default());
    if builtin_op("<", &DataType::Interval(-1), &v, &zero, c)? == Value::Bool(true) {
        let r = crate::sql::func::resolve_prefix("-", &DataType::Interval(-1))?;
        return (r.def.imp)(&[v], c);
    }
    Ok(v)
}

/// time_dist / interval_dist / ts_dist / tstz_dist: `|a - b|` as an interval (an infinite
/// timestamp: the largest interval).
fn time_like_dist(a: &[Value], c: &FnCtx, t: DataType, timestamp: bool) -> Result<Value> {
    if timestamp {
        let inf = |v: &Value| matches!(v, Value::Timestamp(x) | Value::Timestamptz(x) if *x == i64::MIN || *x == i64::MAX);
        if inf(&a[0]) || inf(&a[1]) {
            return Ok(Value::Interval(crate::types::Interval { months: i32::MAX, days: i32::MAX, micros: i64::MAX }));
        }
    }
    let d = builtin_op("-", &t, &a[0], &a[1], c)?;
    abs_interval(d, c)
}

fn i(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn gist_symbol(sym: &str) -> Option<crate::sql::func::FnImpl> {
    Some(match sym {
        "int2_dist" => |a, _| Ok(Value::Int2(int_dist(i(&a[0]), i(&a[1]), i16::MIN as i64, i16::MAX as i64, "smallint")? as i16)),
        "int4_dist" => |a, _| Ok(Value::Int4(int_dist(i(&a[0]), i(&a[1]), i32::MIN as i64, i32::MAX as i64, "integer")? as i32)),
        "int8_dist" => |a, _| {
            let (x, y) = (i(&a[0]), i(&a[1]));
            let r = x.checked_sub(y).filter(|r| *r != i64::MIN).ok_or_else(|| out_of_range("bigint"))?;
            Ok(Value::Int8(r.abs()))
        },
        "cash_dist" => |a, _| {
            let (Value::Money(x), Value::Money(y)) = (&a[0], &a[1]) else { return Ok(Value::Null) };
            let r = x.checked_sub(*y).filter(|r| *r != i64::MIN).ok_or_else(|| out_of_range("money"))?;
            Ok(Value::Money(r.abs()))
        },
        "oid_dist" => |a, _| {
            let (x, y) = (i(&a[0]) as u32, i(&a[1]) as u32);
            Ok(Value::Oid(x.abs_diff(y)))
        },
        "float4_dist" => |a, _| {
            let (x, y) = (f(&a[0]) as f32, f(&a[1]) as f32);
            Ok(Value::Float4(float_dist(x as f64, y as f64, (x - y) as f64)? as f32))
        },
        "float8_dist" => |a, _| {
            let (x, y) = (f(&a[0]), f(&a[1]));
            Ok(Value::Float8(float_dist(x, y, x - y)?))
        },
        "date_dist" => |a, _| {
            let (Value::Date(x), Value::Date(y)) = (&a[0], &a[1]) else { return Ok(Value::Null) };
            Ok(Value::Int4((*x as i64 - *y as i64).abs() as i32))
        },
        "time_dist" => |a, c| time_like_dist(a, c, DataType::Time(-1), false),
        "interval_dist" => |a, c| time_like_dist(a, c, DataType::Interval(-1), false),
        "ts_dist" => |a, c| time_like_dist(a, c, DataType::Timestamp(-1), true),
        "tstz_dist" => |a, c| time_like_dist(a, c, DataType::Timestamptz(-1), true),
        "gist_translate_cmptype_btree" => |a, _| {
            let n = i(&a[0]);
            Ok(Value::Int2(if (1..=5).contains(&n) { n as i16 } else { 0 }))
        },
        "gbtreekey_out" => |_, _| {
            Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot display a value of type gbtreekey?"))
        },
        _ => return None,
    })
}

pub(super) fn btree_gist(sym: &str) -> Option<Native> {
    if let Some(f) = gist_symbol(sym) {
        return Some(Native::scalar(f));
    }
    super::script_symbols("btree_gist").contains(sym).then(|| Native::scalar(support_only))
}

pub(super) fn btree_gin(sym: &str) -> Option<Native> {
    let f: crate::sql::func::FnImpl = match sym {
        // (A GIN compare function over two non-null values: their btree order.)
        "gin_numeric_cmp" | "gin_enum_cmp" => |a, _| {
            Ok(Value::Int4(match a[0].cmp_total(&a[1]) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        },
        s if super::script_symbols("btree_gin").contains(s) => support_only,
        _ => return None,
    };
    Some(Native::scalar(f))
}
