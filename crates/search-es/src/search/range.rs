//! Numeric, date, IP and range-field term and range queries with Elasticsearch's value rules:
//! `NumberFieldMapper.NumberType` parsing (decimal parts, ranges, `coerce`), date math with
//! `format` / `time_zone` and rounding, `InetAddressPoint` ranges and CIDR terms, and the
//! `RangeFieldMapper` relations (`intersects`, `within`, `contains`) on range fields' points.
//! Indexed fields with doc values get `IndexOrDocValuesQuery`, as `NumberFieldType` builds.

use super::query::{QueryCtx, arc, match_none, normalize, value_text};
use crate::engine::fields::{FieldInfo, Kind};
use crate::error::{EsError, EsResult};
use crate::json::{Number, Value};
use crate::query::term_level::{Range, Relation};
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
pub(crate) fn parse_double_value(v: &Value) -> EsResult<f64> {
    match v {
        Value::Num(n) => Ok(n.as_f64()),
        Value::Str(s) => crate::units::parse_java_double(s.trim()).ok_or_else(|| EsError::number_format(s)),
        other => Err(EsError::number_format(&other.to_es_string())),
    }
}

pub(crate) fn has_decimal(v: &Value) -> bool {
    match v {
        Value::Num(Number::Float(f)) => f.fract() != 0.0,
        Value::Str(s) => s.trim().parse::<f64>().map(|f| f.fract() != 0.0).unwrap_or(false),
        _ => false,
    }
}

pub(crate) fn signum(v: &Value) -> f64 {
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

/// `HalfFloatPoint.nextUp/nextDown`: the first half float above (below) `v`. A `v` between
/// two half floats has its neighbour on that side as the answer, whichever is nearer.
fn half_step(v: f32, up: bool) -> f32 {
    // Infinities step no further outward (Lucene's `nextUp(+inf)` / `nextDown(-inf)`).
    if v.is_nan() || (up && v == f32::INFINITY) || (!up && v == f32::NEG_INFINITY) {
        return v;
    }
    let bits = encode::f16_bits(v);
    let h = encode::f16_to_f32(bits);
    if (up && h > v) || (!up && h < v) {
        return h;
    }
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
            index_sorted_exact(f, l, long_range(f, k, l, l)?)
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
            // `UnsignedLongFieldType.termQuery`: the point query alone when indexed.
            if f.index && !crate::opensearch::active() { arc(PointRangeQuery::long(f.number, l, l)) } else { long_range(f, Kind::Long, l, l)? }
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
            index_sorted_exact(f, l, long_range(f, Kind::Long, l, l)?)
        }
        _ => return Err(EsError::illegal_argument(format!("field [{}] is not numeric", f.name))),
    };
    Ok(q)
}

/// Lucene's `LongField.newExactQuery` / `IntField.newExactQuery`, which Elasticsearch's
/// integral `term` queries on indexed fields with doc values are: the point-or-doc-values
/// query inside `IndexSortSortedNumericDocValuesRangeQuery` (a doc range on segments sorted
/// by the field; it is what explanations and profiles show, `f:[5 TO 5]`).
fn index_sorted_exact(f: &FieldInfo, v: i64, q: QueryRef) -> QueryRef {
    if !(f.index && f.doc_values) || !q.as_any().is::<IndexOrDocValuesQuery>() || crate::opensearch::active() {
        return q;
    }
    arc(search_index::query::IndexSortRangeQuery { field: f.number, lower: v, upper: v, fallback: q })
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
        // `PointInSetQuery` scores constantly itself (explanations read `f:{1 2}`).
        Ok(arc(PointInSetQuery::new(f.number, pts, pt)))
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

/// The smallest and largest value a shard indexes for a date field (Lucene's
/// `PointValues.getMinPackedValue` / `getMaxPackedValue` over every segment); `None`
/// without points.
fn date_point_bounds(ctx: &QueryCtx, field: u32) -> Option<(i64, i64)> {
    let mut out: Option<(i64, i64)> = None;
    for seg in ctx.searcher.segments() {
        let Some(p) = seg.reader.points(field) else { continue };
        let (lo, hi) = (encode::i64_from(p.min_packed_value()), encode::i64_from(p.max_packed_value()));
        out = Some(match out {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        });
    }
    out
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

pub(crate) fn ip_step(b: [u8; 16], up: bool) -> Option<[u8; 16]> {
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
        // `RangeQueryBuilder.doRewrite`: `DISJOINT` on a shard without the field.
        return Ok(match_none("The \"range\" query was rewritten to a \"match_none\" query."));
    };
    let from = r.from.as_ref().filter(|v| !v.is_null());
    let to = r.to.as_ref().filter(|v| !v.is_null());
    // `RangeQueryBuilder.doToQuery`: open on both sides it is an `exists` query (whatever the
    // field type, relation or format), which Lucene rewrites to `*:*` when every doc has it.
    if from.is_none() && to.is_none() && r.field != "_seq_no" {
        return ctx.exists_query(&r.field);
    }
    // `AggregateMetricDoubleFieldType.rangeQuery`: the single metric's numeric range, the
    // average of `sum` and `value_count`, or nothing.
    let f = if f.kind == Kind::AggregateMetric {
        match super::aggregate_metric::delegate(&f) {
            super::aggregate_metric::Delegate::Single(m) => std::sync::Arc::new(*m),
            super::aggregate_metric::Delegate::Avg { sum, count } => return super::aggregate_metric::avg_range(&f, sum, count, from, to, r.include_lower, r.include_upper),
            super::aggregate_metric::Delegate::None => return Ok(match_none("aggregate_metric_double without a single metric or [sum, value_count]")),
        }
    } else {
        f
    };
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
            // `SHORT` and `BYTE` ranges are `INTEGER.rangeQuery`: integer bounds (an open
            // range prints `[83 TO 2147483647]`).
            let rk = if matches!(k, Kind::Short | Kind::Byte) { Kind::Integer } else { k };
            let (min, max, _) = int_bounds(rk);
            let mut l = min;
            let mut u = max;
            if let Some(v) = from {
                l = integral_clamped(rk, v)?;
                let dec = has_decimal(v);
                if (!dec && !r.include_lower) || (dec && signum(v) > 0.0) {
                    if l == max {
                        return Ok(match_none("empty range"));
                    }
                    l += 1;
                }
            }
            if let Some(v) = to {
                u = integral_clamped(rk, v)?;
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
                    // The exact decimal rounded up (`BigDecimal`: a double loses digits here).
                    let (exact, fraction) = match decimal_parts(&value_text(v)) {
                        Some((i, true)) => match i.checked_add(1) {
                            Some(c) => (c, true),
                            None => return Ok(match_none("empty")),
                        },
                        Some((i, false)) => (i, false),
                        None => (d.ceil() as u64, d.fract() != 0.0),
                    };
                    lo = exact;
                    if !r.include_lower && !fraction {
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
                // The exact decimal rounded down.
                let (exact, fraction) = match decimal_parts(&value_text(v)) {
                    Some((i, f)) => (i, f),
                    None => (d.floor() as u64, d.fract() != 0.0),
                };
                hi = exact;
                if !r.include_upper && !fraction {
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
            // `RangeQueryBuilder.doRewrite` with `DateFieldType.isFieldWithinQuery`: the shard's
            // smallest and largest indexed values (deleted docs' too) decide first — no
            // values or none in range match nothing, all of them in range is the field's
            // `exists` query.
            if f.index && !crate::opensearch::active() {
                let rewritten = || match_none("The \"range\" query was rewritten to a \"match_none\" query.");
                match date_point_bounds(ctx, f.number) {
                    None => return Ok(rewritten()),
                    Some((min, max)) if max < l || min > u => return Ok(rewritten()),
                    Some((min, max)) if min >= l && max <= u => return ctx.exists_query(&f.name),
                    Some(_) => {}
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
        // `VersionStringFieldType.rangeQuery`: a term range over the encoded bounds (no
        // expensive-query check, unlike keyword ranges).
        Kind::Version => {
            let b = |v: &Value| super::version_field::encode(&value_text(v));
            let lo = from.map(b).transpose()?;
            let hi = to.map(b).transpose()?;
            Ok(arc(MultiTermQuery::range(f.number, lo, hi, r.include_lower, r.include_upper)))
        }
        Kind::Keyword | Kind::Text | Kind::MatchOnlyText | Kind::SearchAsYouType | Kind::Flattened | Kind::CountedKeyword | Kind::Wildcard => {
            if !ctx.allow_expensive {
                return Err(EsError::new(
                    400,
                    "exception",
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
            if f.index && f.kind == Kind::Wildcard {
                let q = arc(ConstantScoreQuery::new(arc(MultiTermQuery::range(f.number, lo.clone(), hi.clone(), r.include_lower, r.include_upper))));
                Ok(super::extra::wildcard_range(&f, q, lo.as_deref(), hi.as_deref(), r.include_lower, r.include_upper))
            } else if f.index {
                // Lucene's `TermRangeQuery` scores constantly by its rewrite and prints (and
                // explains) as itself, without a `ConstantScore(…)` around it.
                Ok(arc(MultiTermQuery::range(f.number, lo, hi, r.include_lower, r.include_upper)))
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
            // Without doc values, the indexed `F` / `T` terms (`TermRangeQuery` over them).
            if !f.doc_values && f.index {
                let terms = (lo..=hi).map(|v| if v == 1 { b"T".to_vec() } else { b"F".to_vec() });
                return Ok(arc(ConstantScoreQuery::new(arc(search_index::query::MultiTermQuery::set(f.number, terms)))));
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

/// A plain decimal's integer part and whether its fraction is non-zero (`"18446744073709551614.5"`
/// → `(18446744073709551614, true)`), exactly; `None` for anything else (signs, exponents,
/// values past `u64`), which goes through a double.
fn decimal_parts(text: &str) -> Option<(u64, bool)> {
    let t = text.trim();
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((int.parse::<u64>().ok()?, frac.bytes().any(|b| b != b'0')))
}
