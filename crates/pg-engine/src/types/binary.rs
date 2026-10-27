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
        T::Timetz(_)
        | T::Inet
        | T::Cidr
        | T::Macaddr
        | T::Macaddr8
        | T::Bit(_)
        | T::Varbit(_)
        | T::Money
        | T::PgLsn
        | T::Xid
        | T::Cid
        | T::Tid
        | T::Xml
        | T::Refcursor
        | T::Enum(..)
        | T::Regprocedure
        | T::Regnamespace
        | T::Regrole
        | T::Regoper
        | T::Regoperator
        | T::Regcollation
        | T::Int2Vector
        | T::OidVector
        | T::Cstring
        | T::Void
        | T::PgNodeTree
        | T::Record => true,
        T::Row(fields) => fields.iter().all(|(_, t)| supports_binary(t)),
        // An extension's native type: when it has send and receive functions.
        T::Base(b) if b.ext != 0 => crate::ext::binary::has_binary(b.ext),
        T::Base(b) => b.repr.as_ref().is_some_and(supports_binary),
        _ => false,
    }
}

/// Text a send function writes (pq_sendtext): in the client's encoding.
pub(crate) fn put_text(out: &mut BytesMut, s: &str, ctx: &FormatCtx) -> Result<()> {
    out.put_slice(&super::mbconv::server::to_client(s, ctx.client_encoding, ctx.server_encoding)?);
    Ok(())
}

/// Text a receive function reads (pq_getmsgtext): from the client's encoding, checked.
pub(crate) fn get_text(b: &[u8], ctx: &FormatCtx) -> Result<String> {
    super::mbconv::server::from_client(b, ctx.client_encoding, ctx.server_encoding)
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
        // A made base type sends as its representation's type does.
        // An extension's native type: its own send function.
        (Value::Ext(kind, b), _) => crate::ext::binary::send(*kind, b, ctx, out)?,
        (_, T::Base(b)) if b.repr.is_some() => encode(v, b.repr.as_ref().unwrap(), ctx, out)?,
        // jsonpath_send: a version byte, then the text.
        (Value::Text(s), T::JsonPath) => {
            out.put_u8(1);
            put_text(out, s, ctx)?;
        }
        // xml_send: the text with its declaration normalized (xml_out_internal).
        (Value::Text(s), T::Xml) => put_text(out, &super::xml::xml_out(s), ctx)?,
        (Value::Text(_), T::Aclitem) => bail!(
            UNDEFINED_FUNCTION,
            "no binary output function available for type aclitem"
        ),
        // A composite, range or array value kept as its text: by the type's own send.
        (Value::Text(s), T::Row(_) | T::Range(_) | T::Multirange(_) | T::Array(_)) => {
            let v = parse_value(s, ty, ctx)?;
            if matches!(v, Value::Text(_)) {
                out.put_slice(s.as_bytes());
            } else {
                encode(&v, ty, ctx, out)?;
            }
        }
        (Value::Text(s), _) => put_text(out, s, ctx)?,
        (Value::Json(s), _) => put_text(out, s, ctx)?,
        (Value::Jsonb(j), _) => {
            out.put_u8(1);
            put_text(out, &j.to_string(), ctx)?;
        }
        // timetz_send: the time, then the zone (seconds west of UTC).
        (Value::Timetz(t, z), _) => {
            out.put_i64(*t);
            out.put_i32(*z);
        }
        // inet_send / cidr_send: family, bits, is_cidr, address length, address.
        (Value::Inet(a, bits), _) => {
            out.put_u8(if a.len() == 4 {
                PGSQL_AF_INET
            } else {
                PGSQL_AF_INET6
            });
            out.put_u8(*bits);
            out.put_u8(matches!(ty, T::Cidr) as u8);
            out.put_u8(a.len() as u8);
            out.put_slice(a);
        }
        (Value::Macaddr(m), _) => out.put_slice(m),
        (Value::Macaddr8(m), _) => out.put_slice(m),
        // bit_send / varbit_send: the length in bits, then the bytes.
        (Value::Bits(len, b), _) => {
            out.put_i32(*len as i32);
            out.put_slice(b);
        }
        (Value::Money(c), _) => out.put_i64(*c),
        (Value::PgLsn(l), _) => out.put_u64(*l),
        (Value::Tid(blk, off), _) => {
            out.put_u32(*blk);
            out.put_u16(*off);
        }
        // int2vectorsend / oidvectorsend are array_send's.
        (Value::Array(a), T::Int2Vector | T::OidVector) => encode_array(a, &a.elem, ctx, out)?,
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
        // array_send by the column's element type (a stored array of composites may hold its
        // elements as their text).
        (Value::Array(a), T::Array(e)) => {
            let elem = if matches!(**e, T::Record | T::Unknown | T::AnyElement) {
                &a.elem
            } else {
                &**e
            };
            encode_array(a, elem, ctx, out)?
        }
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
                out.put_u32(field_oid(ty, i, &ft, ctx));
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

/// The type oid record_send / record_recv give field `i` of a row type: its attribute's
/// (a domain, when declared as one), else its value type's.
fn field_oid(ty: &DataType, i: usize, ft: &DataType, ctx: &FormatCtx) -> u32 {
    match (ty, &ctx.names) {
        (DataType::Row(rt), Some(n)) if rt.oid != 0 => {
            n.field_domain(rt.oid, i).unwrap_or_else(|| ft.oid())
        }
        _ => ft.oid(),
    }
}

// TZDISP_LIMIT: a zone displacement's bound, in seconds.
const TZDISP_LIMIT: i32 = 16 * 3600;

// inet's address families on the wire.
const PGSQL_AF_INET: u8 = 2;
const PGSQL_AF_INET6: u8 = 3;

/// inet_recv / cidr_recv.
fn decode_inet(b: &[u8], cidr: bool) -> Result<Value> {
    let tname = if cidr { "cidr" } else { "inet" };
    let short = || err!(PROTOCOL_VIOLATION, "insufficient data left in message");
    if b.len() < 4 {
        return Err(short());
    }
    let (family, bits, nb) = (b[0], b[1], b[3]);
    let size = match family {
        PGSQL_AF_INET => 4,
        PGSQL_AF_INET6 => 16,
        _ => bail!(
            INVALID_BINARY_REPRESENTATION,
            "invalid address family in external \"{tname}\" value"
        ),
    };
    if bits as usize > size * 8 {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "invalid bits in external \"{tname}\" value"
        );
    }
    if nb as usize != size {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "invalid length in external \"{tname}\" value"
        );
    }
    if b.len() < 4 + size {
        return Err(short());
    }
    let addr = &b[4..4 + size];
    if b.len() > 4 + size {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "incorrect binary data format"
        );
    }
    if cidr && super::text::inet_network(addr, bits) != addr {
        return Err(err!(
            INVALID_BINARY_REPRESENTATION,
            "invalid external \"cidr\" value"
        )
        .detail("Value has bits set to right of mask."));
    }
    Ok(Value::Inet(addr.into(), bits))
}

/// bit_recv / varbit_recv.
fn decode_bits(mut b: &[u8], ty: &DataType) -> Result<Value> {
    if b.len() < 4 {
        bail!(PROTOCOL_VIOLATION, "insufficient data left in message");
    }
    let len = b.get_i32();
    // VARBITMAXLEN
    if !(0..=(0x3fff_ffff - 8)).contains(&len) {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "invalid length in external bit string"
        );
    }
    let typmod = ty.typmod();
    match ty {
        DataType::Bit(_) if typmod > 0 && len != typmod => bail!(
            STRING_DATA_LENGTH_MISMATCH,
            "bit string length {len} does not match type bit({typmod})"
        ),
        DataType::Varbit(_) if typmod > 0 && len > typmod => bail!(
            STRING_DATA_RIGHT_TRUNCATION,
            "bit string too long for type bit varying({typmod})"
        ),
        _ => {}
    }
    let n = (len as usize).div_ceil(8);
    if b.len() < n {
        bail!(PROTOCOL_VIOLATION, "insufficient data left in message");
    }
    let mut bytes = b[..n].to_vec();
    // VARBIT_PAD: the bits past the length are zero.
    if len % 8 != 0
        && let Some(last) = bytes.last_mut()
    {
        *last &= 0xffu8 << (8 - len % 8);
    }
    Ok(Value::Bits(len as u32, bytes.into()))
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
            PROTOCOL_VIOLATION,
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
                PROTOCOL_VIOLATION,
                "insufficient data left in message"
            );
        }
        let len = b.get_i32();
        if len < 0 || b.len() < len as usize {
            bail!(
                PROTOCOL_VIOLATION,
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

fn encode_array(
    a: &ArrayValue,
    elem: &DataType,
    ctx: &FormatCtx,
    out: &mut BytesMut,
) -> Result<()> {
    out.put_i32(a.dims.len() as i32);
    out.put_i32(a.has_nulls() as i32);
    out.put_u32(elem.oid());
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
        encode(item, elem, ctx, &mut buf)?;
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
    // (In i64: the largest numerics have more groups than an i16 counts, 131072 digits
    // before the point and 16383 after.)
    let frac_groups = ((scale + pad) / 4) as i64;
    let weight = groups.len() as i64 - frac_groups - 1;
    // Strip trailing zero groups.
    while groups.last() == Some(&0) {
        groups.pop();
    }
    // Strip leading zero groups (adjusting weight).
    let lead = groups.iter().take_while(|g| **g == 0).count();
    (groups[lead..].to_vec(), (weight - lead as i64) as i16)
}

fn decode_numeric(mut b: &[u8]) -> Result<Numeric> {
    if b.len() < 8 {
        bail!(
            PROTOCOL_VIOLATION,
            "insufficient data left in message"
        );
    }
    // numeric_recv: the digit count and scale are unsigned.
    let ndigits = b.get_u16() as usize;
    let weight = b.get_i16() as i64;
    let sign = b.get_u16();
    let dscale = b.get_u16() as u32;
    match sign {
        0xC000 => return Ok(Numeric::NaN),
        0xD000 => return Ok(Numeric::PosInf),
        0xF000 => return Ok(Numeric::NegInf),
        0x0000 | 0x4000 => {}
        _ => bail!(
            INVALID_BINARY_REPRESENTATION,
            "invalid sign in external \"numeric\" value"
        ),
    }
    // NUMERIC_DSCALE_MAX
    if dscale > 0x3FFF {
        bail!(
            INVALID_BINARY_REPRESENTATION,
            "invalid scale in external \"numeric\" value"
        );
    }
    if b.len() < ndigits * 2 {
        bail!(
            PROTOCOL_VIOLATION,
            "insufficient data left in message"
        );
    }
    let mut int = BigInt::zero();
    for _ in 0..ndigits {
        let d = b.get_i16();
        if !(0..10000).contains(&d) {
            bail!(
                INVALID_BINARY_REPRESENTATION,
                "invalid digit in external \"numeric\" value"
            );
        }
        int = int * 10000 + BigInt::from(d);
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
    // A fixed-size type's recv: too few bytes is pq_getmsg*'s error; bytes left over are
    // exec_bind_message's (which names the parameter).
    let need = |n: usize| -> Result<()> {
        if b.len() < n {
            bail!(PROTOCOL_VIOLATION, "insufficient data left in message");
        }
        if b.len() > n {
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
        T::Oid
        | T::Regclass
        | T::Regtype
        | T::Regproc
        | T::Regconfig
        | T::Regdictionary
        | T::Regprocedure
        | T::Regnamespace
        | T::Regrole
        | T::Regoper
        | T::Regoperator
        | T::Regcollation
        | T::Xid
        | T::Cid => {
            need(4)?;
            Value::Oid(r.get_u32())
        }
        T::Base(bt) if bt.ext != 0 => Value::Ext(bt.ext, crate::ext::binary::recv(bt.ext, b, ctx)?.into()),
        T::Base(bt) if bt.repr.is_some() => decode(b, bt.repr.as_ref().unwrap(), ctx)?,
        // timetz_recv: the time and zone in range, then the typmod's rounding.
        T::Timetz(m) => {
            need(12)?;
            let t = r.get_i64();
            if !(0..=super::value::USECS_PER_DAY).contains(&t) {
                bail!(NUMERIC_VALUE_OUT_OF_RANGE, "time out of range");
            }
            let z = r.get_i32();
            if z <= -TZDISP_LIMIT || z >= TZDISP_LIMIT {
                bail!(
                    INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
                    "time zone displacement out of range"
                );
            }
            Value::Timetz(super::datetime::round_micros(t, *m), z)
        }
        T::Inet => decode_inet(b, false)?,
        T::Cidr => decode_inet(b, true)?,
        T::Macaddr => {
            need(6)?;
            Value::Macaddr(b.try_into().unwrap())
        }
        // macaddr8_recv also takes a 6-byte address (FF:FE in the middle).
        T::Macaddr8 => match b.len() {
            6 => Value::Macaddr8([b[0], b[1], b[2], 0xff, 0xfe, b[3], b[4], b[5]]),
            _ => {
                need(8)?;
                Value::Macaddr8(b.try_into().unwrap())
            }
        },
        T::Bit(_) | T::Varbit(_) => decode_bits(b, ty)?,
        T::Money => {
            need(8)?;
            Value::Money(r.get_i64())
        }
        T::PgLsn => {
            need(8)?;
            Value::PgLsn(r.get_u64())
        }
        T::Tid => {
            need(6)?;
            Value::Tid(r.get_u32(), r.get_u16())
        }
        // int2vectorrecv / oidvectorrecv: array_recv, then one dimension from 0, no NULLs.
        T::Int2Vector | T::OidVector => {
            let elem = if matches!(ty, T::Int2Vector) {
                T::Int2
            } else {
                T::Oid
            };
            let v = decode_array(b, &elem, ctx)?;
            if let Value::Array(a) = &v
                && (a.dims.len() > 1
                    || a.items.iter().any(Value::is_null)
                    || a.dims.first().is_some_and(|d| d.1 != 0))
            {
                bail!(
                    INVALID_PARAMETER_VALUE,
                    "invalid {} data",
                    ty.typname()
                );
            }
            v
        }
        T::Enum(..) => parse_value(&get_text(b, ctx)?, ty, ctx)?,
        T::Cstring => Value::text(get_text(b, ctx)?),
        // void_recv: nothing to read.
        T::Void => Value::Null,
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
        T::Text
        | T::Varchar(_)
        | T::Bpchar(_)
        | T::Name
        | T::Unknown
        | T::Refcursor
        | T::Json => parse_value(&get_text(b, ctx)?, ty, ctx)?,
        T::Jsonb => {
            if b.first() != Some(&1) {
                bail!(
                    INVALID_BINARY_REPRESENTATION,
                    "unsupported jsonb version number {}",
                    b.first().copied().unwrap_or(0)
                );
            }
            Value::Jsonb(Arc::new(parse_jsonb(&get_text(&b[1..], ctx)?)?))
        }
        T::JsonPath => {
            if b.first() != Some(&1) {
                bail!(
                    INVALID_BINARY_REPRESENTATION,
                    "unsupported jsonpath version number: {}",
                    b.first().copied().unwrap_or(0)
                );
            }
            parse_value(&get_text(&b[1..], ctx)?, ty, ctx)?
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
        // date_recv: the range date_in accepts (IS_VALID_DATE), or infinity.
        T::Date => {
            need(4)?;
            let d = r.get_i32();
            if d != super::datetime::DATE_NEG_INF
                && d != super::datetime::DATE_POS_INF
                && !super::datetime::is_valid_date(d)
            {
                bail!(DATETIME_FIELD_OVERFLOW, "date out of range");
            }
            Value::Date(d)
        }
        // time_recv: in range, then the typmod's rounding.
        T::Time(m) => {
            need(8)?;
            let t = r.get_i64();
            if !(0..=super::value::USECS_PER_DAY).contains(&t) {
                bail!(NUMERIC_VALUE_OUT_OF_RANGE, "time out of range");
            }
            Value::Time(super::datetime::round_micros(t, *m))
        }
        // timestamp_recv / timestamptz_recv: what timestamp_out can print (IS_VALID_TIMESTAMP),
        // or infinity; then the typmod's rounding (AdjustTimestampForTypmod).
        T::Timestamp(m) | T::Timestamptz(m) => {
            need(8)?;
            let mut t = r.get_i64();
            if super::datetime::ts_is_finite(t) {
                if !super::datetime::ts_is_valid(t) {
                    bail!(DATETIME_FIELD_OVERFLOW, "timestamp out of range");
                }
                t = super::datetime::round_micros(t, *m);
                if !super::datetime::ts_is_valid(t) {
                    bail!(DATETIME_FIELD_OVERFLOW, "timestamp out of range");
                }
            }
            if matches!(ty, T::Timestamp(_)) {
                Value::Timestamp(t)
            } else {
                Value::Timestamptz(t)
            }
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
                    code::PROTOCOL_VIOLATION,
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
                let want = field_oid(ty, i, ft, ctx);
                if oid != want {
                    bail!(
                        DATATYPE_MISMATCH,
                        "binary data has type {oid} ({}) instead of expected {} ({}) in record column {}",
                        DataType::from_oid(oid)
                            .map(|t| t.typname())
                            .unwrap_or_else(|| oid.to_string()),
                        want,
                        ctx.names
                            .as_ref()
                            .and_then(|n| n.type_name(want))
                            .unwrap_or_else(|| ft.typname()),
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
                    PROTOCOL_VIOLATION,
                    "insufficient data left in message"
                );
            }
            let n = r.get_i32();
            let mut ranges = Vec::new();
            for _ in 0..n.max(0) {
                if r.len() < 4 {
                    bail!(
                        PROTOCOL_VIOLATION,
                        "insufficient data left in message"
                    );
                }
                let len = r.get_i32();
                if len < 0 || r.len() < len as usize {
                    bail!(
                        PROTOCOL_VIOLATION,
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
            PROTOCOL_VIOLATION,
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
                PROTOCOL_VIOLATION,
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
    // (array_recv: no elements is the empty array, of no dimensions.)
    if total == 0 {
        dims.clear();
    }
    let mut items = Vec::with_capacity(total);
    for _ in 0..total {
        if b.len() < 4 {
            bail!(
                PROTOCOL_VIOLATION,
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
                PROTOCOL_VIOLATION,
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
