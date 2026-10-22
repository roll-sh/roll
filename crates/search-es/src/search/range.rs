//! Numeric, date, IP and range-field term and range queries with Elasticsearch's value rules:
//! `NumberFieldMapper.NumberType` parsing (decimal parts, ranges, `coerce`), date math with
//! `format` / `time_zone` and rounding, `InetAddressPoint` ranges and CIDR terms, and the
//! `RangeFieldMapper` relations (`intersects`, `within`, `contains`) on range fields' points.
//! Indexed fields with doc values get `IndexOrDocValuesQuery`, as `NumberFieldType` builds.

use super::query::{QueryCtx, arc, match_none, normalize, value_text};
use crate::engine::fields::{FieldInfo, Kind};
use crate::error::{EsError, EsResult};
use crate::json::{Number, Value};
use crate::query::term_level::{Fuzziness, Range, Relation};
use crate::time::{DateFormatter, Zone};
use search_codec::points::encode;
use search_index::query::{
    ConstantScoreQuery, DocValuesRangeQuery, DvRange, IndexOrDocValuesQuery, MultiTermQuery, PointInSetQuery, PointRangeQuery, PointType,
};
use search_index::QueryRef;
use std::sync::Arc;

fn not_searchable(f: &FieldInfo) -> EsError {
    EsError::illegal_argument(format!("Cannot search on field [{}] since it is not indexed nor has doc values.", f.name))
}

/// A long value of a query (`Numbers.toLong` with coercion).
pub fn parse_long_value(v: &Value) -> EsResult<i64> {
    match v {
        Value::Num(Number::Int(i)) => Ok(*i),
        Value::Num(n) => {
            let f = n.as_f64();
            if f.is_finite() && (-9.223372036854776e18..9.223372036854776e18).contains(&f) {
                Ok(f as i64)
            } else {
                Err(EsError::illegal_argument(format!("Value [{}] is out of range for a long", v.to_es_string())))
            }
        }
        // `NumberType.objectToLong`: range-checked as a double, then read exactly
        // (`Numbers.toLong`, through `BigDecimal`) so no digit is lost.
        Value::Str(s) => {
            if let Some(i) = crate::units::parse_java_long(s) {
                return Ok(i);
            }
            match crate::units::parse_java_double(s) {
                Some(f) if (-9.223372036854776e18..=9.223372036854776e18).contains(&f) => crate::query::xparser::java_to_long(s),
                Some(_) => Err(EsError::illegal_argument(format!("Value [{s}] is out of range for a long"))),
                None => Err(EsError::number_format(s)),
            }
        }
        Value::Bool(b) => Err(EsError::illegal_argument(format!("Cannot convert {b} to a number"))),
        other => Err(EsError::number_format(&other.to_es_string())),
    }
}

/// The double of a query value (`parseDouble` / `Number.doubleValue`).
fn parse_double_value(v: &Value) -> EsResult<f64> {
    match v {
        Value::Num(n) => Ok(n.as_f64()),
        Value::Str(s) => crate::units::parse_java_double(s.trim()).ok_or_else(|| EsError::number_format(s)),
        other => Err(EsError::number_format(&other.to_es_string())),
    }
}

fn has_decimal(v: &Value) -> bool {
    match v {
        Value::Num(Number::Float(f)) => f.fract() != 0.0,
        Value::Str(s) => s.trim().parse::<f64>().map(|f| f.fract() != 0.0).unwrap_or(false),
        _ => false,
    }
}

fn signum(v: &Value) -> f64 {
    parse_double_value(v).map(|d| d.signum()).unwrap_or(0.0)
}

fn int_bounds(k: Kind) -> (i64, i64, &'static str) {
    match k {
        Kind::Integer | Kind::TokenCount => (i32::MIN as i64, i32::MAX as i64, "an integer"),
        Kind::Short => (i16::MIN as i64, i16::MAX as i64, "a short"),
        Kind::Byte => (i8::MIN as i64, i8::MAX as i64, "a byte"),
        _ => (i64::MIN, i64::MAX, "a long"),
    }
}

/// An integer field's value with the type's range check.
fn integral(k: Kind, v: &Value) -> EsResult<i64> {
    let (lo, hi, what) = int_bounds(k);
    let d = parse_double_value(v)?;
    if k != Kind::Long && (d < lo as f64 || d > hi as f64) {
        return Err(EsError::illegal_argument(format!("Value [{}] is out of range for {what}", value_text(v))));
    }
    let l = parse_long_value(v)?;
    if l < lo || l > hi {
        return Err(EsError::illegal_argument(format!("Value [{}] is out of range for {what}", value_text(v))));
    }
    Ok(l)
}

fn point_kind(k: Kind) -> (PointType, usize) {
    match k {
        Kind::Integer | Kind::Short | Kind::Byte | Kind::TokenCount => (PointType::Int, 4),
        Kind::Float => (PointType::Float, 4),
        Kind::HalfFloat => (PointType::HalfFloat, 2),
        Kind::Double => (PointType::Double, 8),
        _ => (PointType::Long, 8),
    }
}

/// Encodes a long-like sortable value of field kind `k` as its point bytes.
fn point_bytes(k: Kind, v: i64) -> Vec<u8> {
    match k {
        Kind::Integer | Kind::Short | Kind::Byte | Kind::TokenCount => encode::i32_bytes(v as i32).to_vec(),
        _ => encode::i64_bytes(v).to_vec(),
    }
}

/// `IndexOrDocValuesQuery(point range, doc-values range)` over long-encoded bounds.
fn long_range(f: &FieldInfo, k: Kind, lo: i64, hi: i64) -> EsResult<QueryRef> {
    if lo > hi {
        return Ok(match_none("empty range"));
    }
    let (pt, bpd) = point_kind(k);
    let point = || -> QueryRef {
        match k {
            Kind::Integer | Kind::Short | Kind::Byte | Kind::TokenCount => arc(PointRangeQuery::int(f.number, lo as i32, hi as i32)),
            _ => {
                let _ = (pt, bpd);
                arc(PointRangeQuery::long(f.number, lo, hi))
            }
        }
    };
    let dv = || -> QueryRef { arc(DocValuesRangeQuery { field: f.number, range: DvRange::Long { lower: lo, upper: hi } }) };
    Ok(match (f.index, f.doc_values) {
        (true, true) => arc(IndexOrDocValuesQuery { index: point(), dv: dv() }),
        (true, false) => point(),
        (false, true) => dv(),
        (false, false) => return Err(not_searchable(f)),
    })
}

fn double_range(f: &FieldInfo, lo: f64, hi: f64) -> EsResult<QueryRef> {
    if lo > hi || lo.is_nan() || hi.is_nan() {
        return Ok(match_none("empty range"));
    }
    let point = || -> QueryRef { arc(PointRangeQuery::double(f.number, lo, hi)) };
    let dv = || -> QueryRef { arc(DocValuesRangeQuery { field: f.number, range: DvRange::Long { lower: encode::f64_sortable(lo), upper: encode::f64_sortable(hi) } }) };
    Ok(match (f.index, f.doc_values) {
        (true, true) => arc(IndexOrDocValuesQuery { index: point(), dv: dv() }),
        (true, false) => point(),
        (false, true) => dv(),
        (false, false) => return Err(not_searchable(f)),
    })
}

fn float_range(f: &FieldInfo, lo: f32, hi: f32) -> EsResult<QueryRef> {
    if lo > hi || lo.is_nan() || hi.is_nan() {
        return Ok(match_none("empty range"));
    }
    let point = || -> QueryRef { arc(PointRangeQuery::float(f.number, lo, hi)) };
    let dv = || -> QueryRef {
        arc(DocValuesRangeQuery { field: f.number, range: DvRange::Long { lower: encode::f32_sortable(lo) as i64, upper: encode::f32_sortable(hi) as i64 } })
    };
    Ok(match (f.index, f.doc_values) {
        (true, true) => arc(IndexOrDocValuesQuery { index: point(), dv: dv() }),
        (true, false) => point(),
        (false, true) => dv(),
        (false, false) => return Err(not_searchable(f)),
    })
}

fn half_range(f: &FieldInfo, lo: f32, hi: f32) -> EsResult<QueryRef> {
    if lo > hi || lo.is_nan() || hi.is_nan() {
        return Ok(match_none("empty range"));
    }
    let point = || -> EsResult<QueryRef> {
        Ok(arc(PointRangeQuery::new(f.number, encode::f16_bytes(lo).to_vec(), encode::f16_bytes(hi).to_vec(), 2, PointType::HalfFloat).map_err(super::query::index_err)?))
    };
    let dv = || -> QueryRef {
        arc(DocValuesRangeQuery { field: f.number, range: DvRange::Long { lower: encode::f16_sortable(lo) as i64, upper: encode::f16_sortable(hi) as i64 } })
    };
    Ok(match (f.index, f.doc_values) {
        (true, true) => arc(IndexOrDocValuesQuery { index: point()?, dv: dv() }),
        (true, false) => point()?,
        (false, true) => dv(),
        (false, false) => return Err(not_searchable(f)),
    })
}

/// Rounds to the nearest half float and steps (`HalfFloatPoint.nextUp/nextDown`).
fn half_step(v: f32, up: bool) -> f32 {
    // Infinities step no further outward (Lucene's `nextUp(+inf)` / `nextDown(-inf)`).
    if v.is_nan() || (up && v == f32::INFINITY) || (!up && v == f32::NEG_INFINITY) {
        return v;
    }
    let bits = encode::f16_bits(v);
    let h = encode::f16_to_f32(bits);
    let s = encode::f16_sortable(h);
    let n = if up { s.saturating_add(1) } else { s.saturating_sub(1) };
    encode::sortable_f16(n)
}

fn half_round(v: f32) -> f32 {
    encode::f16_to_f32(encode::f16_bits(v))
}

/// `NumberFieldType.termQuery`.
pub fn numeric_term(_ctx: &QueryCtx, f: &FieldInfo, v: &Value) -> EsResult<QueryRef> {
    if !f.index && !f.doc_values {
        return Err(not_searchable(f));
    }
    let k = f.kind;
    let q = match k {
        Kind::Long | Kind::Integer | Kind::Short | Kind::Byte | Kind::TokenCount => {
            if has_decimal(v) {
                return Ok(match_none(&format!("Value [{}] has a decimal part", value_text(v))));
            }
            let l = integral(k, v)?;
            long_range(f, k, l, l)?
        }
        Kind::UnsignedLong => {
            let s = value_text(v);
            if has_decimal(v) {
                return Ok(match_none("decimal part"));
            }
            let u: u64 = match s.trim().parse::<u64>() {
                Ok(u) => u,
                Err(_) => match s.trim().parse::<f64>() {
                    Ok(d) if (0.0..1.8446744073709552e19).contains(&d) => d as u64,
                    Ok(_) => return Ok(match_none("out of range")),
                    Err(_) => return Err(EsError::number_format(&s)),
                },
            };
            let l = (u ^ (1 << 63)) as i64;
            long_range(f, Kind::Long, l, l)?
        }
        Kind::Double => {
            let d = parse_double_value(v)?;
            double_range(f, d, d)?
        }
        Kind::Float => {
            let d = parse_double_value(v)? as f32;
            float_range(f, d, d)?
        }
        Kind::HalfFloat => {
            let d = half_round(parse_double_value(v)? as f32);
            half_range(f, d, d)?
        }
        Kind::ScaledFloat(sf) => {
            let d = parse_double_value(v)?;
            let l = (d * sf).round() as i64;
            long_range(f, Kind::Long, l, l)?
        }
        _ => return Err(EsError::illegal_argument(format!("field [{}] is not numeric", f.name))),
    };
    Ok(q)
}

/// `terms` on numbers: a point set (indexed) or doc-values set.
pub fn numeric_terms(ctx: &QueryCtx, f: &FieldInfo, values: &[Value]) -> EsResult<QueryRef> {
    let k = f.kind;
    let mut pts: Vec<Vec<u8>> = Vec::new();
    let mut dvs: Vec<i64> = Vec::new();
    for v in values {
        match k {
            Kind::Long | Kind::Integer | Kind::Short | Kind::Byte | Kind::TokenCount => {
                if has_decimal(v) {
                    continue;
                }
                let l = integral(k, v)?;
                pts.push(point_bytes(k, l));
                dvs.push(l);
            }
            Kind::Date | Kind::DateNanos => {
                let l = date_value(ctx, f, v, None, None, false)?;
                pts.push(encode::i64_bytes(l).to_vec());
                dvs.push(l);
            }
            Kind::Double => {
                let d = parse_double_value(v)?;
                pts.push(encode::f64_bytes(d).to_vec());
                dvs.push(encode::f64_sortable(d));
            }
            Kind::Float => {
                let d = parse_double_value(v)? as f32;
                pts.push(encode::f32_bytes(d).to_vec());
                dvs.push(encode::f32_sortable(d) as i64);
            }
            Kind::HalfFloat => {
                let d = half_round(parse_double_value(v)? as f32);
                pts.push(encode::f16_bytes(d).to_vec());
                dvs.push(encode::f16_sortable(d) as i64);
            }
            Kind::ScaledFloat(sf) => {
                let l = (parse_double_value(v)? * sf).round() as i64;
                pts.push(encode::i64_bytes(l).to_vec());
                dvs.push(l);
            }
            Kind::UnsignedLong => {
                let s = value_text(v);
                if let Ok(u) = s.trim().parse::<u64>() {
                    let l = (u ^ (1 << 63)) as i64;
                    pts.push(encode::i64_bytes(l).to_vec());
                    dvs.push(l);
                }
            }
            _ => {}
        }
    }
    if pts.is_empty() {
        return Ok(match_none("no valid terms"));
    }
    let (pt, _) = point_kind(if k.is_date() || k == Kind::UnsignedLong || matches!(k, Kind::ScaledFloat(_)) { Kind::Long } else { k });
    if f.index {
        Ok(arc(ConstantScoreQuery::new(arc(PointInSetQuery::new(f.number, pts, pt)))))
    } else if f.doc_values {
        let mut b = search_index::query::BooleanQuery::new();
        for l in dvs {
            b = b.add(search_index::query::Occur::Should, arc(DocValuesRangeQuery { field: f.number, range: DvRange::Long { lower: l, upper: l } }));
        }
        Ok(arc(ConstantScoreQuery::new(arc(b))))
    } else {
        Err(not_searchable(f))
    }
}

/// The formatter of a range or term on a date field (`format` overrides the mapping's).
fn date_formatter(f: &FieldInfo, format: Option<&str>) -> EsResult<Arc<DateFormatter>> {
    match format {
        Some(fmt) => DateFormatter::new(fmt).map(Arc::new).map_err(EsError::illegal_argument),
        None => f.date_format.clone().ok_or_else(|| EsError::illegal_argument(format!("field [{}] has no date format", f.name))),
    }
}

/// A date value of a query in the field's resolution (millis or nanos), with date math.
pub fn date_value(ctx: &QueryCtx, f: &FieldInfo, v: &Value, format: Option<&str>, zone: Option<&Zone>, round_up: bool) -> EsResult<i64> {
    let fmt = date_formatter(f, format)?;
    let text = match v {
        Value::Str(s) => s.clone(),
        Value::Num(Number::Float(d)) if d.fract() == 0.0 => format!("{}", *d as i64),
        other => other.to_es_string(),
    };
    let inst = crate::time::parse_date_math(&text, ctx.now_ms, round_up, zone, &fmt).map_err(|e| e.to_es())?;
    if f.kind == Kind::DateNanos {
        crate::time::to_epoch_nanos(inst).map_err(EsError::illegal_argument)
    } else {
        Ok(inst.millis())
    }
}

/// `DateFieldType.termQuery`: the whole rounding range of the value (`2020-01-01||/d`).
pub fn date_term(ctx: &QueryCtx, f: &FieldInfo, v: &Value) -> EsResult<QueryRef> {
    let lo = date_value(ctx, f, v, None, None, false)?;
    let hi = date_value(ctx, f, v, None, None, true)?;
    long_range(f, Kind::Long, lo, hi)
}

/// `IpFieldType.termQuery`: an address or a CIDR prefix.
pub fn ip_term(_ctx: &QueryCtx, f: &FieldInfo, v: &Value) -> EsResult<QueryRef> {
    let s = value_text(v);
    let (lo, hi) = if s.contains('/') {
        crate::engine::ranges::cidr(&s)?
    } else {
        let b = crate::document::values::parse_ip(&s).ok_or_else(|| EsError::illegal_argument(format!("'{s}' is not an IP string literal.")))?;
        (b, b)
    };
    ip_range_q(f, lo, hi)
}

fn ip_range_q(f: &FieldInfo, lo: [u8; 16], hi: [u8; 16]) -> EsResult<QueryRef> {
    if lo > hi {
        return Ok(match_none("empty range"));
    }
    let point = || -> EsResult<QueryRef> { Ok(arc(PointRangeQuery::new(f.number, lo.to_vec(), hi.to_vec(), 16, PointType::InetAddress).map_err(super::query::index_err)?)) };
    let dv = || -> QueryRef {
        arc(DocValuesRangeQuery { field: f.number, range: DvRange::Bytes { lower: Some(lo.to_vec()), upper: Some(hi.to_vec()), include_lower: true, include_upper: true } })
    };
    Ok(match (f.index, f.doc_values) {
        (true, true) => arc(IndexOrDocValuesQuery { index: point()?, dv: dv() }),
        (true, false) => point()?,
        (false, true) => dv(),
        (false, false) => return Err(not_searchable(f)),
    })
}

fn ip_step(b: [u8; 16], up: bool) -> Option<[u8; 16]> {
    let mut b = b;
    for i in (0..16).rev() {
        if up {
            if b[i] == 0xff {
                b[i] = 0;
            } else {
                b[i] += 1;
                return Some(b);
            }
        } else if b[i] == 0 {
            b[i] = 0xff;
        } else {
            b[i] -= 1;
            return Some(b);
        }
    }
    None
}

/// `range` (`RangeQueryBuilder.doToQuery` → `MappedFieldType.rangeQuery`).
pub fn range_query(ctx: &QueryCtx, r: &Range) -> EsResult<QueryRef> {
    if r.field == "_seq_no" {
        ctx.check_seq_no_queryable()?;
    }
    let Some(f) = ctx.field(&r.field) else {
        return Ok(match_none(&format!("unmapped field [{}]", r.field)));
    };
    let from = r.from.as_ref().filter(|v| !v.is_null());
    let to = r.to.as_ref().filter(|v| !v.is_null());
    if let Some(rel) = r.relation
        && !f.kind.is_range()
        && rel != Relation::Intersects
        && !f.kind.is_date()
    {
        return Err(EsError::illegal_argument(format!(
            "[range] query does not support relation [{}] for field [{}]",
            relation_name(rel),
            f.name
        )));
    }
    let k = f.kind;
    match k {
        Kind::Long | Kind::Integer | Kind::Short | Kind::Byte | Kind::TokenCount => {
            let (min, max, _) = int_bounds(k);
            let mut l = min;
            let mut u = max;
            if let Some(v) = from {
                l = integral_clamped(k, v)?;
                let dec = has_decimal(v);
                if (!dec && !r.include_lower) || (dec && signum(v) > 0.0) {
                    if l == max {
                        return Ok(match_none("empty range"));
                    }
                    l += 1;
                }
            }
            if let Some(v) = to {
                u = integral_clamped(k, v)?;
                let dec = has_decimal(v);
                if (!dec && !r.include_upper) || (dec && signum(v) < 0.0) {
                    if u == min {
                        return Ok(match_none("empty range"));
                    }
                    u -= 1;
                }
            }
            long_range(&f, k, l, u)
        }
        Kind::UnsignedLong => {
            let parse = |v: &Value| -> EsResult<f64> { parse_double_value(v) };
            let mut lo: u64 = 0;
            let mut hi: u64 = u64::MAX;
            if let Some(v) = from {
                let d = parse(v)?;
                if d > 1.8446744073709552e19 {
                    return Ok(match_none("empty"));
                }
                if d >= 0.0 {
                    lo = value_text(v).trim().parse::<u64>().unwrap_or(d.ceil() as u64);
                    if !r.include_lower && d.fract() == 0.0 {
                        if lo == u64::MAX {
                            return Ok(match_none("empty"));
                        }
                        lo += 1;
                    }
                }
            }
            if let Some(v) = to {
                let d = parse(v)?;
                if d < 0.0 {
                    return Ok(match_none("empty"));
                }
                hi = value_text(v).trim().parse::<u64>().unwrap_or(d.floor() as u64);
                if !r.include_upper && d.fract() == 0.0 {
                    if hi == 0 {
                        return Ok(match_none("empty"));
                    }
                    hi -= 1;
                }
            }
            long_range(&f, Kind::Long, (lo ^ (1 << 63)) as i64, (hi ^ (1 << 63)) as i64)
        }
        Kind::Double => {
            let mut l = f64::NEG_INFINITY;
            let mut u = f64::INFINITY;
            if let Some(v) = from {
                l = parse_double_value(v)?;
                if !r.include_lower {
                    l = l.next_up();
                }
            }
            if let Some(v) = to {
                u = parse_double_value(v)?;
                if !r.include_upper {
                    u = u.next_down();
                }
            }
            double_range(&f, l, u)
        }
        Kind::Float => {
            let mut l = f32::NEG_INFINITY;
            let mut u = f32::INFINITY;
            if let Some(v) = from {
                l = parse_double_value(v)? as f32;
                if !r.include_lower {
                    l = l.next_up();
                }
            }
            if let Some(v) = to {
                u = parse_double_value(v)? as f32;
                if !r.include_upper {
                    u = u.next_down();
                }
            }
            float_range(&f, l, u)
        }
        Kind::HalfFloat => {
            let mut l = f32::NEG_INFINITY;
            let mut u = f32::INFINITY;
            if let Some(v) = from {
                let x = parse_double_value(v)? as f32;
                l = if r.include_lower { half_step(x, true).min(half_round(x)).max(half_round(x)) } else { half_step(x, true) };
                if r.include_lower {
                    // `HalfFloatPoint.nextUp(nextDown(value))`: the smallest half >= value.
                    l = half_step(half_step(x, false), true);
                }
            }
            if let Some(v) = to {
                let x = parse_double_value(v)? as f32;
                u = if r.include_upper { half_step(half_step(x, true), false) } else { half_step(x, false) };
            }
            half_range(&f, l, u)
        }
        Kind::ScaledFloat(sf) => {
            let mut lo = i64::MIN;
            let mut hi = i64::MAX;
            if let Some(v) = from {
                let mut d = parse_double_value(v)? * sf;
                if !r.include_lower {
                    d = d.next_up();
                }
                lo = d.ceil().round() as i64;
            }
            if let Some(v) = to {
                let mut d = parse_double_value(v)? * sf;
                if !r.include_upper {
                    d = d.next_down();
                }
                hi = d.floor().round() as i64;
            }
            long_range(&f, Kind::Long, lo, hi)
        }
        Kind::Date | Kind::DateNanos => {
            let zone = match &r.time_zone {
                Some(z) => Some(Zone::parse(z).map_err(EsError::illegal_argument)?),
                None => None,
            };
            let fmt = r.format.as_deref();
            let mut l = i64::MIN;
            let mut u = i64::MAX;
            if let Some(v) = from {
                l = date_value(ctx, &f, v, fmt, zone.as_ref(), !r.include_lower)?;
                if !r.include_lower {
                    if l == i64::MAX {
                        return Ok(match_none("empty"));
                    }
                    l += 1;
                }
            }
            if let Some(v) = to {
                u = date_value(ctx, &f, v, fmt, zone.as_ref(), r.include_upper)?;
                if !r.include_upper {
                    if u == i64::MIN {
                        return Ok(match_none("empty"));
                    }
                    u -= 1;
                }
            }
            long_range(&f, Kind::Long, l, u)
        }
        Kind::Ip => {
            let parse = |v: &Value| -> EsResult<[u8; 16]> {
                let s = value_text(v);
                crate::document::values::parse_ip(&s).ok_or_else(|| EsError::illegal_argument(format!("'{s}' is not an IP string literal.")))
            };
            let mut lo = [0u8; 16];
            let mut hi = [0xffu8; 16];
            if let Some(v) = from {
                lo = parse(v)?;
                if !r.include_lower {
                    match ip_step(lo, true) {
                        Some(x) => lo = x,
                        None => return Ok(match_none("empty")),
                    }
                }
            }
            if let Some(v) = to {
                hi = parse(v)?;
                if !r.include_upper {
                    match ip_step(hi, false) {
                        Some(x) => hi = x,
                        None => return Ok(match_none("empty")),
                    }
                }
            }
            ip_range_q(&f, lo, hi)
        }
        Kind::Keyword | Kind::Version | Kind::Text | Kind::MatchOnlyText | Kind::SearchAsYouType | Kind::Flattened | Kind::CountedKeyword | Kind::Wildcard => {
            if !ctx.allow_expensive {
                return Err(EsError::new(
                    400,
                    "elasticsearch_exception",
                    "[range] queries on [text] or [keyword] fields cannot be executed when 'search.allow_expensive_queries' is set to false.",
                ));
            }
            let b = |v: &Value| -> Vec<u8> {
                let s = value_text(v);
                if f.kind.is_text() { s.into_bytes() } else { normalize(&f, &s).into_bytes() }
            };
            let mut lo = from.map(b);
            let mut hi = to.map(b);
            let (mut include_lower, mut include_upper) = (r.include_lower, r.include_upper);
            // A keyed flattened field's open bounds stop at its key's terms.
            if let Some(k) = &f.flattened_key {
                if lo.is_none() {
                    lo = Some(format!("{k}\0").into_bytes());
                    include_lower = true;
                }
                if hi.is_none() {
                    hi = Some(format!("{k}\u{1}").into_bytes());
                    include_upper = false;
                }
            }
            let r = crate::query::term_level::Range { include_lower, include_upper, ..r.clone() };
            if f.index {
                Ok(arc(ConstantScoreQuery::new(arc(MultiTermQuery::range(f.number, lo, hi, r.include_lower, r.include_upper)))))
            } else if f.doc_values && !f.kind.is_text() {
                // Text doc values (`doc_values: true`) serve multi-term queries, not ranges.
                Ok(arc(ConstantScoreQuery::new(arc(DocValuesRangeQuery {
                    field: f.number,
                    range: DvRange::Bytes { lower: lo, upper: hi, include_lower: r.include_lower, include_upper: r.include_upper },
                }))))
            } else if f.kind.is_text() {
                Err(super::query::not_searchable(&f))
            } else {
                Err(not_searchable(&f))
            }
        }
        Kind::ConstantKeyword => {
            let Some(c) = &f.constant_value else { return Ok(match_none("no value")) };
            let c = c.as_bytes();
            let ok_lo = from.is_none_or(|v| {
                let b = value_text(v);
                if r.include_lower { c >= b.as_bytes() } else { c > b.as_bytes() }
            });
            let ok_hi = to.is_none_or(|v| {
                let b = value_text(v);
                if r.include_upper { c <= b.as_bytes() } else { c < b.as_bytes() }
            });
            Ok(if ok_lo && ok_hi { arc(search_index::query::MatchAllQuery) } else { match_none("constant out of range") })
        }
        k if k.is_range() => range_field_query(ctx, &f, from, to, r.include_lower, r.include_upper, r.relation.unwrap_or(Relation::Intersects), r.format.as_deref(), r.time_zone.as_deref()),
        Kind::Boolean => {
            let b = |v: &Value| -> EsResult<i64> {
                match value_text(v).as_str() {
                    "true" => Ok(1),
                    "false" => Ok(0),
                    s => Err(EsError::illegal_argument(format!("Can't parse boolean value [{s}], expected [true] or [false]"))),
                }
            };
            let mut lo = 0;
            let mut hi = 1;
            if let Some(v) = from {
                lo = b(v)? + if r.include_lower { 0 } else { 1 };
            }
            if let Some(v) = to {
                hi = b(v)? - if r.include_upper { 0 } else { 1 };
            }
            if lo > hi {
                return Ok(match_none("empty"));
            }
            Ok(arc(ConstantScoreQuery::new(arc(DocValuesRangeQuery { field: f.number, range: DvRange::Long { lower: lo, upper: hi } }))))
        }
        _ => Err(EsError::illegal_argument(format!("Field [{}] of type [{}] does not support range queries", f.name, f.ty))),
    }
}

fn integral_clamped(k: Kind, v: &Value) -> EsResult<i64> {
    let (lo, hi, what) = int_bounds(k);
    let d = parse_double_value(v)?;
    if d < lo as f64 || d > hi as f64 {
        if k == Kind::Long {
            return Err(EsError::illegal_argument(format!("Value [{}] is out of range for a long", value_text(v))));
        }
        // Out-of-range bounds clamp (`IntegerType.rangeQuery` rounds to the type's range).
        let _ = what;
        return Ok(if d < 0.0 { lo } else { hi });
    }
    parse_long_value(v)
}

fn relation_name(r: Relation) -> &'static str {
    match r {
        Relation::Intersects => "intersects",
        Relation::Disjoint => "disjoint",
        Relation::Within => "within",
        Relation::Contains => "contains",
    }
}

/// A term query on a range field: the ranges containing the value.
pub fn range_field_term(ctx: &QueryCtx, f: &FieldInfo, v: &Value) -> EsResult<QueryRef> {
    range_field_query(ctx, f, Some(v), Some(v), true, true, Relation::Intersects, None, None)
}

/// Range fields: the query range against each doc range by `relation`, as a 2-d box over the
/// doc's `(min, max)` point.
#[allow(clippy::too_many_arguments)]
pub fn range_field_query(ctx: &QueryCtx, f: &FieldInfo, from: Option<&Value>, to: Option<&Value>, il: bool, iu: bool, rel: Relation, format: Option<&str>, tz: Option<&str>) -> EsResult<QueryRef> {
    let k = f.kind;
    let enc_long = |v: i64| -> Vec<u8> { if k == Kind::IntegerRange { encode::i32_bytes(v as i32).to_vec() } else { encode::i64_bytes(v).to_vec() } };
    let (qmin, qmax, low, high, bpd, pt): (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, usize, PointType) = match k {
        Kind::IntegerRange | Kind::LongRange | Kind::DateRange => {
            let (minv, maxv) = if k == Kind::IntegerRange { (i32::MIN as i64, i32::MAX as i64) } else { (i64::MIN, i64::MAX) };
            let zone = match tz {
                Some(z) => Some(Zone::parse(z).map_err(EsError::illegal_argument)?),
                None => None,
            };
            let parse = |v: &Value, up: bool| -> EsResult<i64> {
                if k == Kind::DateRange {
                    let fmt = date_formatter(f, format)?;
                    let inst = crate::time::parse_date_math(&value_text(v), ctx.now_ms, up, zone.as_ref(), &fmt).map_err(|e| e.to_es())?;
                    Ok(inst.millis())
                } else {
                    parse_long_value(v)
                }
            };
            let lo = match from {
                Some(v) => parse(v, !il)? + if il { 0 } else { 1 },
                None => minv,
            };
            let hi = match to {
                Some(v) => parse(v, iu)? - if iu { 0 } else { 1 },
                None => maxv,
            };
            (enc_long(lo), enc_long(hi), enc_long(minv), enc_long(maxv), if k == Kind::IntegerRange { 4 } else { 8 }, if k == Kind::IntegerRange { PointType::Int } else { PointType::Long })
        }
        Kind::FloatRange => {
            let lo = from.map(parse_double_value).transpose()?.map(|d| d as f32).map(|d| if il { d } else { d.next_up() }).unwrap_or(f32::NEG_INFINITY);
            let hi = to.map(parse_double_value).transpose()?.map(|d| d as f32).map(|d| if iu { d } else { d.next_down() }).unwrap_or(f32::INFINITY);
            (encode::f32_bytes(lo).to_vec(), encode::f32_bytes(hi).to_vec(), encode::f32_bytes(f32::NEG_INFINITY).to_vec(), encode::f32_bytes(f32::INFINITY).to_vec(), 4, PointType::Float)
        }
        Kind::DoubleRange => {
            let lo = from.map(parse_double_value).transpose()?.map(|d| if il { d } else { d.next_up() }).unwrap_or(f64::NEG_INFINITY);
            let hi = to.map(parse_double_value).transpose()?.map(|d| if iu { d } else { d.next_down() }).unwrap_or(f64::INFINITY);
            (encode::f64_bytes(lo).to_vec(), encode::f64_bytes(hi).to_vec(), encode::f64_bytes(f64::NEG_INFINITY).to_vec(), encode::f64_bytes(f64::INFINITY).to_vec(), 8, PointType::Double)
        }
        Kind::IpRange => {
            let parse = |v: &Value| -> EsResult<[u8; 16]> {
                let s = value_text(v);
                if s.contains('/') {
                    return Ok(crate::engine::ranges::cidr(&s)?.0);
                }
                crate::document::values::parse_ip(&s).ok_or_else(|| EsError::illegal_argument(format!("'{s}' is not an IP string literal.")))
            };
            let lo = match from {
                Some(v) => {
                    let b = parse(v)?;
                    if il { b } else { ip_step(b, true).unwrap_or(b) }
                }
                None => [0; 16],
            };
            let hi = match to {
                Some(v) => {
                    let s = value_text(v);
                    let b = if s.contains('/') { crate::engine::ranges::cidr(&s)?.1 } else { parse(v)? };
                    if iu { b } else { ip_step(b, false).unwrap_or(b) }
                }
                None => [0xff; 16],
            };
            (lo.to_vec(), hi.to_vec(), vec![0; 16], vec![0xff; 16], 16, PointType::InetAddress)
        }
        _ => return Err(EsError::illegal_argument("not a range field")),
    };
    if qmin > qmax && bpd != 16 {
        // Compare decoded order: packed sortable bytes compare like values.
        return Ok(match_none("empty range"));
    }
    let cat = |a: &[u8], b: &[u8]| -> Vec<u8> { [a, b].concat() };
    let (lower, upper) = match rel {
        // doc.min <= qmax && doc.max >= qmin
        Relation::Intersects => (cat(&low, &qmin), cat(&qmax, &high)),
        // qmin <= doc.min && doc.max <= qmax
        Relation::Within => (cat(&qmin, &low), cat(&high, &qmax)),
        // doc.min <= qmin && doc.max >= qmax
        Relation::Contains => (cat(&low, &qmax), cat(&qmin, &high)),
        Relation::Disjoint => return Err(EsError::illegal_argument("[range] query does not support relation [disjoint]")),
    };
    if !f.index {
        return Err(EsError::illegal_argument(format!("Cannot search on field [{}] since it is not indexed.", f.name)));
    }
    Ok(arc(ConstantScoreQuery::new(arc(PointRangeQuery::new(f.number, lower, upper, bpd, pt).map_err(super::query::index_err)?))))
}

/// `fuzzy` on numbers and dates: the range `value ± fuzziness`.
pub fn numeric_fuzzy(ctx: &QueryCtx, f: &FieldInfo, v: &Value, z: &Fuzziness) -> EsResult<QueryRef> {
    let _ = ctx;
    let d = parse_double_value(v)?;
    let delta = match z {
        Fuzziness::Edits(n) => *n as f64,
        Fuzziness::Auto { .. } => 1.0,
    };
    let r = Range {
        field: f.name.clone(),
        from: Some(Value::Num(Number::Float(d - delta))),
        to: Some(Value::Num(Number::Float(d + delta))),
        include_lower: true,
        include_upper: true,
        time_zone: None,
        format: None,
        relation: None,
        common: Default::default(),
    };
    range_query(ctx, &r)
}
