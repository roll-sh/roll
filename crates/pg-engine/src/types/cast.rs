//! Casts: which conversions are allowed in which context (pg_cast), and performing them.

use super::datetime;
use super::json::Jsonb;
use super::numeric::Numeric;
use super::text::{FormatCtx, apply_char_typmod, format_value, parse_value_ex};
use super::value::USECS_PER_DAY as USECS_PER_DAY_I64;
use super::value::{USECS_PER_SEC, Value};
use super::{Category, DataType};
use crate::error::{Error, Result, code};
use crate::{bail, err};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CastContext {
    Implicit,
    Assignment,
    Explicit,
}

thread_local! {
    /// The casts CREATE CAST made in the catalog statements on this thread are bound
    /// against: (source, target, context). See `set_user_casts`.
    static USER_CASTS: std::cell::RefCell<Vec<(u32, u32, CastContext)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Make the catalog's made casts the ones `cast_context` knows (a binder over it starts).
pub fn set_user_casts<'a>(casts: impl Iterator<Item = &'a crate::catalog::userobj::Cast>) {
    let list: Vec<(u32, u32, CastContext)> = casts
        .map(|c| {
            (
                c.source,
                c.target,
                match c.context {
                    'i' => CastContext::Implicit,
                    'a' => CastContext::Assignment,
                    _ => CastContext::Explicit,
                },
            )
        })
        .collect();
    USER_CASTS.with(|u| {
        let mut u = u.borrow_mut();
        if *u != list {
            *u = list;
        }
    });
}

fn user_cast_context(from: u32, to: u32) -> Option<CastContext> {
    USER_CASTS.with(|u| {
        u.borrow()
            .iter()
            .find(|c| c.0 == from && c.1 == to)
            .map(|c| c.2)
    })
}

/// The weakest context in which `from` can be converted to `to`, or None if it can't.
pub fn cast_context(from: &DataType, to: &DataType) -> Option<CastContext> {
    use CastContext::*;
    use DataType as T;
    if from.same_base(to) {
        return Some(Implicit);
    }
    if let Some(c) = user_cast_context(from.oid(), to.oid()) {
        return Some(c);
    }
    if matches!(from, T::Unknown) {
        return Some(Implicit);
    }
    if matches!(to, T::Any) || matches!((from, to), (T::Row(_), T::Record)) {
        return Some(Implicit);
    }
    let int_like = |t: &DataType| matches!(t, T::Int2 | T::Int4 | T::Int8);
    let reg = |t: &DataType| {
        matches!(
            t,
            T::Regclass
                | T::Regtype
                | T::Regproc
                | T::Regprocedure
                | T::Regnamespace
                | T::Regrole
                | T::Regconfig
                | T::Regdictionary
                | T::Regoper
                | T::Regoperator
                | T::Regcollation
        )
    };
    let explicit_io = || {
        // Automatic I/O conversion: to string types is assignment, from string types is explicit.
        if to.category() == Category::String && !matches!(from, T::Record) || matches!(to, T::Text)
        {
            Some(Assignment)
        } else if from.category() == Category::String {
            Some(Explicit)
        } else {
            None
        }
    };
    let ctx = match (from, to) {
        // numeric promotions
        (T::Int2, T::Int4 | T::Int8 | T::Float4 | T::Float8 | T::Numeric(_)) => Implicit,
        (T::Int4, T::Int8 | T::Float4 | T::Float8 | T::Numeric(_)) => Implicit,
        (T::Int8, T::Float4 | T::Float8 | T::Numeric(_)) => Implicit,
        (T::Int4, T::Int2) | (T::Int8, T::Int2 | T::Int4) => Assignment,
        (T::Float4, T::Float8) => Implicit,
        (T::Float8, T::Float4) => Assignment,
        (T::Float4 | T::Float8, T::Int2 | T::Int4 | T::Int8 | T::Numeric(_)) => Assignment,
        (T::Numeric(_), T::Float4 | T::Float8) => Implicit,
        (T::Numeric(_), T::Int2 | T::Int4 | T::Int8) => Assignment,
        (T::Int4 | T::Int8 | T::Numeric(_), T::Money) => Assignment,
        (T::Money, T::Numeric(_)) => Assignment,
        // oids
        (f, T::Oid) if int_like(f) => Implicit,
        (f, t) if int_like(f) && reg(t) => Implicit,
        (T::Oid, t) if reg(t) => Implicit,
        (f, T::Oid) if reg(f) => Implicit,
        (f, t) if reg(f) && reg(t) => Implicit,
        (
            T::Oid
            | T::Regclass
            | T::Regtype
            | T::Regproc
            | T::Regprocedure
            | T::Regnamespace
            | T::Regrole
            | T::Regoper
            | T::Regoperator
            | T::Regconfig
            | T::Regdictionary
            | T::Regcollation,
            T::Int8,
        ) => Assignment,
        (
            T::Oid
            | T::Regclass
            | T::Regtype
            | T::Regproc
            | T::Regprocedure
            | T::Regnamespace
            | T::Regrole
            | T::Regoper
            | T::Regoperator
            | T::Regconfig
            | T::Regdictionary
            | T::Regcollation,
            T::Int4,
        ) => Assignment,
        (T::Text | T::Varchar(_) | T::Bpchar(_) | T::Name, T::Regclass) => Implicit,
        // bool
        (T::Int4, T::Bool) | (T::Bool, T::Int4) => Explicit,
        (T::Int4, T::Char) => Explicit,
        (T::Char, T::Int4) => Explicit,
        // strings
        (T::Text, T::Varchar(_) | T::Bpchar(_) | T::Name) => Implicit,
        (T::Varchar(_), T::Text | T::Bpchar(_) | T::Name) => Implicit,
        (T::Bpchar(_), T::Text | T::Varchar(_) | T::Name) => Implicit,
        (T::Name, T::Text) => Implicit,
        (T::Name, T::Varchar(_) | T::Bpchar(_)) => Assignment,
        (T::Text | T::Varchar(_) | T::Bpchar(_), T::Char) => Assignment,
        (T::Char, T::Text) => Implicit,
        (T::Char, T::Varchar(_) | T::Bpchar(_)) => Assignment,
        // date/time
        (T::Date, T::Timestamp(_) | T::Timestamptz(_)) => Implicit,
        (T::Timestamp(_), T::Timestamptz(_)) => Implicit,
        (T::Timestamptz(_), T::Timestamp(_) | T::Date | T::Time(_) | T::Timetz(_)) => Assignment,
        (T::Timestamp(_), T::Date | T::Time(_)) => Assignment,
        (T::Time(_), T::Interval(_) | T::Timetz(_)) => Implicit,
        (T::Interval(_), T::Time(_)) => Assignment,
        (T::Timetz(_), T::Time(_)) => Assignment,
        (T::Text | T::Varchar(_) | T::Bpchar(_), T::Refcursor) => Assignment,
        // json
        (T::Json, T::Jsonb) | (T::Jsonb, T::Json) => Assignment,
        (
            T::Jsonb,
            T::Bool | T::Int2 | T::Int4 | T::Int8 | T::Float4 | T::Float8 | T::Numeric(_),
        ) => Explicit,
        // bits
        (T::Bit(_), T::Varbit(_)) | (T::Varbit(_), T::Bit(_)) => Implicit,
        (T::Int4 | T::Int8, T::Bit(_)) | (T::Bit(_), T::Int4 | T::Int8) => Explicit,
        (T::Cidr, T::Inet) => Implicit,
        (T::Inet, T::Cidr) => Assignment,
        (T::Macaddr, T::Macaddr8) | (T::Macaddr8, T::Macaddr) => Implicit,
        (T::Xid8, T::Xid) => Explicit,
        (T::Int2 | T::Int4 | T::Int8, T::Bytea) | (T::Bytea, T::Int2 | T::Int4 | T::Int8) => {
            Explicit
        }
        // Extended statistics: their bytes, or their text.
        (T::PgNdistinct | T::PgDependencies | T::PgMcvList, T::Bytea | T::Text) => Implicit,
        // Binary-coercible to text.
        (T::PgNodeTree, T::Text) => Implicit,
        // arrays: element-wise
        (T::Array(a), T::Array(b)) => return cast_context(a, b),
        (T::Int2Vector, T::Array(e)) if matches!(**e, T::Int2) => Implicit,
        (T::OidVector, T::Array(e)) if matches!(**e, T::Oid) => Implicit,
        // Element-wise, as an array of oid.
        (T::OidVector, T::Array(e)) => return cast_context(&T::Oid, e),
        (_, T::AnyElement | T::AnyCompatible) => Implicit,
        (T::Range(_), T::AnyRange | T::AnyCompatibleRange)
        | (T::Multirange(_), T::AnyMultirange | T::AnyCompatibleMultirange) => Implicit,
        (T::Range(a), T::Multirange(b)) if a == b => Explicit,
        (T::Array(_), T::AnyArray | T::AnyCompatibleArray) => Implicit,
        (f, T::AnyNonArray | T::AnyCompatibleNonArray) if !f.is_array() => Implicit,
        (f, t) if f.category() == Category::Geometric && t.category() == Category::Geometric => {
            match super::geo::cast_kind(f, t) {
                Some(true) => Assignment,
                Some(false) => Explicit,
                None => return None,
            }
        }
        _ => return explicit_io(),
    };
    Some(ctx)
}

/// A binary-coercible cast (WITHOUT FUNCTION): the same bits, read as the target type's
/// value (int4 bits as a float4, ...).
pub fn binary_reinterpret(v: Value, to: &DataType) -> Value {
    // An extension's native type: its own value to and from the built-in one it is stored as
    // (citext and text).
    match to {
        DataType::Base(b) if b.ext != 0 => return crate::ext::types::from_builtin(b.ext, v),
        _ if matches!(v, Value::Ext(..)) => return crate::ext::types::to_builtin(v),
        _ => {}
    }
    let target = match to {
        DataType::Base(b) => b.repr.clone().unwrap_or(DataType::Text),
        t => t.clone(),
    };
    match (v, &target) {
        (Value::Int4(x), DataType::Float4) => Value::Float4(f32::from_bits(x as u32)),
        (Value::Float4(f), DataType::Int4) => Value::Int4(f.to_bits() as i32),
        (Value::Int8(x), DataType::Float8) => Value::Float8(f64::from_bits(x as u64)),
        (Value::Float8(f), DataType::Int8) => Value::Int8(f.to_bits() as i64),
        (Value::Oid(o), DataType::Int4) => Value::Int4(o as i32),
        (Value::Int4(x), DataType::Oid) => Value::Oid(x as u32),
        (Value::Int8(x), DataType::Money) => Value::Money(x),
        (Value::Money(x), DataType::Int8) => Value::Int8(x),
        (v, _) => v,
    }
}

pub fn int_range_error(ty: &DataType) -> Error {
    let name = match ty {
        DataType::Int2 => "smallint",
        DataType::Int4 => "integer",
        _ => "bigint",
    };
    err!(NUMERIC_VALUE_OUT_OF_RANGE, "{name} out of range")
}

fn int_from_i128(v: i128, ty: &DataType) -> Result<Value> {
    Ok(match ty {
        DataType::Int2 => Value::Int2(i16::try_from(v).map_err(|_| int_range_error(ty))?),
        DataType::Int4 => Value::Int4(i32::try_from(v).map_err(|_| int_range_error(ty))?),
        _ => Value::Int8(i64::try_from(v).map_err(|_| int_range_error(ty))?),
    })
}

/// float → int uses rint (round half to even), like Postgres.
fn float_to_int(f: f64, ty: &DataType) -> Result<Value> {
    if f.is_nan() || f.is_infinite() {
        return Err(int_range_error(ty));
    }
    let r = f.round_ties_even();
    let (lo, hi) = match ty {
        DataType::Int2 => (i16::MIN as f64, i16::MAX as f64 + 1.0),
        DataType::Int4 => (i32::MIN as f64, i32::MAX as f64 + 1.0),
        _ => (i64::MIN as f64, -(i64::MIN as f64)),
    };
    if r < lo || r >= hi {
        return Err(int_range_error(ty));
    }
    int_from_i128(r as i128, ty)
}

/// Apply a column/cast typmod to a value already of the right base type.
pub fn apply_typmod(v: Value, ty: &DataType, explicit: bool) -> Result<Value> {
    use DataType as T;
    if ty.typmod() < 0 && !matches!(ty, T::Bpchar(_)) {
        return Ok(v);
    }
    Ok(match (v, ty) {
        (Value::Text(s), T::Varchar(_) | T::Bpchar(_)) => {
            Value::Text(apply_char_typmod(&s, ty, explicit)?)
        }
        (Value::Numeric(n), T::Numeric(m)) => Value::numeric(n.apply_typmod(*m)?),
        (Value::Timestamp(t), T::Timestamp(m)) => Value::Timestamp(datetime::round_micros(t, *m)),
        (Value::Timestamptz(t), T::Timestamptz(m)) => {
            Value::Timestamptz(datetime::round_micros(t, *m))
        }
        (Value::Time(t), T::Time(m)) => Value::Time(datetime::round_micros(t, *m)),
        (Value::Timetz(t, z), T::Timetz(m)) => Value::Timetz(datetime::round_micros(t, *m), z),
        (Value::Interval(i), T::Interval(m)) => {
            Value::Interval(datetime::interval_apply_typmod(i, *m))
        }
        (Value::Bits(len, b), T::Bit(m)) if *m > 0 && len != *m as u32 => {
            if explicit {
                let bits = super::text::bits_of(len, &b);
                let mut out: Vec<bool> = bits.into_iter().take(*m as usize).collect();
                out.resize(*m as usize, false);
                super::text::bits_value(&out)
            } else {
                bail!(
                    STRING_DATA_LENGTH_MISMATCH,
                    "bit string length {len} does not match type bit({m})"
                );
            }
        }
        (Value::Bits(len, b), T::Varbit(m)) if *m > 0 && len > *m as u32 => {
            if explicit {
                let bits = super::text::bits_of(len, &b);
                super::text::bits_value(&bits[..*m as usize])
            } else {
                bail!(
                    STRING_DATA_RIGHT_TRUNCATION,
                    "bit string too long for type bit varying({m})"
                );
            }
        }
        (Value::Array(a), T::Array(elem))
            if elem.typmod() >= 0 || matches!(**elem, T::Bpchar(_)) =>
        {
            let mut a2 = (*a).clone();
            for item in a2.items.iter_mut() {
                if !item.is_null() {
                    *item = apply_typmod(std::mem::replace(item, Value::Null), elem, explicit)?;
                }
            }
            a2.elem = (**elem).clone();
            Value::Array(Arc::new(a2))
        }
        (v, _) => v,
    })
}

/// Convert a non-null value from one type to another (the cast must be allowed).
pub fn cast_value(
    v: Value,
    from: &DataType,
    to: &DataType,
    ctx: &FormatCtx,
    explicit: bool,
) -> Result<Value> {
    use DataType as T;
    if v.is_null() {
        return Ok(Value::Null);
    }
    if from.same_base(to) && !matches!(to, T::Array(_)) {
        return apply_typmod(v, to, explicit);
    }
    // A made base type converts through text (its output, then the other's input).
    if matches!(from, T::Base(_)) || matches!(to, T::Base(_)) {
        let text = match &v {
            Value::Text(t) if from.category() == Category::String => t.to_string(),
            v => super::text::format_value(v, from, ctx),
        };
        // (Into a built-in type: as text's I/O conversion to it, its typmod applied as an
        // explicit cast's length coercion is.)
        if !matches!(to, T::Base(_)) && (to.category() == Category::String || matches!(from, T::Base(b) if b.ext != 0)) {
            return cast_value(Value::text(text), &T::Text, to, ctx, explicit);
        }
        return parse_value_ex(&text, to, ctx, explicit);
    }
    if matches!(
        to,
        T::Any
            | T::AnyElement
            | T::AnyNonArray
            | T::AnyCompatible
            | T::AnyCompatibleNonArray
            | T::AnyArray
            | T::AnyCompatibleArray
    ) {
        return Ok(v);
    }
    let out = match (&v, to) {
        (Value::Bytea(_), T::Bytea)
            if matches!(from, T::PgNdistinct | T::PgDependencies | T::PgMcvList) =>
        {
            v
        }
        // to integers
        (
            Value::Int2(_) | Value::Int4(_) | Value::Int8(_) | Value::Oid(_),
            T::Int2 | T::Int4 | T::Int8,
        ) => {
            if matches!(v, Value::Oid(_)) && matches!(to, T::Int4) {
                // oid → int4 reinterprets the bits.
                Value::Int4(v.as_i64().unwrap() as u32 as i32)
            } else {
                int_from_i128(v.as_i64().unwrap() as i128, to)?
            }
        }
        (Value::Float4(f), T::Int2 | T::Int4 | T::Int8) => float_to_int(*f as f64, to)?,
        (Value::Float8(f), T::Int2 | T::Int4 | T::Int8) => float_to_int(*f, to)?,
        (Value::Numeric(n), T::Int2 | T::Int4 | T::Int8) => {
            if !n.is_finite() {
                bail!(
                    FEATURE_NOT_SUPPORTED,
                    "cannot convert {} to {}",
                    if n.is_nan() { "NaN" } else { "infinity" },
                    super::text::int_type_name(to)
                );
            }
            int_from_i128(n.to_i128_round().ok_or_else(|| int_range_error(to))?, to)?
        }
        // xid(xid8): the low 32 bits.
        (Value::Xid8(x), T::Xid) => Value::Oid(*x as u32),
        (Value::Bool(b), T::Int4) => Value::Int4(*b as i32),
        (Value::Int4(i), T::Bool) => Value::Bool(*i != 0),
        (Value::Int4(i), T::Char) => Value::Char(*i as u8),
        (Value::Char(c), T::Int4) => Value::Int4(*c as i8 as i32),
        // to floats
        (Value::Int2(_) | Value::Int4(_) | Value::Int8(_), T::Float4) => {
            Value::Float4(v.as_i64().unwrap() as f32)
        }
        (Value::Int2(_) | Value::Int4(_) | Value::Int8(_), T::Float8) => {
            Value::Float8(v.as_i64().unwrap() as f64)
        }
        (Value::Float4(f), T::Float8) => Value::Float8(*f as f64),
        (Value::Float8(f), T::Float4) => {
            let r = *f as f32;
            if r.is_infinite() && f.is_finite() {
                bail!(NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow");
            }
            if r == 0.0 && *f != 0.0 {
                bail!(NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: underflow");
            }
            Value::Float4(r)
        }
        // numeric_float8 / numeric_float4: numeric_out, then float8in / float4in.
        (Value::Numeric(n), T::Float8) if n.is_finite() => {
            Value::Float8(super::text::parse_float8(&n.to_string())?)
        }
        (Value::Numeric(n), T::Float4) if n.is_finite() => {
            Value::Float4(super::text::parse_float4(&n.to_string())?)
        }
        (Value::Numeric(n), T::Float8) => Value::Float8(n.to_f64()),
        (Value::Numeric(n), T::Float4) => Value::Float4(n.to_f64() as f32),
        // to numeric
        (Value::Int2(_) | Value::Int4(_) | Value::Int8(_), T::Numeric(m)) => {
            Value::numeric(Numeric::from_i64(v.as_i64().unwrap()).apply_typmod(*m)?)
        }
        (Value::Float8(f), T::Numeric(m)) => {
            Value::numeric(Numeric::from_f64(*f).apply_typmod(*m)?)
        }
        (Value::Float4(f), T::Numeric(m)) => {
            Value::numeric(Numeric::from_f32(*f).apply_typmod(*m)?)
        }
        (Value::Money(c), T::Numeric(m)) => {
            Value::numeric(Numeric::from_parts((*c).into(), 2).apply_typmod(*m)?)
        }
        (Value::Int4(_) | Value::Int8(_), T::Money) => Value::Money(v.as_i64().unwrap() * 100),
        (Value::Numeric(n), T::Money) => Value::Money(
            n.mul(&Numeric::from_i64(100))
                .to_i64_round()
                .ok_or_else(|| err!(NUMERIC_VALUE_OUT_OF_RANGE, "money out of range"))?,
        ),
        // oids
        (
            Value::Int2(_) | Value::Int4(_) | Value::Int8(_),
            T::Oid
            | T::Regclass
            | T::Regtype
            | T::Regproc
            | T::Regprocedure
            | T::Regnamespace
            | T::Regrole
            | T::Regconfig
            | T::Regdictionary
            | T::Regoper
            | T::Regoperator
            | T::Regcollation,
        ) => {
            let i = v.as_i64().unwrap();
            if i < i32::MIN as i64 || i > u32::MAX as i64 {
                bail!(NUMERIC_VALUE_OUT_OF_RANGE, "OID out of range");
            }
            Value::Oid(i as u32)
        }
        (
            Value::Oid(o),
            T::Oid
            | T::Regclass
            | T::Regtype
            | T::Regproc
            | T::Regprocedure
            | T::Regnamespace
            | T::Regrole
            | T::Regconfig
            | T::Regdictionary
            | T::Regoper
            | T::Regoperator
            | T::Regcollation,
        ) => Value::Oid(*o),
        // date/time
        (Value::Date(d), T::Timestamp(_)) => Value::Timestamp(date2timestamp(*d)?),
        (Value::Date(d), T::Timestamptz(_)) => Value::Timestamptz(date2timestamptz(*d, &ctx.tz)?),
        (Value::Timestamp(t), T::Timestamptz(m)) => Value::Timestamptz(datetime::round_micros(
            timestamp2timestamptz(*t, &ctx.tz)?,
            *m,
        )),
        (Value::Timestamptz(t), T::Timestamp(m)) => {
            if !datetime::ts_is_finite(*t) {
                Value::Timestamp(*t)
            } else {
                // timestamptz2timestamp
                let (tm, fsec) = datetime::timestamp2tm(*t, Some(&ctx.tz)).ok_or_else(ts_range)?;
                Value::Timestamp(datetime::round_micros(
                    datetime::tm2timestamp(&tm, fsec, None).ok_or_else(ts_range)?,
                    *m,
                ))
            }
        }
        (Value::Timestamp(t), T::Date) => Value::Date(timestamp_date(*t, None)?),
        (Value::Timestamptz(t), T::Date) => Value::Date(timestamp_date(*t, Some(&ctx.tz))?),
        (Value::Timestamp(t), T::Time(_)) | (Value::Timestamptz(t), T::Time(_)) => {
            if !datetime::ts_is_finite(*t) {
                Value::Null
            } else {
                let zone = matches!(v, Value::Timestamptz(_)).then_some(&ctx.tz);
                let (tm, fsec) = datetime::timestamp2tm(*t, zone).ok_or_else(ts_range)?;
                Value::Time(datetime::time2t(tm.hour, tm.min, tm.sec, fsec))
            }
        }
        (Value::Timestamptz(t), T::Timetz(_)) => {
            if !datetime::ts_is_finite(*t) {
                Value::Null
            } else {
                let (tm, fsec) = datetime::timestamp2tm(*t, Some(&ctx.tz)).ok_or_else(ts_range)?;
                Value::Timetz(
                    datetime::time2t(tm.hour, tm.min, tm.sec, fsec),
                    -tm.gmtoff as i32,
                )
            }
        }
        (Value::Time(t), T::Interval(_)) => Value::Interval(super::Interval::new(0, 0, *t)),
        (Value::Time(t), T::Timetz(_)) => {
            // time_timetz: the session zone's offset today at that time of day.
            let (mut tm, _) = datetime::timestamp2tm(ctx.clock.now, Some(&ctx.tz)).unwrap_or_default();
            let (h, mi, sec, _) = datetime::dt2time(*t);
            tm.hour = h;
            tm.min = mi;
            tm.sec = sec;
            Value::Timetz(*t, datetime::determine_tz_offset(&tm, &ctx.tz).0)
        }
        (Value::Timetz(t, _), T::Time(_)) => Value::Time(*t),
        (Value::Interval(i), T::Time(_)) => {
            if i.is_infinite() {
                return Err(err!(
                    DATETIME_FIELD_OVERFLOW,
                    "cannot convert infinite interval to time"
                ));
            }
            Value::Time(i.micros.rem_euclid(USECS_PER_DAY_I64))
        }
        // json
        (Value::Json(s), T::Jsonb) => Value::Jsonb(Arc::new(super::json::parse_jsonb(s)?)),
        (Value::Jsonb(j), T::Json) => Value::Json(j.to_string().into()),
        (
            Value::Jsonb(j),
            T::Bool | T::Int2 | T::Int4 | T::Int8 | T::Float4 | T::Float8 | T::Numeric(_),
        ) => jsonb_to_scalar(j, to)?,
        // network/bits
        (Value::Inet(..), T::Inet) => v.clone(),
        (Value::Inet(a, m), T::Cidr) => Value::Inet(super::text::inet_network(a, *m).into(), *m),
        (Value::Macaddr(m), T::Macaddr8) => {
            Value::Macaddr8([m[0], m[1], m[2], 0xff, 0xfe, m[3], m[4], m[5]])
        }
        (Value::Macaddr8(m), T::Macaddr) => {
            if m[3] != 0xff || m[4] != 0xfe {
                return Err(err!(NUMERIC_VALUE_OUT_OF_RANGE, "macaddr8 data out of range to convert to macaddr").hint("Only addresses that have FF and FE as values in the 4th and 5th bytes from the left, for example xx:xx:xx:ff:fe:xx:xx:xx, are eligible to be converted from macaddr8 to macaddr."));
            }
            Value::Macaddr([m[0], m[1], m[2], m[5], m[6], m[7]])
        }
        (Value::Bits(..), T::Bit(_) | T::Varbit(_)) => apply_typmod(v.clone(), to, explicit)?,
        (Value::Int4(_) | Value::Int8(_), T::Bit(m)) => {
            let width = if *m > 0 { *m as usize } else { 1 };
            let i = v.as_i64().unwrap();
            let bits: Vec<bool> = (0..width)
                .rev()
                .map(|b| if b < 64 { (i >> b) & 1 == 1 } else { false })
                .collect();
            super::text::bits_value(&bits)
        }
        (Value::Bits(len, b), T::Int4 | T::Int8) => {
            let bits = super::text::bits_of(*len, b);
            let mut i: i64 = 0;
            for bit in bits {
                i = (i << 1) | bit as i64;
            }
            if matches!(to, T::Int4) {
                Value::Int4(i as i32)
            } else {
                Value::Int8(i)
            }
        }
        // bytea_int2 & co: the bytes, big-endian, as an integer of the type's width.
        (Value::Bytea(b), T::Int2 | T::Int4 | T::Int8) => {
            let (width, name) = match to {
                T::Int2 => (2, "smallint"),
                T::Int4 => (4, "integer"),
                _ => (8, "bigint"),
            };
            if b.len() > width {
                return Err(Error::new(
                    code::NUMERIC_VALUE_OUT_OF_RANGE,
                    format!("{name} out of range"),
                ));
            }
            let u = b.iter().fold(0u64, |acc, x| (acc << 8) | *x as u64);
            match to {
                T::Int2 => Value::Int2(u as u16 as i16),
                T::Int4 => Value::Int4(u as u32 as i32),
                _ => Value::Int8(u as i64),
            }
        }
        (Value::Int2(_) | Value::Int4(_) | Value::Int8(_), T::Bytea) => {
            let bytes: Vec<u8> = match v {
                Value::Int2(i) => i.to_be_bytes().to_vec(),
                Value::Int4(i) => i.to_be_bytes().to_vec(),
                Value::Int8(i) => i.to_be_bytes().to_vec(),
                _ => unreachable!(),
            };
            Value::bytea(bytes)
        }
        // arrays
        (Value::Array(a), T::Array(elem)) => {
            let mut items = Vec::with_capacity(a.items.len());
            for item in &a.items {
                items.push(cast_value(item.clone(), &a.elem, elem, ctx, explicit)?);
            }
            Value::Array(Arc::new(super::array::ArrayValue {
                elem: (**elem).clone(),
                dims: a.dims.clone(),
                items,
            }))
        }
        (Value::Text(s), T::Char) => Value::Char(super::text::parse_char(s)),
        // strings to strings
        (Value::Text(s), T::Text | T::Varchar(_) | T::Bpchar(_) | T::Name) => {
            let s = if matches!(from, T::Bpchar(_)) && !matches!(to, T::Bpchar(_)) {
                super::text::bpchar_trim(s)
            } else {
                s
            };
            Value::Text(apply_char_typmod(s, to, explicit)?)
        }
        // text(boolean), the cast function: "true" / "false", not boolout's "t" / "f".
        (Value::Bool(b), T::Text | T::Varchar(_) | T::Bpchar(_)) => Value::Text(apply_char_typmod(
            if *b { "true" } else { "false" },
            to,
            explicit,
        )?),
        // I/O conversion
        (_, T::Text | T::Varchar(_) | T::Bpchar(_) | T::Name) => {
            let s = format_value(&v, from, ctx);
            Value::Text(apply_char_typmod(&s, to, explicit)?)
        }
        (Value::Text(s), T::Regclass) => {
            // text_regclass (unlike regclassin) looks the name up strictly: a schema that
            // isn't there is the error.
            if let Some(n) = &ctx.names
                && let Ok(parts) = crate::catalog::names::parse_qualified(s)
                && parts.len() >= 2
            {
                crate::types::text::NameResolver::nsp_oid(
                    n.as_ref(),
                    &crate::catalog::quote_ident(&parts[parts.len() - 2]),
                )?;
            }
            parse_value_ex(s, to, ctx, explicit)?
        }
        // CoerceViaIO into the type without its typmod, then the length coercion (an
        // explicit cast truncates a bit string).
        (Value::Text(s), T::Bit(_) | T::Varbit(_)) if to.typmod() > 0 => {
            let v = parse_value_ex(s, &to.without_typmod(), ctx, explicit)?;
            apply_typmod(v, to, explicit)?
        }
        (Value::Text(s), _) => parse_value_ex(s, to, ctx, explicit)?,
        // A table's row as its ancestor's (or a typed table's as its type's): field by
        // name (ConvertRowtypeExpr).
        (Value::Record(items), T::Row(tf))
            if matches!(from, T::Row(ff) if ff.oid != 0 && tf.oid != 0 && ff.oid != tf.oid
                && ff.fields.len() == items.len()
                && tf.fields.iter().all(|(n, _)| ff.fields.iter().any(|(m, _)| m == n))
                && (ff.fields.len() != tf.fields.len()
                    || ff.fields.iter().zip(&tf.fields).any(|(a, b)| a.0 != b.0))) =>
        {
            let T::Row(ff) = from else { unreachable!() };
            let mut out = Vec::with_capacity(tf.fields.len());
            for (name, ft) in &tf.fields {
                let i = ff.fields.iter().position(|(m, _)| m == name).unwrap_or(0);
                let v = &items[i];
                out.push(if v.is_null() {
                    Value::Null
                } else {
                    cast_value(v.clone(), &ff.fields[i].1, ft, ctx, explicit)?
                });
            }
            Value::Record(Arc::new(out))
        }
        (Value::Record(items), T::Row(tf)) => {
            if items.len() != tf.len() {
                let more = if items.len() > tf.len() {
                    "many"
                } else {
                    "few"
                };
                return Err(Error::new(
                    code::CANNOT_COERCE,
                    format!("cannot cast type {} to {}", from.display(), to.display()),
                )
                .detail(format!("Input has too {more} columns.")));
            }
            let mut out = Vec::with_capacity(items.len());
            for (i, (v, (_, ft))) in items.iter().zip(tf.iter()).enumerate() {
                let from_t = match from {
                    T::Row(ff) if ff.len() == items.len() => ff[i].1.clone(),
                    _ => value_type(v),
                };
                out.push(if v.is_null() {
                    Value::Null
                } else {
                    cast_value(v.clone(), &from_t, ft, ctx, explicit)?
                });
            }
            Value::Record(Arc::new(out))
        }
        (Value::Record(_), _) => {
            bail!(CANNOT_COERCE, "cannot cast type record to {}", to.display())
        }
        (Value::Range(r), T::Multirange(_)) => {
            super::range::multi_value(if r.empty { vec![] } else { vec![(**r).clone()] })
        }
        (Value::Geo(g), t)
            if t.category() == Category::Geometric && super::geo::cast(g, t)?.is_some() =>
        {
            Value::Geo(std::sync::Arc::new(super::geo::cast(g, t)?.unwrap()))
        }
        _ => {
            return Err(Error::new(
                code::CANNOT_COERCE,
                format!("cannot cast type {} to {}", from.display(), to.display()),
            ));
        }
    };
    if to.typmod() >= 0 && !matches!(out, Value::Text(_)) {
        apply_typmod(out, to, explicit)
    } else {
        Ok(out)
    }
}

fn ts_range() -> crate::error::Error {
    err!(DATETIME_FIELD_OVERFLOW, "timestamp out of range")
}

fn date_ts_range() -> crate::error::Error {
    err!(DATETIME_FIELD_OVERFLOW, "date out of range for timestamp")
}

/// date2timestamp.
fn date2timestamp(d: i32) -> Result<i64> {
    Ok(match d {
        datetime::DATE_NEG_INF => datetime::TS_NEG_INF,
        datetime::DATE_POS_INF => datetime::TS_POS_INF,
        d => {
            if d >= datetime::TIMESTAMP_END_JULIAN - datetime::POSTGRES_EPOCH_JDATE {
                return Err(date_ts_range());
            }
            d as i64 * USECS_PER_DAY_I64
        }
    })
}

/// date2timestamptz: local midnight in the session zone.
fn date2timestamptz(d: i32, tz: &datetime::TimeZone) -> Result<i64> {
    let ts = date2timestamp(d)?;
    if !datetime::ts_is_finite(ts) {
        return Ok(ts);
    }
    let (y, m, day) = datetime::j2date(d + datetime::POSTGRES_EPOCH_JDATE);
    let tm = datetime::Tm {
        year: y,
        mon: m,
        mday: day,
        ..Default::default()
    };
    let (off, _) = datetime::determine_tz_offset(&tm, tz);
    let r = ts + off as i64 * USECS_PER_SEC;
    if !datetime::ts_is_valid(r) {
        return Err(date_ts_range());
    }
    Ok(r)
}

/// timestamp2timestamptz.
pub fn timestamp2timestamptz(t: i64, tz: &datetime::TimeZone) -> Result<i64> {
    if !datetime::ts_is_finite(t) {
        return Ok(t);
    }
    let (tm, _) = datetime::timestamp2tm(t, None).ok_or_else(ts_range)?;
    let (off, _) = datetime::determine_tz_offset(&tm, tz);
    let r = t + off as i64 * USECS_PER_SEC;
    if !datetime::ts_is_valid(r) {
        return Err(ts_range());
    }
    Ok(r)
}

/// timestamp_date / timestamptz_date.
fn timestamp_date(t: i64, tz: Option<&datetime::TimeZone>) -> Result<i32> {
    Ok(match t {
        datetime::TS_NEG_INF => datetime::DATE_NEG_INF,
        datetime::TS_POS_INF => datetime::DATE_POS_INF,
        t => {
            let (tm, _) = datetime::timestamp2tm(t, tz).ok_or_else(ts_range)?;
            datetime::date2j(tm.year, tm.mon, tm.mday) - datetime::POSTGRES_EPOCH_JDATE
        }
    })
}

fn jsonb_to_scalar(j: &Jsonb, to: &DataType) -> Result<Value> {
    let tname = to.display();
    let what = j.type_name();
    let fail = || {
        err!(
            INVALID_PARAMETER_VALUE,
            "cannot cast jsonb {what} to type {tname}"
        )
    };
    match (j, to) {
        // A JSON null casts to SQL NULL (PG 18).
        (Jsonb::Null, _) => Ok(Value::Null),
        (Jsonb::Bool(b), DataType::Bool) => Ok(Value::Bool(*b)),
        (Jsonb::Number(n), DataType::Numeric(m)) => Ok(Value::numeric(n.apply_typmod(*m)?)),
        (Jsonb::Number(n), DataType::Float8) => Ok(Value::Float8(n.to_f64())),
        (Jsonb::Number(n), DataType::Float4) => Ok(Value::Float4(n.to_f64() as f32)),
        (Jsonb::Number(n), DataType::Int2 | DataType::Int4 | DataType::Int8) => {
            int_from_i128(n.to_i128_round().ok_or_else(|| int_range_error(to))?, to)
        }
        _ => Err(fail()),
    }
}

/// Least common supertype for UNION/CASE/COALESCE/VALUES (select_common_type).
pub fn common_type(types: &[DataType], context: &str) -> Result<DataType> {
    let mut iter = types.iter().filter(|t| !matches!(t, DataType::Unknown));
    let Some(first) = iter.next() else {
        // All unknown → text.
        return Ok(DataType::Text);
    };
    // select_common_typmod: an input of another type (an unknown literal too) means no typmod.
    let mut result = if types.iter().any(|t| matches!(t, DataType::Unknown)) {
        first.without_typmod()
    } else {
        first.clone()
    };
    for t in iter {
        if t.same_base(&result) {
            // Keep typmod only if all agree.
            if t.typmod() != result.typmod() {
                result = result.without_typmod();
            }
            continue;
        }
        if t.category() != result.category() {
            return Err(err!(
                DATATYPE_MISMATCH,
                "{context} types {} and {} cannot be matched",
                result.display(),
                t.display()
            ));
        }
        // Like PG: switch only when the current type isn't preferred and the
        // cast goes one way implicitly (coalesce(varchar, text) stays varchar).
        let to_t = cast_context(&result, t) == Some(CastContext::Implicit);
        let to_r = cast_context(t, &result) == Some(CastContext::Implicit);
        if !result.is_preferred() && to_t && !to_r {
            result = t.without_typmod();
        } else if to_r {
            // Mixed input types: PG keeps no typmod.
            result = result.without_typmod();
        } else {
            return Err(err!(
                DATATYPE_MISMATCH,
                "{context} types {} and {} cannot be matched",
                result.display(),
                t.display()
            ));
        }
    }
    Ok(result)
}

/// The natural type of a value whose declared type isn't known (fields of an anonymous record).
pub fn value_type(v: &Value) -> DataType {
    use DataType as T;
    match v {
        Value::Null | Value::Text(_) | Value::Ext(..) => T::Text,
        Value::Bool(_) => T::Bool,
        Value::Int2(_) => T::Int2,
        Value::Int4(_) => T::Int4,
        Value::Int8(_) => T::Int8,
        Value::Float4(_) => T::Float4,
        Value::Float8(_) => T::Float8,
        Value::Numeric(_) => T::Numeric(-1),
        Value::Char(_) => T::Char,
        Value::Bytea(_) => T::Bytea,
        Value::Date(_) => T::Date,
        Value::Time(_) => T::Time(-1),
        Value::Timetz(..) => T::Timetz(-1),
        Value::Timestamp(_) => T::Timestamp(-1),
        Value::Timestamptz(_) => T::Timestamptz(-1),
        Value::Interval(_) => T::Interval(-1),
        Value::Uuid(_) => T::Uuid,
        Value::Json(_) => T::Json,
        Value::Jsonb(_) => T::Jsonb,
        Value::Oid(_) => T::Oid,
        Value::Array(a) => DataType::array_of(a.elem.clone()),
        Value::Record(_) => T::Record,
        Value::Bits(..) => T::Varbit(-1),
        Value::Inet(..) => T::Inet,
        Value::Macaddr(_) => T::Macaddr,
        Value::Macaddr8(_) => T::Macaddr8,
        Value::TsVector(_) => T::TsVector,
        Value::TsQuery(_) => T::TsQuery,
        Value::Geo(g) => match **g {
            super::geo::Geo::Point(_) => T::Point,
            super::geo::Geo::Lseg(_) => T::Lseg,
            super::geo::Geo::Path(_) => T::Path,
            super::geo::Geo::Box(_) => T::GBox,
            super::geo::Geo::Polygon(_) => T::Polygon,
            super::geo::Geo::Line(_) => T::Line,
            super::geo::Geo::Circle(_) => T::Circle,
        },
        Value::Money(_) => T::Money,
        Value::PgLsn(_) => T::PgLsn,
        Value::Xid8(_) => T::Xid8,
        Value::Snapshot(_) => T::PgSnapshot,
        Value::Tid(..) => T::Tid,
        Value::Range(r) => match r
            .lower
            .as_ref()
            .or(r.upper.as_ref())
            .and_then(|v| super::range::builtin_for_subtype(&value_type(v)))
        {
            Some(rt) => T::Range(rt),
            None => T::Text,
        },
        Value::Multirange(m) => match m
            .first()
            .and_then(|r| r.lower.as_ref().or(r.upper.as_ref()))
            .and_then(|v| super::range::builtin_for_subtype(&value_type(v)))
        {
            Some(rt) => T::Multirange(rt),
            None => T::Text,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts() {
        assert_eq!(
            cast_context(&DataType::Int4, &DataType::Int8),
            Some(CastContext::Implicit)
        );
        assert_eq!(
            cast_context(&DataType::Int8, &DataType::Int4),
            Some(CastContext::Assignment)
        );
        assert_eq!(
            cast_context(&DataType::Int4, &DataType::Text),
            Some(CastContext::Assignment)
        );
        assert_eq!(
            cast_context(&DataType::Text, &DataType::Int4),
            Some(CastContext::Explicit)
        );
        assert_eq!(cast_context(&DataType::Bool, &DataType::Int8), None);
    }

    /// Every built-in pg_cast row between types the engine models gets that row's context
    /// (format_type / coercion follow `cast_context`, clients read pg_cast).
    #[test]
    fn contexts_match_pg_cast() {
        let mut bad = Vec::new();
        for line in include_str!("../catalog/data/pg_cast.tsv").lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let (Ok(src), Ok(dst)) = (f[1].parse::<u32>(), f[2].parse::<u32>()) else {
                continue;
            };
            let (Some(from), Some(to)) = (DataType::from_oid(src), DataType::from_oid(dst)) else {
                continue;
            };
            let want = match f[4] {
                "i" => CastContext::Implicit,
                "a" => CastContext::Assignment,
                _ => CastContext::Explicit,
            };
            let got = cast_context(&from, &to);
            if got != Some(want) {
                bad.push(format!("{from:?} -> {to:?}: pg_cast {want:?}, engine {got:?}"));
            }
        }
        assert!(bad.is_empty(), "{} casts differ:\n{}", bad.len(), bad.join("\n"));
    }

    #[test]
    fn common() {
        assert_eq!(
            common_type(&[DataType::Int4, DataType::Int8], "UNION").unwrap(),
            DataType::Int8
        );
        assert_eq!(
            common_type(&[DataType::Int4, DataType::NUMERIC], "UNION").unwrap(),
            DataType::NUMERIC
        );
        assert_eq!(
            common_type(&[DataType::NUMERIC, DataType::Float8], "UNION").unwrap(),
            DataType::Float8
        );
        assert_eq!(
            common_type(&[DataType::Unknown, DataType::Int4], "UNION").unwrap(),
            DataType::Int4
        );
        assert!(common_type(&[DataType::Int4, DataType::Text], "UNION").is_err());
        assert_eq!(
            common_type(&[DataType::Varchar(14), DataType::Text], "UNION").unwrap(),
            DataType::VARCHAR
        );
    }

    #[test]
    fn values() {
        let ctx = FormatCtx::default();
        let r = cast_value(
            Value::Float8(2.5),
            &DataType::Float8,
            &DataType::Int4,
            &ctx,
            true,
        )
        .unwrap();
        assert_eq!(r, Value::Int4(2));
        let r = cast_value(
            Value::Float8(3.5),
            &DataType::Float8,
            &DataType::Int4,
            &ctx,
            true,
        )
        .unwrap();
        assert_eq!(r, Value::Int4(4));
        assert!(
            cast_value(
                Value::Int8(1 << 40),
                &DataType::Int8,
                &DataType::Int4,
                &ctx,
                true
            )
            .is_err()
        );
        let n = cast_value(
            Value::numeric("2.5".parse().unwrap()),
            &DataType::NUMERIC,
            &DataType::Int4,
            &ctx,
            true,
        )
        .unwrap();
        assert_eq!(n, Value::Int4(3));
    }
}
