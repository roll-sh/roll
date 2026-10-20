//! Binary wire format (send/recv) for the types drivers commonly request in binary.

use super::array::ArrayValue;
use super::json::parse_jsonb;
use super::numeric::Numeric;
use super::text::{FormatCtx, format_value, parse_value};
use super::value::Interval;
use super::{DataType, Value};
use crate::error::{Error, Result, code};
use crate::{bail, err};
use bytes::{Buf, BufMut, BytesMut};
use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};
use std::sync::Arc;

/// Whether `ty` has a binary representation we implement.
pub fn supports_binary(ty: &DataType) -> bool {
    use DataType as T;
    match ty {
        T::Array(e) => supports_binary(e),
        T::Range(rt) | T::Multirange(rt) => supports_binary(&rt.subtype),
        T::Point | T::Lseg | T::Path | T::GBox | T::Polygon | T::Line | T::Circle => true,
        T::Bool
        | T::Int2
        | T::Int4
        | T::Int8
        | T::Float4
        | T::Float8
        | T::Numeric(_)
        | T::Text
        | T::Varchar(_)
        | T::Bpchar(_)
        | T::Name
        | T::Char
        | T::Bytea
        | T::Date
        | T::Time(_)
        | T::Timestamp(_)
        | T::Timestamptz(_)
        | T::Interval(_)
        | T::Uuid
        | T::Json
        | T::Jsonb
        | T::JsonPath
        | T::Oid
        | T::Regclass
        | T::Regtype
        | T::Regproc
        | T::Regconfig
        | T::Regdictionary
        | T::TsVector
        | T::TsQuery
        | T::Unknown
        | T::Xid8
        | T::PgSnapshot
        | T::TxidSnapshot => true,
        // Extended statistics send their bytes (bytea's send).
        T::PgNdistinct | T::PgDependencies | T::PgMcvList => true,
        _ => false,
    }
}

pub fn encode(v: &Value, ty: &DataType, ctx: &FormatCtx, out: &mut BytesMut) -> Result<()> {
    use DataType as T;
    match (v, ty) {
        (Value::Bool(b), _) => out.put_u8(*b as u8),
        (Value::Int2(i), _) => out.put_i16(*i),
        (Value::Int4(i), _) => out.put_i32(*i),
        (Value::Int8(i), _) => out.put_i64(*i),
        (Value::Oid(o), _) => out.put_u32(*o),
        (Value::Xid8(x), _) => out.put_u64(*x),
        // pg_snapshot_send: the number of running transactions, xmin, xmax, then those.
        (Value::Snapshot(s), _) => {
            out.put_i32(s.xip.len() as i32);
            out.put_u64(s.xmin);
            out.put_u64(s.xmax);
            for x in &s.xip {
                out.put_u64(*x);
            }
        }
        (Value::Float4(f), _) => out.put_f32(*f),
        (Value::Float8(f), _) => out.put_f64(*f),
        (Value::Numeric(n), _) => encode_numeric(n, out),
        // jsonpath_send: a version byte, then the text.
        (Value::Text(s), T::JsonPath) => {
            out.put_u8(1);
            out.put_slice(s.as_bytes());
        }
        // xml_send: the text with its declaration normalized (xml_out_internal).
        (Value::Text(s), T::Xml) => out.put_slice(super::xml::xml_out(s).as_bytes()),
        (Value::Text(s), _) => out.put_slice(s.as_bytes()),
        (Value::Json(s), _) => out.put_slice(s.as_bytes()),
        (Value::Jsonb(j), _) => {
            out.put_u8(1);
            out.put_slice(j.to_string().as_bytes());
        }
        (Value::Char(c), _) => out.put_u8(*c),
        (Value::Bytea(b), _) => out.put_slice(b),
        (Value::Date(d), _) => out.put_i32(*d),
        (Value::Time(t), _) => out.put_i64(*t),
        (Value::Timestamp(t), _) | (Value::Timestamptz(t), _) => out.put_i64(*t),
        (Value::Interval(i), _) => {
            out.put_i64(i.micros);
            out.put_i32(i.days);
            out.put_i32(i.months);
        }
        (Value::Uuid(u), _) => out.put_u128(*u),
        (Value::TsVector(v), _) => {
            let mut b = Vec::new();
            super::tsearch::tsvector_send(v, &mut b);
            out.put_slice(&b);
        }
        (Value::TsQuery(q), _) => {
            let mut b = Vec::new();
            super::tsearch::tsquery_send(q, &mut b);
            out.put_slice(&b);
        }
        (Value::Array(a), T::Array(_)) => encode_array(a, ctx, out)?,
        // path_send: a closed flag and a count first; polygon_send: a count.
        (Value::Geo(g), _) => {
            use super::geo::Geo;
            match &**g {
                Geo::Path(p) => {
                    out.put_u8(p.closed as u8);
                    out.put_i32(p.p.len() as i32);
                }
                Geo::Polygon(p) => out.put_i32(p.p.len() as i32),
                _ => {}
            }
            for f in g.floats() {
                out.put_f64(f);
            }
        }
        (Value::Range(r), T::Range(rt)) => encode_range(r, &rt.subtype, ctx, out)?,
        // multirange_send: the range count, then (length, range_send) per range.
        (Value::Multirange(m), T::Multirange(rt)) => {
            out.put_i32(m.len() as i32);
            for r in m.iter() {
                let mut b = BytesMut::new();
                encode_range(r, &rt.subtype, ctx, &mut b)?;
                out.put_i32(b.len() as i32);
                out.put_slice(&b);
            }
        }
        // record_send: the field count, then (type oid, length or -1, bytes) per field.
        (Value::Record(items), T::Row(_) | T::Record) => {
            out.put_i32(items.len() as i32);
            for (i, v) in items.iter().enumerate() {
                let ft = match ty {
                    T::Row(f) if f.len() == items.len() => f[i].1.clone(),
                    _ => super::cast::value_type(v),
                };
                out.put_u32(ft.oid());
                if v.is_null() {
                    out.put_i32(-1);
                    continue;
                }
                let mut b = BytesMut::new();
                encode(v, &ft, ctx, &mut b)?;
                out.put_i32(b.len() as i32);
                out.put_slice(&b);
            }
        }
        _ => {
            // Fall back to the text form (valid for text-like types).
            out.put_slice(format_value(v, ty, ctx).as_bytes());
        }
    }
    Ok(())
}

// range_send flags.
const RANGE_EMPTY: u8 = 0x01;
const RANGE_LB_INC: u8 = 0x02;
const RANGE_UB_INC: u8 = 0x04;
const RANGE_LB_INF: u8 = 0x08;
const RANGE_UB_INF: u8 = 0x10;

/// range_send: a flags byte, then (length, bytes) per finite bound.
fn encode_range(
    r: &super::range::Range,
    sub: &DataType,
    ctx: &FormatCtx,
    out: &mut BytesMut,
) -> Result<()> {
    if r.empty {
        out.put_u8(RANGE_EMPTY);
        return Ok(());
    }
    let mut flags = 0;
    if r.lower_inc {
        flags |= RANGE_LB_INC;
    }
    if r.upper_inc {
        flags |= RANGE_UB_INC;
    }
    if r.lower.is_none() {
        flags |= RANGE_LB_INF;
    }
    if r.upper.is_none() {
        flags |= RANGE_UB_INF;
    }
    out.put_u8(flags);
    for v in [&r.lower, &r.upper].into_iter().flatten() {
        let mut b = BytesMut::new();
        encode(v, sub, ctx, &mut b)?;
        out.put_i32(b.len() as i32);
        out.put_slice(&b);
    }
    Ok(())
}

/// range_recv.
fn decode_range(
    mut b: &[u8],
    rt: &Arc<super::range::RangeType>,
    ctx: &FormatCtx,
) -> Result<super::range::Range> {
    use super::range::{Bound, Range, make};
    if b.is_empty() {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "insufficient data left in message"
        );
    }
    let flags = b.get_u8();
    if flags & RANGE_EMPTY != 0 {
        if !b.is_empty() {
            bail!(
                INVALID_BINARY_REPRESENTATION,
                "incorrect binary data format"
            );
        }
        return Ok(Range::empty());
    }
    let mut bound = |inf: bool| -> Result<Option<Value>> {
        if inf {
            return Ok(None);
        }
        if b.len() < 4 {
            bail!(
                INVALID_BINARY_REPRESENTATION,
                "insufficient data left in message"
            );
        }
        let len = b.get_i32();
        if len < 0 || b.len() < len as usize {
            bail!(
                INVALID_BINARY_REPRESENTATION,
                "insufficient data left in message"
            );
        }
        let v = decode(&b[..len as usize], &rt.subtype, ctx)?;
        b.advance(len as usize);
        Ok(Some(v))
    };
    let lower = bound(flags & RANGE_LB_INF != 0)?;
    let upper = bound(flags & RANGE_UB_INF != 0)?;
    if !b.is_empty() {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "incorrect binary data format"
        );
    }
    let li = flags & RANGE_LB_INC != 0 && lower.is_some();
    let ui = flags & RANGE_UB_INC != 0 && upper.is_some();
    make(
        rt,
        Bound {
            val: lower,
            inclusive: li,
            lower: true,
        },
        Bound {
            val: upper,
            inclusive: ui,
            lower: false,
        },
    )
}

/// point_recv & co.
fn decode_geo(mut b: &[u8], ty: &DataType) -> Result<Value> {
    use super::geo::{self, Geo, Line, Point};
    use DataType as T;
    let short = || err!(PROTOCOL_VIOLATION, "insufficient data left in message");
    let floats = |b: &mut &[u8], n: usize| -> Result<Vec<f64>> {
        if b.len() < n * 8 {
            return Err(short());
        }
        Ok((0..n).map(|_| b.get_f64()).collect())
    };
    let pts = |f: Vec<f64>| {
        f.as_chunks::<2>()
            .0
            .iter()
            .map(|c| Point { x: c[0], y: c[1] })
            .collect::<Vec<_>>()
    };
    let g = match ty {
        T::Point => {
            let f = floats(&mut b, 2)?;
            Geo::Point(Point { x: f[0], y: f[1] })
        }
        T::Lseg => {
            let p = pts(floats(&mut b, 4)?);
            Geo::Lseg(geo::Lseg { p: [p[0], p[1]] })
        }
        T::GBox => {
            let p = pts(floats(&mut b, 4)?);
            Geo::Box(geo::box_construct(&p[0], &p[1]))
        }
        T::Line => {
            let f = floats(&mut b, 3)?;
            if geo::fp_zero(f[0]) && geo::fp_zero(f[1]) {
                bail!(
                    INVALID_PARAMETER_VALUE,
                    "invalid line specification: A and B cannot both be zero"
                );
            }
            Geo::Line(Line {
                a: f[0],
                b: f[1],
                c: f[2],
            })
        }
        T::Circle => {
            let f = floats(&mut b, 3)?;
            if f[2] < 0.0 {
                bail!(
                    INVALID_BINARY_REPRESENTATION,
                    "invalid radius in external \"circle\" value"
                );
            }
            Geo::Circle(geo::Circle {
                center: Point { x: f[0], y: f[1] },
                radius: f[2],
            })
        }
        T::Path | T::Polygon => {
            let closed = if matches!(ty, T::Path) {
                if b.is_empty() {
                    return Err(short());
                }
                b.get_u8() != 0
            } else {
                true
            };
            if b.len() < 4 {
                return Err(short());
            }
            let n = b.get_i32();
            if n <= 0 || n as usize > b.len() / 16 {
                bail!(
                    INVALID_BINARY_REPRESENTATION,
                    "invalid number of points in external \"{}\" value",
                    ty.display()
                );
            }
            let p = pts(floats(&mut b, n as usize * 2)?);
            if matches!(ty, T::Path) {
                Geo::Path(geo::Path { closed, p })
            } else {
                Geo::Polygon(geo::polygon(p)?)
            }
        }
        _ => unreachable!(),
    };
    if !b.is_empty() {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "incorrect binary data format"
        );
    }
    Ok(Value::Geo(Arc::new(g)))
}

fn encode_array(a: &ArrayValue, ctx: &FormatCtx, out: &mut BytesMut) -> Result<()> {
    out.put_i32(a.dims.len() as i32);
    out.put_i32(a.has_nulls() as i32);
    out.put_u32(a.elem.oid());
    for (len, lb) in &a.dims {
        out.put_i32(*len as i32);
        out.put_i32(*lb);
    }
    for item in &a.items {
        if item.is_null() {
            out.put_i32(-1);
            continue;
        }
        let mut buf = BytesMut::new();
        encode(item, &a.elem, ctx, &mut buf)?;
        out.put_i32(buf.len() as i32);
        out.put_slice(&buf);
    }
    Ok(())
}

/// Numeric binary format: ndigits, weight, sign, dscale, base-10000 digits.
fn encode_numeric(n: &Numeric, out: &mut BytesMut) {
    match n {
        Numeric::NaN => {
            out.put_i16(0);
            out.put_i16(0);
            out.put_u16(0xC000);
            out.put_i16(0);
        }
        Numeric::PosInf => {
            out.put_i16(0);
            out.put_i16(0);
            out.put_u16(0xD000);
            out.put_i16(0);
        }
        Numeric::NegInf => {
            out.put_i16(0);
            out.put_i16(0);
            out.put_u16(0xF000);
            out.put_i16(0);
        }
        Numeric::Finite { int, scale } => {
            let sign: u16 = if int.is_negative() { 0x4000 } else { 0 };
            let (groups, weight) = numeric_digits(int, *scale);
            out.put_i16(groups.len() as i16);
            out.put_i16(weight);
            out.put_u16(sign);
            out.put_i16(*scale as i16);
            for g in groups {
                out.put_i16(g);
            }
        }
    }
}

/// A finite numeric's base-10000 digits (without leading or trailing zero groups) and the
/// weight of the first; no digits for zero.
pub fn numeric_digits(int: &BigInt, scale: u32) -> (Vec<i16>, i16) {
    let abs = int.abs();
    if abs.is_zero() {
        return (Vec::new(), 0);
    }
    // Pad the fraction to a multiple of 4 digits so groups align at the decimal point.
    let pad = (4 - (scale % 4)) % 4;
    let scaled = &abs * BigInt::from(10u32).pow(pad);
    let mut groups: Vec<i16> = Vec::new();
    let mut x = scaled;
    let base = BigInt::from(10000u32);
    while !x.is_zero() {
        groups.push((&x % &base).to_i16().unwrap());
        x /= &base;
    }
    groups.reverse();
    let frac_groups = ((scale + pad) / 4) as i16;
    let weight = groups.len() as i16 - frac_groups - 1;
    // Strip trailing zero groups.
    while groups.last() == Some(&0) {
        groups.pop();
    }
    // Strip leading zero groups (adjusting weight).
    let lead = groups.iter().take_while(|g| **g == 0).count();
    (groups[lead..].to_vec(), weight - lead as i16)
}

fn decode_numeric(mut b: &[u8]) -> Result<Numeric> {
    if b.len() < 8 {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "insufficient data left in message"
        );
    }
    let ndigits = b.get_i16() as usize;
    let weight = b.get_i16() as i64;
    let sign = b.get_u16();
    let dscale = b.get_i16() as u32;
    match sign {
        0xC000 => return Ok(Numeric::NaN),
        0xD000 => return Ok(Numeric::PosInf),
        0xF000 => return Ok(Numeric::NegInf),
        _ => {}
    }
    if b.len() < ndigits * 2 {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "insufficient data left in message"
        );
    }
    let mut int = BigInt::zero();
    for _ in 0..ndigits {
        int = int * 10000 + BigInt::from(b.get_i16());
    }
    // Value = int * 10000^(weight - ndigits + 1)
    let exp4 = weight - ndigits as i64 + 1;
    let n = if exp4 >= 0 {
        Numeric::from_parts(int * BigInt::from(10u32).pow((exp4 * 4) as u32), 0)
    } else {
        Numeric::from_parts(int, (-exp4 * 4) as u32)
    };
    let n = n.with_scale(dscale);
    Ok(if sign == 0x4000 { n.neg() } else { n })
}

pub fn decode(b: &[u8], ty: &DataType, ctx: &FormatCtx) -> Result<Value> {
    use DataType as T;
    let need = |n: usize| -> Result<()> {
        if b.len() != n {
            bail!(
                INVALID_BINARY_REPRESENTATION,
                "incorrect binary data format in bind parameter"
            );
        }
        Ok(())
    };
    let mut r = b;
    Ok(match ty {
        T::Bool => {
            need(1)?;
            Value::Bool(b[0] != 0)
        }
        T::Int2 => {
            need(2)?;
            Value::Int2(r.get_i16())
        }
        T::Int4 => {
            need(4)?;
            Value::Int4(r.get_i32())
        }
        T::Int8 => {
            need(8)?;
            Value::Int8(r.get_i64())
        }
        T::Oid | T::Regclass | T::Regtype | T::Regproc | T::Regconfig | T::Regdictionary => {
            need(4)?;
            Value::Oid(r.get_u32())
        }
        T::Xid8 => {
            need(8)?;
            Value::Xid8(r.get_u64())
        }
        T::PgSnapshot | T::TxidSnapshot => {
            let bad = || {
                crate::error::Error::new(
                    crate::error::code::INVALID_BINARY_REPRESENTATION,
                    format!("invalid external {} data", ty.typname()),
                )
            };
            if b.len() < 20 {
                return Err(bad());
            }
            let n = r.get_i32();
            if n < 0 || b.len() != 20 + n as usize * 8 {
                return Err(bad());
            }
            let (xmin, xmax) = (r.get_u64(), r.get_u64());
            let xip: Vec<u64> = (0..n).map(|_| r.get_u64()).collect();
            let text = super::snapshot::Snapshot { xmin, xmax, xip }.format();
            Value::Snapshot(std::sync::Arc::new(
                super::snapshot::Snapshot::parse(&text, &ty.typname()).map_err(|_| bad())?,
            ))
        }
        T::TsVector => Value::TsVector(std::sync::Arc::new(super::tsearch::tsvector_recv(b)?)),
        T::TsQuery => Value::TsQuery(std::sync::Arc::new(super::tsearch::tsquery_recv(b)?)),
        T::Float4 => {
            need(4)?;
            Value::Float4(r.get_f32())
        }
        T::Float8 => {
            need(8)?;
            Value::Float8(r.get_f64())
        }
        T::Numeric(m) => Value::numeric(decode_numeric(b)?.apply_typmod(*m)?),
        T::Xml => Value::text(super::xml::xml_recv(
            b,
            if ctx.xml_document {
                super::xml::XmlOption::Document
            } else {
                super::xml::XmlOption::Content
            },
        )?),
        T::Text | T::Varchar(_) | T::Bpchar(_) | T::Name | T::Unknown | T::Refcursor => {
            let s = std::str::from_utf8(b).map_err(|_| {
                err!(
                    CHARACTER_NOT_IN_REPERTOIRE,
                    "invalid byte sequence for encoding \"UTF8\""
                )
            })?;
            parse_value(s, ty, ctx)?
        }
        T::Json => {
            let s = std::str::from_utf8(b).map_err(|_| {
                err!(
                    CHARACTER_NOT_IN_REPERTOIRE,
                    "invalid byte sequence for encoding \"UTF8\""
                )
            })?;
            parse_value(s, ty, ctx)?
        }
        T::Jsonb => {
            if b.first() != Some(&1) {
                bail!(
                    INVALID_BINARY_REPRESENTATION,
                    "unsupported jsonb version number {}",
                    b.first().copied().unwrap_or(0)
                );
            }
            let s = std::str::from_utf8(&b[1..]).map_err(|_| {
                err!(
                    CHARACTER_NOT_IN_REPERTOIRE,
                    "invalid byte sequence for encoding \"UTF8\""
                )
            })?;
            Value::Jsonb(Arc::new(parse_jsonb(s)?))
        }
        T::JsonPath => {
            if b.first() != Some(&1) {
                bail!(
                    INVALID_BINARY_REPRESENTATION,
                    "unsupported jsonpath version number: {}",
                    b.first().copied().unwrap_or(0)
                );
            }
            let s = std::str::from_utf8(&b[1..]).map_err(|_| {
                err!(
                    CHARACTER_NOT_IN_REPERTOIRE,
                    "invalid byte sequence for encoding \"UTF8\""
                )
            })?;
            parse_value(s, ty, ctx)?
        }
        T::Char => Value::Char(b.first().copied().unwrap_or(0)),
        T::Bytea => Value::bytea(b.to_vec()),
        T::PgNdistinct | T::PgDependencies | T::PgMcvList => bail!(
            FEATURE_NOT_SUPPORTED,
            "cannot accept a value of type {}",
            ty.typname()
        ),
        // gtsvector has no receive function; the BRIN summaries' reject every value.
        T::Gtsvector => bail!(
            UNDEFINED_FUNCTION,
            "no binary input function available for type gtsvector"
        ),
        T::PgBrinBloomSummary | T::PgBrinMinmaxMultiSummary => parse_value("", ty, ctx)?,
        T::Date => {
            need(4)?;
            Value::Date(r.get_i32())
        }
        T::Time(_) => {
            need(8)?;
            Value::Time(r.get_i64())
        }
        T::Timestamp(_) => {
            need(8)?;
            Value::Timestamp(r.get_i64())
        }
        T::Timestamptz(_) => {
            need(8)?;
            Value::Timestamptz(r.get_i64())
        }
        T::Interval(_) => {
            need(16)?;
            let micros = r.get_i64();
            let days = r.get_i32();
            let months = r.get_i32();
            Value::Interval(Interval::new(months, days, micros))
        }
        T::Uuid => {
            need(16)?;
            Value::Uuid(r.get_u128())
        }
        T::Array(elem) => decode_array(b, elem, ctx)?,
        T::Row(fields) => {
            let short = || {
                Error::new(
                    code::INVALID_BINARY_REPRESENTATION,
                    "insufficient data left in message",
                )
            };
            if r.len() < 4 {
                return Err(short());
            }
            let n = r.get_i32();
            if n != fields.len() as i32 {
                bail!(
                    DATATYPE_MISMATCH,
                    "wrong number of columns: {n}, expected {}",
                    fields.len()
                );
            }
            let mut items = Vec::with_capacity(fields.len());
            for (i, (_, ft)) in fields.iter().enumerate() {
                if r.len() < 8 {
                    return Err(short());
                }
                let oid = r.get_u32();
                if oid != ft.oid() {
                    bail!(
                        DATATYPE_MISMATCH,
                        "binary data has type {oid} ({}) instead of expected {} ({}) in record column {}",
                        DataType::from_oid(oid)
                            .map(|t| t.typname())
                            .unwrap_or_else(|| oid.to_string()),
                        ft.oid(),
                        ft.typname(),
                        i + 1
                    );
                }
                let len = r.get_i32();
                if len < 0 {
                    items.push(Value::Null);
                    continue;
                }
                if r.len() < len as usize {
                    return Err(short());
                }
                items.push(decode(&r[..len as usize], ft, ctx)?);
                r = &r[len as usize..];
            }
            Value::Record(Arc::new(items))
        }
        T::Range(rt) => super::range::value(decode_range(b, rt, ctx)?),
        T::Point | T::Lseg | T::Path | T::GBox | T::Polygon | T::Line | T::Circle => {
            decode_geo(b, ty)?
        }
        T::Multirange(rt) => {
            if r.len() < 4 {
                bail!(
                    INVALID_BINARY_REPRESENTATION,
                    "insufficient data left in message"
                );
            }
            let n = r.get_i32();
            let mut ranges = Vec::new();
            for _ in 0..n.max(0) {
                if r.len() < 4 {
                    bail!(
                        INVALID_BINARY_REPRESENTATION,
                        "insufficient data left in message"
                    );
                }
                let len = r.get_i32();
                if len < 0 || r.len() < len as usize {
                    bail!(
                        INVALID_BINARY_REPRESENTATION,
                        "insufficient data left in message"
                    );
                }
                ranges.push(decode_range(&r[..len as usize], rt, ctx)?);
                r.advance(len as usize);
            }
            super::range::multi_value(super::range::normalize(rt, ranges)?)
        }
        _ => {
            return Err(crate::error::Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("binary input for type {} is not supported", ty.display()),
            ));
        }
    })
}

fn decode_array(mut b: &[u8], elem: &DataType, ctx: &FormatCtx) -> Result<Value> {
    if b.len() < 12 {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "insufficient data left in message"
        );
    }
    let ndim = b.get_i32();
    let _has_nulls = b.get_i32();
    let _elem_oid = b.get_u32();
    if !(0..=6).contains(&ndim) {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "invalid number of dimensions: {ndim}"
        );
    }
    let mut dims = Vec::new();
    for _ in 0..ndim {
        if b.len() < 8 {
            bail!(
                INVALID_BINARY_REPRESENTATION,
                "insufficient data left in message"
            );
        }
        let len = b.get_i32() as usize;
        let lb = b.get_i32();
        dims.push((len, lb));
    }
    let total: usize = if dims.is_empty() {
        0
    } else {
        dims.iter().map(|d| d.0).product()
    };
    let mut items = Vec::with_capacity(total);
    for _ in 0..total {
        if b.len() < 4 {
            bail!(
                INVALID_BINARY_REPRESENTATION,
                "insufficient data left in message"
            );
        }
        let len = b.get_i32();
        if len < 0 {
            items.push(Value::Null);
            continue;
        }
        let len = len as usize;
        if b.len() < len {
            bail!(
                INVALID_BINARY_REPRESENTATION,
                "insufficient data left in message"
            );
        }
        items.push(decode(&b[..len], elem, ctx)?);
        b.advance(len);
    }
    Ok(Value::Array(Arc::new(ArrayValue {
        elem: elem.clone(),
        dims,
        items,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_roundtrip() {
        let ctx = FormatCtx::default();
        for s in [
            "0",
            "1",
            "-1.5",
            "12345.6789",
            "0.0001",
            "100000000",
            "3.14159265358979",
            "0.00",
        ] {
            let n: Numeric = s.parse().unwrap();
            let mut buf = BytesMut::new();
            encode_numeric(&n, &mut buf);
            let back = decode(&buf, &DataType::NUMERIC, &ctx).unwrap();
            assert_eq!(super::super::text::format_plain(&back, &ctx), s, "{s}");
        }
    }
}
