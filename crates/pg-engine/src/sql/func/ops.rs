//! Comparison, arithmetic, boolean, string and date/time operators.

use super::{FnCtx, Registry, Ret, text, text_arg};
use crate::{bail, err};
use crate::error::Result;
use crate::types::cast::int_range_error;
use crate::types::datetime;
use crate::types::numeric::Numeric;
use crate::types::text::{bpchar_trim, format_value};
use crate::types::value::{Interval, USECS_PER_DAY};
use crate::types::{DataType as T, Value};
use std::cmp::Ordering;

fn cmp(a: &Value, b: &Value) -> Ordering {
    a.cmp_total(b)
}

/// Under the call's collation when it has one (texts, and arrays of them).
fn cmp_in(a: &Value, b: &Value, c: &FnCtx) -> Ordering {
    match c.coll {
        Some(coll) => crate::types::collation::cmp_values(coll, a, b),
        None => cmp(a, b),
    }
}

macro_rules! cmp_fns {
    ($($name:ident => $test:expr),* $(,)?) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            let o = cmp_in(&a[0], &a[1], c);
            Ok(Value::Bool($test(o)))
        })*
    };
}

cmp_fns! {
    op_eq => |o| o == Ordering::Equal,
    op_ne => |o| o != Ordering::Equal,
    op_lt => |o| o == Ordering::Less,
    op_le => |o| o != Ordering::Greater,
    op_gt => |o| o == Ordering::Greater,
    op_ge => |o| o != Ordering::Less,
}

/// record_cmp / record_eq's checks of the two row types, column by column: the same
/// types, the same number of columns, and a comparison (`ordering`) or equality for each.
fn check_record_types(c: &FnCtx, ordering: bool) -> Result<()> {
    let (T::Row(l), T::Row(r)) = (c.arg_type(0), c.arg_type(1)) else {
        return Ok(());
    };
    if l.fields.is_empty() || r.fields.is_empty() {
        return Ok(());
    }
    for i in 0..l.fields.len().max(r.fields.len()) {
        let (Some((_, lt)), Some((_, rt))) = (l.fields.get(i), r.fields.get(i)) else {
            return Err(err!(
                DATATYPE_MISMATCH,
                "cannot compare record types with different numbers of columns"
            ));
        };
        if lt.oid() != rt.oid() {
            return Err(err!(
                DATATYPE_MISMATCH,
                "cannot compare dissimilar column types {} and {} at record column {}",
                lt.without_typmod().display(),
                rt.without_typmod().display(),
                i + 1
            ));
        }
        if c.exec.catalog().default_opclass(403, lt) == 0
            && !matches!(lt, T::Row(_) | T::Record | T::Array(_))
        {
            return Err(if ordering {
                err!(
                    UNDEFINED_FUNCTION,
                    "could not identify a comparison function for type {}",
                    lt.without_typmod().display()
                )
            } else {
                err!(
                    UNDEFINED_FUNCTION,
                    "could not identify an equality operator for type {}",
                    lt.without_typmod().display()
                )
            });
        }
    }
    Ok(())
}

macro_rules! record_cmp_fns {
    ($($name:ident => ($ordering:expr, $test:expr)),* $(,)?) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            check_record_types(c, $ordering)?;
            let o = cmp_in(&a[0], &a[1], c);
            Ok(Value::Bool($test(o)))
        })*
    };
}

record_cmp_fns! {
    rec_eq => (false, |o| o == Ordering::Equal),
    rec_ne => (false, |o| o != Ordering::Equal),
    rec_lt => (true, |o| o == Ordering::Less),
    rec_le => (true, |o| o != Ordering::Greater),
    rec_gt => (true, |o| o == Ordering::Greater),
    rec_ge => (true, |o| o != Ordering::Less),
}

/// A value's image as record_image_cmp compares it: by-value types as their Datum (an
/// unsigned 64-bit word), others as their bytes (then their lengths).
enum Image {
    Word(u64),
    Bytes(Vec<u8>),
}

fn image_of(v: &Value) -> Option<Image> {
    Some(match v {
        Value::Bool(b) => Image::Word(*b as u64),
        Value::Char(c) => Image::Word(*c as i8 as i64 as u64),
        Value::Int2(x) => Image::Word(*x as i64 as u64),
        Value::Int4(x) | Value::Date(x) => Image::Word(*x as i64 as u64),
        Value::Int8(x) | Value::Timestamp(x) | Value::Timestamptz(x) | Value::Time(x) | Value::Money(x) => {
            Image::Word(*x as u64)
        }
        Value::Oid(x) => Image::Word(*x as u64),
        Value::Float4(f) => Image::Word(f.to_bits() as u64),
        Value::Float8(f) => Image::Word(f.to_bits()),
        Value::Text(t) | Value::Json(t) => Image::Bytes(t.as_bytes().to_vec()),
        Value::Bytea(b) => Image::Bytes(b.to_vec()),
        Value::Uuid(u) => Image::Bytes(u.to_be_bytes().to_vec()),
        // (A numeric's image carries its display scale: 1.0 and 1.00 differ.)
        Value::Numeric(n) => Image::Bytes(n.to_string().into_bytes()),
        Value::Geo(g) => match &**g {
            crate::types::geo::Geo::Point(p) => {
                let mut b = p.x.to_le_bytes().to_vec();
                b.extend(p.y.to_le_bytes());
                Image::Bytes(b)
            }
            _ => return None,
        },
        _ => return None,
    })
}

/// record_image_cmp: column by column over the values' images (NULL after a value).
fn record_image_cmp(a: &Value, b: &Value, c: &FnCtx) -> Result<Ordering> {
    if let (T::Row(l), T::Row(r)) = (c.arg_type(0), c.arg_type(1))
        && !l.fields.is_empty()
        && !r.fields.is_empty()
    {
        for i in 0..l.fields.len().max(r.fields.len()) {
            let (Some((_, lt)), Some((_, rt))) = (l.fields.get(i), r.fields.get(i)) else {
                return Err(err!(
                    DATATYPE_MISMATCH,
                    "cannot compare record types with different numbers of columns"
                ));
            };
            if lt.oid() != rt.oid() {
                return Err(err!(
                    DATATYPE_MISMATCH,
                    "cannot compare dissimilar column types {} and {} at record column {}",
                    lt.without_typmod().display(),
                    rt.without_typmod().display(),
                    i + 1
                ));
            }
        }
    }
    let (Value::Record(x), Value::Record(y)) = (a, b) else {
        return Ok(cmp(a, b));
    };
    for (p, q) in x.iter().zip(y.iter()) {
        let o = match (p.is_null(), q.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => match (image_of(p), image_of(q)) {
                (Some(Image::Word(m)), Some(Image::Word(n))) => m.cmp(&n),
                (Some(Image::Bytes(m)), Some(Image::Bytes(n))) => {
                    let k = m.len().min(n.len());
                    m[..k].cmp(&n[..k]).then(m.len().cmp(&n.len()))
                }
                _ => cmp(p, q),
            },
        };
        if o.is_ne() {
            return Ok(o);
        }
    }
    Ok(x.len().cmp(&y.len()))
}

/// Whether two records are equal under record_image_ops (the btree family's `*=`): field by
/// field over their images (a unique index under that opclass tells `row(1.0)` and
/// `row(1.00)` apart).
pub fn record_image_equal(a: &Value, b: &Value) -> bool {
    let (Value::Record(x), Value::Record(y)) = (a, b) else {
        return cmp(a, b).is_eq();
    };
    x.len() == y.len()
        && x.iter().zip(y.iter()).all(|(p, q)| match (p.is_null(), q.is_null()) {
            (true, true) => true,
            (false, false) => match (image_of(p), image_of(q)) {
                (Some(Image::Word(m)), Some(Image::Word(n))) => m == n,
                (Some(Image::Bytes(m)), Some(Image::Bytes(n))) => m == n,
                _ => cmp(p, q).is_eq(),
            },
            _ => false,
        })
}

macro_rules! record_image_fns {
    ($($name:ident => $test:expr),* $(,)?) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            Ok(Value::Bool($test(record_image_cmp(&a[0], &a[1], c)?)))
        })*
    };
}

record_image_fns! {
    img_eq => |o| o == Ordering::Equal,
    img_ne => |o| o != Ordering::Equal,
    img_lt => |o| o == Ordering::Less,
    img_le => |o| o != Ordering::Greater,
    img_gt => |o| o == Ordering::Greater,
    img_ge => |o| o != Ordering::Less,
}

/// btoidvectorcmp / int2vectoreq: shorter vectors sort first, then element by element.
fn vector_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => x.items.len().cmp(&y.items.len()).then_with(|| {
            x.items
                .iter()
                .zip(&y.items)
                .map(|(p, q)| cmp(p, q))
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
        }),
        _ => cmp(a, b),
    }
}

macro_rules! vector_cmp_fns {
    ($($name:ident => $test:expr),* $(,)?) => {
        $(fn $name(a: &[Value], _: &FnCtx) -> Result<Value> {
            Ok(Value::Bool($test(vector_cmp(&a[0], &a[1]))))
        })*
    };
}

vector_cmp_fns! {
    vec_eq => |o| o == Ordering::Equal,
    vec_ne => |o| o != Ordering::Equal,
    vec_lt => |o| o == Ordering::Less,
    vec_le => |o| o != Ordering::Greater,
    vec_gt => |o| o == Ordering::Greater,
    vec_ge => |o| o != Ordering::Less,
}

fn bp(a: &Value) -> &str {
    bpchar_trim(text_arg(a))
}

macro_rules! bpchar_cmp {
    ($($name:ident => $test:expr),* $(,)?) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            let o = match c.coll {
                Some(coll) => coll.cmp(bp(&a[0]), bp(&a[1])),
                None => bp(&a[0]).as_bytes().cmp(bp(&a[1]).as_bytes()),
            };
            Ok(Value::Bool($test(o)))
        })*
    };
}

bpchar_cmp! {
    bp_eq => |o| o == Ordering::Equal,
    bp_ne => |o| o != Ordering::Equal,
    bp_lt => |o| o == Ordering::Less,
    bp_le => |o| o != Ordering::Greater,
    bp_gt => |o| o == Ordering::Greater,
    bp_ge => |o| o != Ordering::Less,
}

/// The comparison set for a type.
pub const CMP_OPS: [&str; 6] = ["=", "<>", "<", "<=", ">", ">="];

fn xid_eq_int4(a: &[Value], _: &FnCtx) -> Result<Value> {
    match (&a[0], &a[1]) {
        (Value::Oid(x), Value::Int4(y)) => Ok(Value::Bool(*x == *y as u32)),
        _ => Ok(Value::Null),
    }
}
fn xid_ne_int4(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(match xid_eq_int4(a, c)? {
        Value::Bool(b) => Value::Bool(!b),
        v => v,
    })
}

pub(super) fn register_cmp(r: &mut Registry, l: T, rt: T) {
    let fns: [super::FnImpl; 6] = [op_eq, op_ne, op_lt, op_le, op_gt, op_ge];
    for (op, f) in CMP_OPS.iter().zip(fns) {
        r.binop(op, l.clone(), rt.clone(), T::Bool, f);
    }
}

// ── integer arithmetic ──

fn int_result(v: i128, ty: &T) -> Result<Value> {
    match ty {
        T::Int2 => i16::try_from(v)
            .map(Value::Int2)
            .map_err(|_| int_range_error(ty)),
        T::Int4 => i32::try_from(v)
            .map(Value::Int4)
            .map_err(|_| int_range_error(ty)),
        _ => i64::try_from(v)
            .map(Value::Int8)
            .map_err(|_| int_range_error(ty)),
    }
}

fn i(a: &Value) -> i128 {
    a.as_i64().unwrap_or(0) as i128
}

fn int_add(a: &[Value], c: &FnCtx) -> Result<Value> {
    int_result(i(&a[0]) + i(&a[1]), c.ret)
}
fn int_sub(a: &[Value], c: &FnCtx) -> Result<Value> {
    int_result(i(&a[0]) - i(&a[1]), c.ret)
}
fn int_mul(a: &[Value], c: &FnCtx) -> Result<Value> {
    int_result(i(&a[0]) * i(&a[1]), c.ret)
}
fn int_div(a: &[Value], c: &FnCtx) -> Result<Value> {
    let d = i(&a[1]);
    if d == 0 {
        return Err(err!(DIVISION_BY_ZERO, "division by zero"));
    }
    int_result(i(&a[0]) / d, c.ret)
}
fn int_mod(a: &[Value], c: &FnCtx) -> Result<Value> {
    let d = i(&a[1]);
    if d == 0 {
        return Err(err!(DIVISION_BY_ZERO, "division by zero"));
    }
    int_result(i(&a[0]) % d, c.ret)
}
fn int_neg(a: &[Value], c: &FnCtx) -> Result<Value> {
    int_result(-i(&a[0]), c.ret)
}
fn identity(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(a[0].clone())
}
fn int_abs(a: &[Value], c: &FnCtx) -> Result<Value> {
    int_result(i(&a[0]).abs(), c.ret)
}
fn int_and(a: &[Value], c: &FnCtx) -> Result<Value> {
    int_result(i(&a[0]) & i(&a[1]), c.ret)
}
fn int_or(a: &[Value], c: &FnCtx) -> Result<Value> {
    int_result(i(&a[0]) | i(&a[1]), c.ret)
}
fn int_xor(a: &[Value], c: &FnCtx) -> Result<Value> {
    int_result(i(&a[0]) ^ i(&a[1]), c.ret)
}
fn int_not(a: &[Value], c: &FnCtx) -> Result<Value> {
    int_result(!i(&a[0]), c.ret)
}
fn int_shl(a: &[Value], c: &FnCtx) -> Result<Value> {
    let bits = match c.ret {
        T::Int2 => 16,
        T::Int4 => 32,
        _ => 64,
    };
    let s = i(&a[1]).rem_euclid(bits) as u32;
    Ok(match c.ret {
        T::Int2 => Value::Int2((i(&a[0]) as i16).wrapping_shl(s)),
        T::Int4 => Value::Int4((i(&a[0]) as i32).wrapping_shl(s)),
        _ => Value::Int8((i(&a[0]) as i64).wrapping_shl(s)),
    })
}
fn int_shr(a: &[Value], c: &FnCtx) -> Result<Value> {
    let bits = match c.ret {
        T::Int2 => 16,
        T::Int4 => 32,
        _ => 64,
    };
    let s = i(&a[1]).rem_euclid(bits) as u32;
    Ok(match c.ret {
        T::Int2 => Value::Int2((i(&a[0]) as i16).wrapping_shr(s)),
        T::Int4 => Value::Int4((i(&a[0]) as i32).wrapping_shr(s)),
        _ => Value::Int8((i(&a[0]) as i64).wrapping_shr(s)),
    })
}

// ── float arithmetic ──

fn f(a: &Value) -> f64 {
    a.as_f64().unwrap_or(0.0)
}

fn float_result(v: f64, ty: &T, inputs_finite: bool, zero_ok: bool) -> Result<Value> {
    if v.is_infinite() && inputs_finite {
        return Err(err!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: overflow"
        ));
    }
    if v == 0.0 && !zero_ok {
        return Err(err!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: underflow"
        ));
    }
    Ok(match ty {
        T::Float4 => {
            let r = v as f32;
            if r.is_infinite() && v.is_finite() {
                return Err(err!(
                    NUMERIC_VALUE_OUT_OF_RANGE,
                    "value out of range: overflow"
                ));
            }
            Value::Float4(r)
        }
        _ => Value::Float8(v),
    })
}

fn fl_add(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (x, y) = (f(&a[0]), f(&a[1]));
    float_result(x + y, c.ret, x.is_finite() && y.is_finite(), true)
}
fn fl_sub(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (x, y) = (f(&a[0]), f(&a[1]));
    float_result(x - y, c.ret, x.is_finite() && y.is_finite(), true)
}
fn fl_mul(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (x, y) = (f(&a[0]), f(&a[1]));
    float_result(
        x * y,
        c.ret,
        x.is_finite() && y.is_finite(),
        x == 0.0 || y == 0.0,
    )
}
fn fl_div(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (x, y) = (f(&a[0]), f(&a[1]));
    // float8_div: NaN / 0 is NaN.
    if y == 0.0 && !x.is_nan() {
        return Err(err!(DIVISION_BY_ZERO, "division by zero"));
    }
    float_result(
        x / y,
        c.ret,
        x.is_finite() && y.is_finite(),
        x == 0.0 || y.is_infinite(),
    )
}
fn fl_neg(a: &[Value], c: &FnCtx) -> Result<Value> {
    float_result(-f(&a[0]), c.ret, false, true)
}
fn fl_abs(a: &[Value], c: &FnCtx) -> Result<Value> {
    float_result(f(&a[0]).abs(), c.ret, false, true)
}
fn fl_pow(a: &[Value], _: &FnCtx) -> Result<Value> {
    super::math::float_power(f(&a[0]), f(&a[1]))
}

// ── numeric ──

fn n(a: &Value) -> Numeric {
    match a {
        Value::Numeric(n) => (**n).clone(),
        v => v
            .as_i64()
            .map(Numeric::from_i64)
            .unwrap_or_else(Numeric::zero),
    }
}

fn num_add(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(n(&a[0]).add(&n(&a[1])).checked()?))
}
fn num_sub(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(n(&a[0]).sub(&n(&a[1])).checked()?))
}
fn num_mul(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(n(&a[0]).mul(&n(&a[1])).checked()?))
}
fn num_div(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(n(&a[0]).div(&n(&a[1]))?.checked()?))
}
fn num_mod(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(n(&a[0]).rem(&n(&a[1]))?))
}
fn num_neg(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(n(&a[0]).neg()))
}
fn num_abs(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(n(&a[0]).abs()))
}
fn num_pow(a: &[Value], _: &FnCtx) -> Result<Value> {
    super::math::numeric_power(&n(&a[0]), &n(&a[1]))
}

// ── boolean ──

fn bool_and(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(
        a[0].as_bool().unwrap_or(false) && a[1].as_bool().unwrap_or(false),
    ))
}

// ── text ──

/// `a || b` where either side may be a non-text value (converted with its output function).
fn concat_any(a: &[Value], c: &FnCtx) -> Result<Value> {
    // Non-text operands go through their text cast (`true` → "true", char(n) loses its padding).
    let fmt = c.exec.fmt();
    let side = |i: usize| match (&a[i], c.arg_type(i)) {
        (Value::Text(s), T::Bpchar(_)) => s.trim_end_matches(' ').to_string(),
        (Value::Text(s), _) => s.to_string(),
        (Value::Bool(b), _) => if *b { "true" } else { "false" }.to_string(),
        (v, ty) => format_value(v, &ty, fmt),
    };
    let mut s = side(0);
    s.push_str(&side(1));
    Ok(text(s))
}

fn bytea_concat(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (Value::Bytea(x), Value::Bytea(y)) = (&a[0], &a[1]) else {
        return Ok(Value::Null);
    };
    let mut v = x.to_vec();
    v.extend_from_slice(y);
    Ok(Value::bytea(v))
}

// ── date/time ──

fn date_of(v: &Value) -> i32 {
    match v {
        Value::Date(d) => *d,
        _ => 0,
    }
}

fn ts_of(v: &Value) -> i64 {
    match v {
        Value::Timestamp(t) | Value::Timestamptz(t) | Value::Time(t) => *t,
        _ => 0,
    }
}

fn iv_of(v: &Value) -> Interval {
    match v {
        Value::Interval(i) => *i,
        _ => Interval::default(),
    }
}

fn date_out_of_range() -> crate::error::Error {
    err!(DATETIME_FIELD_OVERFLOW, "date out of range")
}
/// date_pli.
fn date_pl_int(a: &[Value], _: &FnCtx) -> Result<Value> {
    let d = date_of(&a[0]);
    if !datetime::date_is_finite(d) {
        return Ok(a[0].clone());
    }
    let r = d as i64 + i(&a[1]) as i64;
    if r < i32::MIN as i64 || r > i32::MAX as i64 || !datetime::is_valid_date(r as i32) {
        return Err(date_out_of_range());
    }
    let r = r as i32;
    Ok(Value::Date(r))
}
fn int_pl_date(a: &[Value], c: &FnCtx) -> Result<Value> {
    date_pl_int(&[a[1].clone(), a[0].clone()], c)
}
/// date_mii.
fn date_mi_int(a: &[Value], _: &FnCtx) -> Result<Value> {
    let d = date_of(&a[0]);
    if !datetime::date_is_finite(d) {
        return Ok(a[0].clone());
    }
    let r = d as i64 - i(&a[1]) as i64;
    if r < i32::MIN as i64 || r > i32::MAX as i64 || !datetime::is_valid_date(r as i32) {
        return Err(date_out_of_range());
    }
    Ok(Value::Date(r as i32))
}
fn date_mi_date(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (date_of(&a[0]), date_of(&a[1]));
    if !datetime::date_is_finite(x) || !datetime::date_is_finite(y) {
        return Err(err!(
            DATETIME_FIELD_OVERFLOW,
            "cannot subtract infinite dates"
        ));
    }
    Ok(Value::Int4(x - y))
}
/// date2timestamp.
fn date_to_ts(d: i32) -> Result<i64> {
    Ok(match d {
        datetime::DATE_POS_INF => datetime::TS_POS_INF,
        datetime::DATE_NEG_INF => datetime::TS_NEG_INF,
        d => {
            if d >= datetime::TIMESTAMP_END_JULIAN - datetime::POSTGRES_EPOCH_JDATE {
                return Err(err!(
                    DATETIME_FIELD_OVERFLOW,
                    "date out of range for timestamp"
                ));
            }
            d as i64 * USECS_PER_DAY
        }
    })
}
fn date_pl_interval(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamp(datetime::ts_add_interval(
        date_to_ts(date_of(&a[0]))?,
        &iv_of(&a[1]),
    )?))
}
fn interval_pl_date(a: &[Value], c: &FnCtx) -> Result<Value> {
    date_pl_interval(&[a[1].clone(), a[0].clone()], c)
}
fn date_mi_interval(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamp(datetime::ts_add_interval(
        date_to_ts(date_of(&a[0]))?,
        &datetime::interval_neg(&iv_of(&a[1]))?,
    )?))
}
/// datetime_timestamp: date + time.
fn date_time(d: i32, t: i64) -> Result<Value> {
    let mut r = date_to_ts(d)?;
    if datetime::ts_is_finite(r) {
        r += t;
        if !datetime::ts_is_valid(r) {
            return Err(err!(DATETIME_FIELD_OVERFLOW, "timestamp out of range"));
        }
    }
    Ok(Value::Timestamp(r))
}
fn date_pl_time(a: &[Value], _: &FnCtx) -> Result<Value> {
    date_time(date_of(&a[0]), ts_of(&a[1]))
}
fn time_pl_date(a: &[Value], _: &FnCtx) -> Result<Value> {
    date_time(date_of(&a[1]), ts_of(&a[0]))
}
fn ts_pl_interval(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamp(datetime::ts_add_interval(
        ts_of(&a[0]),
        &iv_of(&a[1]),
    )?))
}
fn interval_pl_ts(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamp(datetime::ts_add_interval(
        ts_of(&a[1]),
        &iv_of(&a[0]),
    )?))
}
fn ts_mi_interval(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamp(datetime::ts_add_interval(
        ts_of(&a[0]),
        &datetime::interval_neg(&iv_of(&a[1]))?,
    )?))
}
fn tstz_pl_interval(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamptz(datetime::tstz_add_interval(
        ts_of(&a[0]),
        &iv_of(&a[1]),
        &c.exec.fmt().tz,
    )?))
}
fn interval_pl_tstz(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamptz(datetime::tstz_add_interval(
        ts_of(&a[1]),
        &iv_of(&a[0]),
        &c.exec.fmt().tz,
    )?))
}
fn tstz_mi_interval(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamptz(datetime::tstz_add_interval(
        ts_of(&a[0]),
        &datetime::interval_neg(&iv_of(&a[1]))?,
        &c.exec.fmt().tz,
    )?))
}
fn ts_mi_ts(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(datetime::ts_diff(
        ts_of(&a[0]),
        ts_of(&a[1]),
    )?))
}
fn time_mi_time(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(Interval::new(
        0,
        0,
        ts_of(&a[0]) - ts_of(&a[1]),
    )))
}
/// time_pl_interval / time_mi_interval (the day wraps; months and days are ignored).
fn time_shift(t: i64, span: &Interval, sub: bool) -> Result<i64> {
    if span.is_infinite() {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "{}",
            if sub {
                "cannot subtract infinite interval from time"
            } else {
                "cannot add infinite interval to time"
            }
        );
    }
    let mut r = if sub {
        t.wrapping_sub(span.micros)
    } else {
        t.wrapping_add(span.micros)
    };
    r -= r / USECS_PER_DAY * USECS_PER_DAY;
    if r < 0 {
        r += USECS_PER_DAY;
    }
    Ok(r)
}
fn time_pl_interval(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Time(time_shift(ts_of(&a[0]), &iv_of(&a[1]), false)?))
}
fn interval_pl_time(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Time(time_shift(ts_of(&a[1]), &iv_of(&a[0]), false)?))
}
fn time_mi_interval(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Time(time_shift(ts_of(&a[0]), &iv_of(&a[1]), true)?))
}
fn timetz_parts(v: &Value) -> (i64, i32) {
    match v {
        Value::Timetz(t, z) => (*t, *z),
        _ => (0, 0),
    }
}
fn timetz_pl_interval(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (t, z) = timetz_parts(&a[0]);
    Ok(Value::Timetz(time_shift(t, &iv_of(&a[1]), false)?, z))
}
fn interval_pl_timetz(a: &[Value], c: &FnCtx) -> Result<Value> {
    timetz_pl_interval(&[a[1].clone(), a[0].clone()], c)
}
fn timetz_mi_interval(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (t, z) = timetz_parts(&a[0]);
    Ok(Value::Timetz(time_shift(t, &iv_of(&a[1]), true)?, z))
}
/// datetimetz_timestamptz: date + timetz is that time on that day, in the time's zone.
fn date_pl_timetz(a: &[Value], _: &FnCtx) -> Result<Value> {
    let d = date_of(&a[0]);
    if d == datetime::DATE_NEG_INF {
        return Ok(Value::Timestamptz(datetime::TS_NEG_INF));
    }
    if d == datetime::DATE_POS_INF {
        return Ok(Value::Timestamptz(datetime::TS_POS_INF));
    }
    let range = || {
        err!(
            DATETIME_FIELD_OVERFLOW,
            "date out of range for timestamp"
        )
    };
    if d >= datetime::TIMESTAMP_END_JULIAN - datetime::POSTGRES_EPOCH_JDATE {
        return Err(range());
    }
    let (t, z) = timetz_parts(&a[1]);
    let r = d as i64 * USECS_PER_DAY + t + z as i64 * 1_000_000;
    if !datetime::ts_is_valid(r) {
        return Err(range());
    }
    Ok(Value::Timestamptz(r))
}
fn timetz_pl_date(a: &[Value], c: &FnCtx) -> Result<Value> {
    date_pl_timetz(&[a[1].clone(), a[0].clone()], c)
}

// ── money ──
fn cash(v: &Value) -> i64 {
    match v {
        Value::Money(c) => *c,
        _ => 0,
    }
}
fn money_range() -> crate::error::Error {
    err!(NUMERIC_VALUE_OUT_OF_RANGE, "money out of range")
}
fn cash_pl(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Money(
        cash(&a[0])
            .checked_add(cash(&a[1]))
            .ok_or_else(money_range)?,
    ))
}
fn cash_mi(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Money(
        cash(&a[0])
            .checked_sub(cash(&a[1]))
            .ok_or_else(money_range)?,
    ))
}
fn cash_from_f64(r: f64) -> Result<Value> {
    let r = r.round_ties_even();
    if r.is_nan() || !(-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&r) {
        return Err(money_range());
    }
    Ok(Value::Money(r as i64))
}
/// money * or / a float: float8 arithmetic (overflow and division by zero as float8's), rounded.
fn cash_mul_float(c: i64, x: f64) -> Result<Value> {
    let r = c as f64 * x;
    if r.is_infinite() && x.is_finite() {
        return Err(err!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: overflow"
        ));
    }
    cash_from_f64(r)
}
fn cash_div_float(c: i64, x: f64) -> Result<Value> {
    if x == 0.0 {
        return Err(err!(DIVISION_BY_ZERO, "division by zero"));
    }
    let r = c as f64 / x;
    if r.is_infinite() && x.is_finite() {
        return Err(err!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: overflow"
        ));
    }
    cash_from_f64(r)
}
fn cash_mul_flt(a: &[Value], _: &FnCtx) -> Result<Value> {
    cash_mul_float(cash(&a[0]), f(&a[1]))
}
fn flt_mul_cash(a: &[Value], _: &FnCtx) -> Result<Value> {
    cash_mul_float(cash(&a[1]), f(&a[0]))
}
fn cash_div_flt(a: &[Value], _: &FnCtx) -> Result<Value> {
    cash_div_float(cash(&a[0]), f(&a[1]))
}
fn cash_mul_int(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Money(
        cash(&a[0])
            .checked_mul(a[1].as_i64().unwrap_or(0))
            .ok_or_else(money_range)?,
    ))
}
fn int_mul_cash(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Money(
        cash(&a[1])
            .checked_mul(a[0].as_i64().unwrap_or(0))
            .ok_or_else(money_range)?,
    ))
}
fn cash_div_int(a: &[Value], _: &FnCtx) -> Result<Value> {
    let d = a[1].as_i64().unwrap_or(0);
    if d == 0 {
        return Err(err!(DIVISION_BY_ZERO, "division by zero"));
    }
    Ok(Value::Money(
        cash(&a[0]).checked_div(d).ok_or_else(money_range)?,
    ))
}
fn cash_div_cash(a: &[Value], _: &FnCtx) -> Result<Value> {
    let d = cash(&a[1]);
    if d == 0 {
        return Err(err!(DIVISION_BY_ZERO, "division by zero"));
    }
    Ok(Value::Float8(cash(&a[0]) as f64 / d as f64))
}

/// append_num_word (cash.c).
fn append_num_word(buf: &mut String, value: u64) {
    const SMALL: [&str; 28] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen",
        "nineteen", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    let big = |i: u64| SMALL[18 + i as usize];
    let tu = value % 100;
    if value <= 20 {
        buf.push_str(SMALL[value as usize]);
        return;
    }
    if tu == 0 {
        buf.push_str(&format!("{} hundred", SMALL[(value / 100) as usize]));
        return;
    }
    if value > 99 {
        let h = SMALL[(value / 100) as usize];
        if value.is_multiple_of(10) && tu > 10 {
            buf.push_str(&format!("{h} hundred {}", big(tu / 10)));
        } else if tu < 20 {
            buf.push_str(&format!("{h} hundred and {}", SMALL[tu as usize]));
        } else {
            buf.push_str(&format!(
                "{h} hundred {} {}",
                big(tu / 10),
                SMALL[(tu % 10) as usize]
            ));
        }
    } else if value.is_multiple_of(10) && tu > 10 {
        buf.push_str(big(tu / 10));
    } else if tu < 20 {
        buf.push_str(SMALL[tu as usize]);
    } else {
        buf.push_str(&format!("{} {}", big(tu / 10), SMALL[(tu % 10) as usize]));
    }
}

/// cash_words.
fn cash_words(a: &[Value], _: &FnCtx) -> Result<Value> {
    let mut value = cash(&a[0]);
    let mut buf = String::new();
    if value < 0 {
        value = value.wrapping_neg();
        buf.push_str("minus ");
    }
    let val = value as u64;
    let dollars = val / 100;
    let m0 = val % 100;
    let parts = [
        ((val / 100_000_000_000_000_000) % 1000, " quadrillion "),
        ((val / 100_000_000_000_000) % 1000, " trillion "),
        ((val / 100_000_000_000) % 1000, " billion "),
        ((val / 100_000_000) % 1000, " million "),
        ((val / 100_000) % 1000, " thousand "),
    ];
    for (m, word) in parts {
        if m != 0 {
            append_num_word(&mut buf, m);
            buf.push_str(word);
        }
    }
    let m1 = (val / 100) % 1000;
    if m1 != 0 {
        append_num_word(&mut buf, m1);
    }
    if dollars == 0 {
        buf.push_str("zero");
    }
    buf.push_str(if dollars == 1 {
        " dollar and "
    } else {
        " dollars and "
    });
    append_num_word(&mut buf, m0);
    buf.push_str(if m0 == 1 { " cent" } else { " cents" });
    let mut out = buf.into_bytes();
    out[0] = out[0].to_ascii_uppercase();
    Ok(text(String::from_utf8_lossy(&out).into_owned()))
}

// ── pg_lsn ──
fn lsn(v: &Value) -> u64 {
    match v {
        Value::PgLsn(l) => *l,
        _ => 0,
    }
}
fn lsn_num(v: &Value) -> Numeric {
    match v {
        Value::Numeric(n) => (**n).clone(),
        v => Numeric::from_i64(v.as_i64().unwrap_or(0)),
    }
}
fn pg_lsn_mi(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(Numeric::from_i128(
        lsn(&a[0]) as i128 - lsn(&a[1]) as i128,
    )))
}
/// numeric_pg_lsn: rounded to a whole byte position.
fn lsn_of_numeric(n: &Numeric) -> Result<Value> {
    if n.is_nan() {
        return Err(err!(FEATURE_NOT_SUPPORTED, "cannot convert NaN to pg_lsn"));
    }
    if !n.is_finite() {
        return Err(err!(
            FEATURE_NOT_SUPPORTED,
            "cannot convert infinity to pg_lsn"
        ));
    }
    let v = n
        .to_i128_round()
        .filter(|v| (0..=u64::MAX as i128).contains(v))
        .ok_or_else(|| err!(INVALID_PARAMETER_VALUE, "pg_lsn out of range"))?;
    Ok(Value::PgLsn(v as u64))
}
fn pg_lsn_pli(a: &[Value], _: &FnCtx) -> Result<Value> {
    let n = lsn_num(&a[1]);
    if n.is_nan() {
        return Err(err!(NUMERIC_VALUE_OUT_OF_RANGE, "cannot add NaN to pg_lsn"));
    }
    lsn_of_numeric(&Numeric::from_i128(lsn(&a[0]) as i128).add(&n))
}
fn numeric_pl_pg_lsn(a: &[Value], c: &FnCtx) -> Result<Value> {
    pg_lsn_pli(&[a[1].clone(), a[0].clone()], c)
}
fn pg_lsn_mii(a: &[Value], _: &FnCtx) -> Result<Value> {
    let n = lsn_num(&a[1]);
    if n.is_nan() {
        return Err(err!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "cannot subtract NaN from pg_lsn"
        ));
    }
    lsn_of_numeric(&Numeric::from_i128(lsn(&a[0]) as i128).sub(&n))
}

fn iv_add(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(datetime::interval_add(
        &iv_of(&a[0]),
        &iv_of(&a[1]),
    )?))
}
fn iv_sub(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(datetime::interval_sub(
        &iv_of(&a[0]),
        &iv_of(&a[1]),
    )?))
}
fn iv_neg(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(datetime::interval_neg(&iv_of(&a[0]))?))
}
fn iv_mul(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(datetime::interval_mul(
        &iv_of(&a[0]),
        f(&a[1]),
    )?))
}
fn f_mul_iv(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(datetime::interval_mul(
        &iv_of(&a[1]),
        f(&a[0]),
    )?))
}
fn iv_div(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(datetime::interval_div(
        &iv_of(&a[0]),
        f(&a[1]),
    )?))
}

pub fn register(r: &mut Registry) {
    use T::*;
    // Comparisons for every ordered type.
    let ordered = [
        Bool,
        Int2,
        Int4,
        Int8,
        Float4,
        Float8,
        T::NUMERIC,
        Text,
        Name,
        Bytea,
        Date,
        T::TIME,
        Timetz(-1),
        T::TIMESTAMP,
        T::TIMESTAMPTZ,
        T::INTERVAL,
        Uuid,
        Jsonb,
        Oid,
        Char,
        Money,
        PgLsn,
        Xid8,
        Inet,
        Macaddr,
        Bit(-1),
        Varbit(-1),
        Tid,
        AnyArray,
    ];
    for t in ordered {
        register_cmp(r, t.clone(), t);
    }
    let rec_fns: [super::FnImpl; 6] = [rec_eq, rec_ne, rec_lt, rec_le, rec_gt, rec_ge];
    for (op, fnc) in CMP_OPS.iter().zip(rec_fns) {
        r.binop(op, Record, Record, Bool, fnc);
    }
    // record_image_*: the *= *<> *< *<= *> *>= operators.
    let img_fns: [super::FnImpl; 6] = [img_eq, img_ne, img_lt, img_le, img_gt, img_ge];
    for (op, fnc) in ["*=", "*<>", "*<", "*<=", "*>", "*>="].iter().zip(img_fns) {
        r.binop(op, Record, Record, Bool, fnc);
    }
    // xid and cid only have equality (cid not even <>); xid also compares with int4 (by bit
    // pattern).
    r.binop("=", Xid, Xid, Bool, op_eq);
    r.binop("<>", Xid, Xid, Bool, op_ne);
    r.binop("=", Cid, Cid, Bool, op_eq);
    // oidvector has the full comparison set, int2vector only equality.
    let vec_fns: [super::FnImpl; 6] = [vec_eq, vec_ne, vec_lt, vec_le, vec_gt, vec_ge];
    for (op, f) in CMP_OPS.iter().zip(vec_fns) {
        r.binop(op, OidVector, OidVector, Bool, f);
    }
    r.binop("=", Int2Vector, Int2Vector, Bool, vec_eq);
    r.binop("=", Xid, Int4, Bool, xid_eq_int4);
    r.binop("<>", Xid, Int4, Bool, xid_ne_int4);
    // bpchar ignores trailing spaces.
    let bpfns: [super::FnImpl; 6] = [bp_eq, bp_ne, bp_lt, bp_le, bp_gt, bp_ge];
    for (op, fnc) in CMP_OPS.iter().zip(bpfns) {
        r.binop(op, Bpchar(-1), Bpchar(-1), Bool, fnc);
    }
    // Cross-type integer and float comparisons, and name with text (nameeqtext & co.).
    for (a, b) in [
        (Int2, Int4),
        (Int4, Int2),
        (Int2, Int8),
        (Int8, Int2),
        (Int4, Int8),
        (Int8, Int4),
        (Float4, Float8),
        (Float8, Float4),
        (Name, Text),
        (Text, Name),
    ] {
        register_cmp(r, a, b);
    }
    // Cross-type date/time comparisons.
    for (a, b) in [
        (Date, T::TIMESTAMP),
        (T::TIMESTAMP, Date),
        (Date, T::TIMESTAMPTZ),
        (T::TIMESTAMPTZ, Date),
        (T::TIMESTAMP, T::TIMESTAMPTZ),
        (T::TIMESTAMPTZ, T::TIMESTAMP),
    ] {
        let fns: [super::FnImpl; 6] = [dt_eq, dt_ne, dt_lt, dt_le, dt_gt, dt_ge];
        for (op, fnc) in CMP_OPS.iter().zip(fns) {
            r.binop(op, a.clone(), b.clone(), Bool, fnc);
        }
    }

    // Integer arithmetic, including the cross-width variants.
    let ints = [Int2, Int4, Int8];
    for a in &ints {
        for b in &ints {
            let ret = if *a == Int8 || *b == Int8 {
                Int8
            } else if *a == Int4 || *b == Int4 {
                Int4
            } else {
                Int2
            };
            r.binop("+", a.clone(), b.clone(), ret.clone(), int_add);
            r.binop("-", a.clone(), b.clone(), ret.clone(), int_sub);
            r.binop("*", a.clone(), b.clone(), ret.clone(), int_mul);
            r.binop("/", a.clone(), b.clone(), ret.clone(), int_div);
            if a == b {
                r.binop("%", a.clone(), b.clone(), ret.clone(), int_mod);
                r.binop("&", a.clone(), b.clone(), ret.clone(), int_and);
                r.binop("|", a.clone(), b.clone(), ret.clone(), int_or);
                r.binop("#", a.clone(), b.clone(), ret.clone(), int_xor);
                r.binop("<<", a.clone(), Int4, ret.clone(), int_shl);
                r.binop(">>", a.clone(), Int4, ret.clone(), int_shr);
            }
        }
        r.prefix_op("-", a.clone(), a.clone(), int_neg);
        r.prefix_op("+", a.clone(), a.clone(), identity);
        r.prefix_op("@", a.clone(), a.clone(), int_abs);
        r.prefix_op("~", a.clone(), a.clone(), int_not);
    }
    for t in [Float4, Float8] {
        r.binop("+", t.clone(), t.clone(), t.clone(), fl_add);
        r.binop("-", t.clone(), t.clone(), t.clone(), fl_sub);
        r.binop("*", t.clone(), t.clone(), t.clone(), fl_mul);
        r.binop("/", t.clone(), t.clone(), t.clone(), fl_div);
        r.prefix_op("-", t.clone(), t.clone(), fl_neg);
        r.prefix_op("+", t.clone(), t.clone(), identity);
        r.prefix_op("@", t.clone(), t.clone(), fl_abs);
    }
    for (a, b) in [(Float4, Float8), (Float8, Float4)] {
        r.binop("+", a.clone(), b.clone(), Float8, fl_add);
        r.binop("-", a.clone(), b.clone(), Float8, fl_sub);
        r.binop("*", a.clone(), b.clone(), Float8, fl_mul);
        r.binop("/", a.clone(), b.clone(), Float8, fl_div);
    }
    r.binop("^", Float8, Float8, Float8, fl_pow);
    let num = T::NUMERIC;
    r.binop("+", num.clone(), num.clone(), num.clone(), num_add);
    r.binop("-", num.clone(), num.clone(), num.clone(), num_sub);
    r.binop("*", num.clone(), num.clone(), num.clone(), num_mul);
    r.binop("/", num.clone(), num.clone(), num.clone(), num_div);
    r.binop("%", num.clone(), num.clone(), num.clone(), num_mod);
    r.binop("^", num.clone(), num.clone(), num.clone(), num_pow);
    r.prefix_op("-", num.clone(), num.clone(), num_neg);
    r.prefix_op("+", num.clone(), num.clone(), identity);
    r.prefix_op("@", num.clone(), num.clone(), num_abs);
    r.prefix_op("|/", Float8, Float8, super::math::sqrt_f);
    r.prefix_op("||/", Float8, Float8, super::math::cbrt_f);

    // Strings.
    r.binop("||", Text, Text, Text, concat_any);
    r.binop("||", Text, AnyNonArray, Text, concat_any);
    r.binop("||", AnyNonArray, Text, Text, concat_any);
    r.binop("||", Bytea, Bytea, Bytea, bytea_concat);

    // Dates and times.
    r.binop("+", Date, Int4, Date, date_pl_int);
    r.binop("+", Int4, Date, Date, int_pl_date);
    r.binop("-", Date, Int4, Date, date_mi_int);
    r.binop("-", Date, Date, Int4, date_mi_date);
    r.binop("+", Date, T::INTERVAL, T::TIMESTAMP, date_pl_interval);
    r.binop("+", T::INTERVAL, Date, T::TIMESTAMP, interval_pl_date);
    r.binop("-", Date, T::INTERVAL, T::TIMESTAMP, date_mi_interval);
    r.binop("+", Date, T::TIME, T::TIMESTAMP, date_pl_time);
    r.binop("+", T::TIME, Date, T::TIMESTAMP, time_pl_date);
    r.binop("+", T::TIMESTAMP, T::INTERVAL, T::TIMESTAMP, ts_pl_interval);
    r.binop("+", T::INTERVAL, T::TIMESTAMP, T::TIMESTAMP, interval_pl_ts);
    r.binop("-", T::TIMESTAMP, T::INTERVAL, T::TIMESTAMP, ts_mi_interval);
    r.binop(
        "+",
        T::TIMESTAMPTZ,
        T::INTERVAL,
        T::TIMESTAMPTZ,
        tstz_pl_interval,
    );
    r.binop(
        "+",
        T::INTERVAL,
        T::TIMESTAMPTZ,
        T::TIMESTAMPTZ,
        interval_pl_tstz,
    );
    r.binop(
        "-",
        T::TIMESTAMPTZ,
        T::INTERVAL,
        T::TIMESTAMPTZ,
        tstz_mi_interval,
    );
    r.binop("-", T::TIMESTAMP, T::TIMESTAMP, T::INTERVAL, ts_mi_ts);
    r.binop("-", T::TIMESTAMPTZ, T::TIMESTAMPTZ, T::INTERVAL, ts_mi_ts);
    r.binop("-", T::TIME, T::TIME, T::INTERVAL, time_mi_time);
    r.binop("+", T::TIME, T::INTERVAL, T::TIME, time_pl_interval);
    r.binop("+", T::INTERVAL, T::TIME, T::TIME, interval_pl_time);
    r.binop("-", T::TIME, T::INTERVAL, T::TIME, time_mi_interval);
    r.binop("+", Timetz(-1), T::INTERVAL, Timetz(-1), timetz_pl_interval);
    r.binop("+", T::INTERVAL, Timetz(-1), Timetz(-1), interval_pl_timetz);
    r.binop("-", Timetz(-1), T::INTERVAL, Timetz(-1), timetz_mi_interval);
    r.binop("+", Date, Timetz(-1), T::TIMESTAMPTZ, date_pl_timetz);
    r.binop("+", Timetz(-1), Date, T::TIMESTAMPTZ, timetz_pl_date);
    r.func("cash_words", vec![Money], Text, cash_words);
    r.binop("+", Money, Money, Money, cash_pl);
    r.binop("-", Money, Money, Money, cash_mi);
    r.binop("/", Money, Money, Float8, cash_div_cash);
    for t in [Float4, Float8] {
        r.binop("*", Money, t.clone(), Money, cash_mul_flt);
        r.binop("*", t.clone(), Money, Money, flt_mul_cash);
        r.binop("/", Money, t, Money, cash_div_flt);
    }
    for t in [Int2, Int4, Int8] {
        r.binop("*", Money, t.clone(), Money, cash_mul_int);
        r.binop("*", t.clone(), Money, Money, int_mul_cash);
        r.binop("/", Money, t, Money, cash_div_int);
    }
    r.binop("-", PgLsn, PgLsn, T::NUMERIC, pg_lsn_mi);
    r.binop("+", PgLsn, T::NUMERIC, PgLsn, pg_lsn_pli);
    r.binop("+", T::NUMERIC, PgLsn, PgLsn, numeric_pl_pg_lsn);
    r.func("pg_lsn", vec![T::NUMERIC], PgLsn, |a, _| {
        lsn_of_numeric(&lsn_num(&a[0]))
    });
    r.binop("-", PgLsn, T::NUMERIC, PgLsn, pg_lsn_mii);
    r.binop("+", T::INTERVAL, T::INTERVAL, T::INTERVAL, iv_add);
    r.binop("-", T::INTERVAL, T::INTERVAL, T::INTERVAL, iv_sub);
    r.binop("*", T::INTERVAL, Float8, T::INTERVAL, iv_mul);
    r.binop("*", Float8, T::INTERVAL, T::INTERVAL, f_mul_iv);
    r.binop("/", T::INTERVAL, Float8, T::INTERVAL, iv_div);
    r.prefix_op("-", T::INTERVAL, T::INTERVAL, iv_neg);

    r.binop_ret("AND", Bool, Bool, Ret::Fixed(Bool), bool_and);
}

/// Mixed date/timestamp comparisons: compare as timestamptz instants.
fn as_instant(v: &Value, c: &FnCtx) -> i64 {
    match v {
        // date2timestamp_opt_overflow: dates past the timestamp range sort as infinity.
        Value::Date(d) => date_to_ts(*d).unwrap_or(datetime::TS_POS_INF),
        Value::Timestamp(t) => *t,
        Value::Timestamptz(t) => {
            // Compare timestamptz against local timestamps in the session zone.
            if !datetime::ts_is_finite(*t) {
                return *t;
            }
            t + c.exec.fmt().tz.offset_at_utc(*t) as i64 * crate::types::value::USECS_PER_SEC
        }
        _ => 0,
    }
}

macro_rules! dt_cmp {
    ($($name:ident => $test:expr),* $(,)?) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            let o = as_instant(&a[0], c).cmp(&as_instant(&a[1], c));
            Ok(Value::Bool($test(o)))
        })*
    };
}

dt_cmp! {
    dt_eq => |o| o == Ordering::Equal,
    dt_ne => |o| o != Ordering::Equal,
    dt_lt => |o| o == Ordering::Less,
    dt_le => |o| o != Ordering::Greater,
    dt_gt => |o| o == Ordering::Greater,
    dt_ge => |o| o != Ordering::Less,
}
