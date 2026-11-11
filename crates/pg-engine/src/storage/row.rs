//! Row encoding: a self-describing binary format (tag + payload per value).

use super::sst::{get_varint, put_varint};
use crate::error::{Result, internal};
use crate::types::array::ArrayValue;
use crate::types::json::parse_jsonb;
use crate::types::numeric::Numeric;
use crate::types::{DataType, Interval, Value};
use crate::types::decint::DecInt;
use std::sync::Arc;

pub(crate) mod tag {
    pub const NULL: u8 = 0;
    pub const FALSE: u8 = 1;
    pub const TRUE: u8 = 2;
    pub const INT2: u8 = 3;
    pub const INT4: u8 = 4;
    pub const INT8: u8 = 5;
    pub const FLOAT4: u8 = 6;
    pub const FLOAT8: u8 = 7;
    pub const NUMERIC: u8 = 8;
    pub const TEXT: u8 = 9;
    pub const BYTEA: u8 = 10;
    pub const DATE: u8 = 11;
    pub const TIME: u8 = 12;
    pub const TIMESTAMP: u8 = 13;
    pub const TIMESTAMPTZ: u8 = 14;
    pub const INTERVAL: u8 = 15;
    pub const UUID: u8 = 16;
    pub const JSON: u8 = 17;
    pub const JSONB: u8 = 18;
    pub const OID: u8 = 19;
    pub const ARRAY: u8 = 20;
    pub const CHAR: u8 = 21;
    pub const TIMETZ: u8 = 22;
    pub const RECORD: u8 = 23;
    pub const BITS: u8 = 24;
    pub const INET: u8 = 25;
    pub const MACADDR: u8 = 26;
    pub const MONEY: u8 = 27;
    pub const LSN: u8 = 28;
    pub const TID: u8 = 29;
    pub const NUM_NAN: u8 = 30;
    pub const NUM_PINF: u8 = 31;
    pub const NUM_NINF: u8 = 32;
    pub const RANGE: u8 = 33;
    pub const MULTIRANGE: u8 = 34;
    pub const GEO: u8 = 35;
    pub const MACADDR8: u8 = 36;
    pub const TSVECTOR: u8 = 37;
    pub const TSQUERY: u8 = 38;
    pub const XID8: u8 = 39;
    pub const SNAPSHOT: u8 = 40;
    /// An extension's native type: kind (u16) and its bytes.
    pub const EXT: u8 = 41;
    /// A value stored out of line (`storage::toast`): the writing transaction, its sequence
    /// number there and the value's encoded length, as varints.
    pub const EXTERNAL: u8 = 42;
    /// A numeric past i128: scale (varint), then bytes: a sign byte and its base-10^9 limbs
    /// (u32 LE, least significant first). (Up to i128, `NUMERIC`'s two's complement bytes.)
    pub const NUMERIC_DEC: u8 = 43;
}

/// The tag of a value stored out of line (`storage::toast`).
pub const EXTERNAL: u8 = tag::EXTERNAL;

/// Stored rows start with a header byte (never a value tag) followed by the writing
/// transaction's id as a varint: the row's `xmin`.
pub const ROW_HEADER: u8 = 0xFF;
/// The header of a row stored with the heap TID its version got (block and offset as
/// varints after the xmin): a heap model laid out afresh puts the rows back in that
/// order.
pub const ROW_HEADER_TID: u8 = 0xFE;
/// `ROW_HEADER` and `ROW_HEADER_TID` of a row with values stored out of line
/// (`storage::toast`; builds before it don't read them).
pub const ROW_HEADER_EXT: u8 = 0xFD;
pub const ROW_HEADER_TID_EXT: u8 = 0xFC;

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

pub fn encode_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(tag::NULL),
        Value::Bool(b) => out.push(if *b { tag::TRUE } else { tag::FALSE }),
        Value::Int2(i) => {
            out.push(tag::INT2);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Int4(i) => {
            out.push(tag::INT4);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Int8(i) => {
            out.push(tag::INT8);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Float4(f) => {
            out.push(tag::FLOAT4);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Value::Float8(f) => {
            out.push(tag::FLOAT8);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Value::Numeric(n) => match n.as_ref() {
            Numeric::NaN => out.push(tag::NUM_NAN),
            Numeric::PosInf => out.push(tag::NUM_PINF),
            Numeric::NegInf => out.push(tag::NUM_NINF),
            Numeric::Finite { int, scale } if int.to_i128().is_some() => {
                out.push(tag::NUMERIC);
                put_varint(out, *scale as u64);
                put_bytes(out, &int.to_signed_bytes_le());
            }
            Numeric::Finite { int, scale } => {
                out.push(tag::NUMERIC_DEC);
                put_varint(out, *scale as u64);
                let mut b = Vec::with_capacity(1 + 4 * int.limbs().len());
                b.push(int.is_negative() as u8);
                for l in int.limbs() {
                    b.extend_from_slice(&l.to_le_bytes());
                }
                put_bytes(out, &b);
            }
        },
        Value::Text(s) => {
            out.push(tag::TEXT);
            put_bytes(out, s.as_bytes());
        }
        Value::Json(s) => {
            out.push(tag::JSON);
            put_bytes(out, s.as_bytes());
        }
        Value::Jsonb(j) => {
            out.push(tag::JSONB);
            put_bytes(out, j.to_string().as_bytes());
        }
        Value::Bytea(b) => {
            out.push(tag::BYTEA);
            put_bytes(out, b);
        }
        Value::Char(c) => {
            out.push(tag::CHAR);
            out.push(*c);
        }
        Value::Date(d) => {
            out.push(tag::DATE);
            out.extend_from_slice(&d.to_le_bytes());
        }
        Value::Time(t) => {
            out.push(tag::TIME);
            out.extend_from_slice(&t.to_le_bytes());
        }
        Value::Timetz(t, z) => {
            out.push(tag::TIMETZ);
            out.extend_from_slice(&t.to_le_bytes());
            out.extend_from_slice(&z.to_le_bytes());
        }
        Value::Timestamp(t) => {
            out.push(tag::TIMESTAMP);
            out.extend_from_slice(&t.to_le_bytes());
        }
        Value::Timestamptz(t) => {
            out.push(tag::TIMESTAMPTZ);
            out.extend_from_slice(&t.to_le_bytes());
        }
        Value::Interval(i) => {
            out.push(tag::INTERVAL);
            out.extend_from_slice(&i.months.to_le_bytes());
            out.extend_from_slice(&i.days.to_le_bytes());
            out.extend_from_slice(&i.micros.to_le_bytes());
        }
        Value::Uuid(u) => {
            out.push(tag::UUID);
            out.extend_from_slice(&u.to_be_bytes());
        }
        Value::Oid(o) => {
            out.push(tag::OID);
            out.extend_from_slice(&o.to_le_bytes());
        }
        Value::Array(a) => {
            out.push(tag::ARRAY);
            encode_type(out, &a.elem);
            put_varint(out, a.dims.len() as u64);
            for (len, lb) in &a.dims {
                put_varint(out, *len as u64);
                out.extend_from_slice(&lb.to_le_bytes());
            }
            put_varint(out, a.items.len() as u64);
            for item in &a.items {
                encode_value(out, item);
            }
        }
        Value::Range(r) => {
            out.push(tag::RANGE);
            encode_range(out, r);
        }
        Value::Multirange(m) => {
            out.push(tag::MULTIRANGE);
            put_varint(out, m.len() as u64);
            for r in m.iter() {
                encode_range(out, r);
            }
        }
        Value::Record(items) => {
            out.push(tag::RECORD);
            put_varint(out, items.len() as u64);
            for item in items.iter() {
                encode_value(out, item);
            }
        }
        Value::Bits(len, b) => {
            out.push(tag::BITS);
            put_varint(out, *len as u64);
            put_bytes(out, b);
        }
        Value::Inet(a, m) => {
            out.push(tag::INET);
            put_bytes(out, a);
            out.push(*m);
        }
        Value::Geo(g) => {
            out.push(tag::GEO);
            out.push(g.kind() * 2 + g.is_closed_path() as u8);
            let f = g.floats();
            put_varint(out, f.len() as u64);
            for x in f {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        Value::Macaddr(m) => {
            out.push(tag::MACADDR);
            out.extend_from_slice(m);
        }
        Value::Macaddr8(m) => {
            out.push(tag::MACADDR8);
            out.extend_from_slice(m);
        }
        // Stored in the binary send format.
        Value::TsVector(v) => {
            out.push(tag::TSVECTOR);
            let mut b = Vec::new();
            crate::types::tsearch::tsvector_send(v, &mut b);
            put_bytes(out, &b);
        }
        Value::Ext(k, b) => {
            out.push(tag::EXT);
            out.extend_from_slice(&k.to_le_bytes());
            put_bytes(out, b);
        }
        Value::TsQuery(q) => {
            out.push(tag::TSQUERY);
            let mut b = Vec::new();
            crate::types::tsearch::tsquery_send(q, &mut b);
            put_bytes(out, &b);
        }
        Value::Money(c) => {
            out.push(tag::MONEY);
            out.extend_from_slice(&c.to_le_bytes());
        }
        Value::PgLsn(l) => {
            out.push(tag::LSN);
            out.extend_from_slice(&l.to_le_bytes());
        }
        Value::Xid8(x) => {
            out.push(tag::XID8);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Snapshot(s) => {
            out.push(tag::SNAPSHOT);
            put_bytes(out, s.format().as_bytes());
        }
        Value::Tid(b, o) => {
            out.push(tag::TID);
            out.extend_from_slice(&b.to_le_bytes());
            out.extend_from_slice(&o.to_le_bytes());
        }
    }
}

/// Types inside arrays are stored by OID + typmod.
fn encode_range(out: &mut Vec<u8>, r: &crate::types::range::Range) {
    let flags = r.empty as u8
        | (r.lower_inc as u8) << 1
        | (r.upper_inc as u8) << 2
        | (r.lower.is_some() as u8) << 3
        | (r.upper.is_some() as u8) << 4;
    out.push(flags);
    if let Some(v) = &r.lower {
        encode_value(out, v);
    }
    if let Some(v) = &r.upper {
        encode_value(out, v);
    }
}

fn decode_range(b: &[u8], pos: &mut usize) -> Result<crate::types::range::Range> {
    let flags = take(b, pos, 1)?[0];
    let lower = if flags & 8 != 0 {
        Some(decode_value(b, pos)?)
    } else {
        None
    };
    let upper = if flags & 16 != 0 {
        Some(decode_value(b, pos)?)
    } else {
        None
    };
    Ok(crate::types::range::Range {
        empty: flags & 1 != 0,
        lower_inc: flags & 2 != 0,
        upper_inc: flags & 4 != 0,
        lower,
        upper,
    })
}

fn encode_type(out: &mut Vec<u8>, t: &DataType) {
    out.extend_from_slice(&t.oid().to_le_bytes());
    out.extend_from_slice(&t.typmod().to_le_bytes());
}

fn decode_type(b: &[u8], pos: &mut usize) -> Result<DataType> {
    let oid = u32::from_le_bytes(take(b, pos, 4)?.try_into().unwrap());
    let typmod = i32::from_le_bytes(take(b, pos, 4)?.try_into().unwrap());
    let t = DataType::from_oid(oid).unwrap_or(DataType::Text);
    Ok(with_typmod(t, typmod))
}

pub fn with_typmod(t: DataType, m: i32) -> DataType {
    use DataType::*;
    match t {
        Numeric(_) => Numeric(m),
        Varchar(_) => Varchar(m),
        Bpchar(_) => Bpchar(m),
        Time(_) => Time(m),
        Timetz(_) => Timetz(m),
        Timestamp(_) => Timestamp(m),
        Timestamptz(_) => Timestamptz(m),
        Interval(_) => Interval(m),
        Bit(_) => Bit(m),
        Varbit(_) => Varbit(m),
        Base(b) => DataType::base_with_typmod(&b, m),
        // (An array's modifier is its element's; rows store arrays without one.)
        Array(e) => Array(Box::new(with_typmod(*e, m))),
        t => t,
    }
}

fn take<'a>(b: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    let s = b
        .get(*pos..*pos + n)
        .ok_or_else(|| internal("truncated row"))?;
    *pos += n;
    Ok(s)
}

fn take_bytes<'a>(b: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    let n = get_varint(b, pos)? as usize;
    take(b, pos, n)
}

fn text(b: &[u8]) -> Result<Arc<str>> {
    std::str::from_utf8(b)
        .map(Arc::from)
        .map_err(|_| internal("invalid UTF-8 in stored row"))
}

pub fn decode_value(b: &[u8], pos: &mut usize) -> Result<Value> {
    let t = *b.get(*pos).ok_or_else(|| internal("truncated row"))?;
    *pos += 1;
    macro_rules! le {
        ($ty:ty, $n:expr) => {
            <$ty>::from_le_bytes(take(b, pos, $n)?.try_into().unwrap())
        };
    }
    Ok(match t {
        tag::NULL => Value::Null,
        tag::FALSE => Value::Bool(false),
        tag::TRUE => Value::Bool(true),
        tag::INT2 => Value::Int2(le!(i16, 2)),
        tag::INT4 => Value::Int4(le!(i32, 4)),
        tag::INT8 => Value::Int8(le!(i64, 8)),
        tag::FLOAT4 => Value::Float4(le!(f32, 4)),
        tag::FLOAT8 => Value::Float8(le!(f64, 8)),
        tag::NUMERIC => {
            let scale = get_varint(b, pos)? as u32;
            let bytes = take_bytes(b, pos)?;
            // (Up to 8 bytes: sign-extended to an i64, the common case, without the
            // general conversion.)
            let int = if !bytes.is_empty() && bytes.len() <= 8 {
                let fill = if bytes[bytes.len() - 1] & 0x80 != 0 { 0xFF } else { 0 };
                let mut w = [fill; 8];
                w[..bytes.len()].copy_from_slice(bytes);
                DecInt::from(i64::from_le_bytes(w))
            } else {
                DecInt::from_signed_bytes_le(bytes)
            };
            Value::numeric(Numeric::from_parts(int, scale))
        }
        tag::NUMERIC_DEC => {
            let scale = get_varint(b, pos)? as u32;
            let bytes = take_bytes(b, pos)?;
            let int = match bytes.split_first() {
                Some((&neg, limbs)) if neg <= 1 && limbs.len() % 4 == 0 => DecInt::from_limbs(
                    neg == 1,
                    limbs.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
                ),
                _ => None,
            };
            let Some(int) = int else {
                return Err(internal("invalid numeric in stored row"));
            };
            Value::numeric(Numeric::from_parts(int, scale))
        }
        tag::NUM_NAN => Value::numeric(Numeric::NaN),
        tag::NUM_PINF => Value::numeric(Numeric::PosInf),
        tag::NUM_NINF => Value::numeric(Numeric::NegInf),
        tag::TEXT => Value::Text(text(take_bytes(b, pos)?)?),
        tag::JSON => Value::Json(text(take_bytes(b, pos)?)?),
        tag::JSONB => Value::Jsonb(Arc::new(parse_jsonb(&text(take_bytes(b, pos)?)?)?)),
        tag::BYTEA => Value::bytea(take_bytes(b, pos)?.to_vec()),
        tag::CHAR => Value::Char(take(b, pos, 1)?[0]),
        tag::DATE => Value::Date(le!(i32, 4)),
        tag::TIME => Value::Time(le!(i64, 8)),
        tag::TIMETZ => {
            let t = le!(i64, 8);
            Value::Timetz(t, le!(i32, 4))
        }
        tag::TIMESTAMP => Value::Timestamp(le!(i64, 8)),
        tag::TIMESTAMPTZ => Value::Timestamptz(le!(i64, 8)),
        tag::INTERVAL => {
            let months = le!(i32, 4);
            let days = le!(i32, 4);
            Value::Interval(Interval::new(months, days, le!(i64, 8)))
        }
        tag::UUID => Value::Uuid(u128::from_be_bytes(take(b, pos, 16)?.try_into().unwrap())),
        tag::OID => Value::Oid(le!(u32, 4)),
        tag::ARRAY => {
            let elem = decode_type(b, pos)?;
            let nd = get_varint(b, pos)? as usize;
            let mut dims = Vec::with_capacity(nd);
            for _ in 0..nd {
                let len = get_varint(b, pos)? as usize;
                dims.push((len, le!(i32, 4)));
            }
            let n = get_varint(b, pos)? as usize;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(decode_value(b, pos)?);
            }
            Value::Array(Arc::new(ArrayValue { elem, dims, items }))
        }
        tag::RANGE => Value::Range(Arc::new(decode_range(b, pos)?)),
        tag::MULTIRANGE => {
            let n = get_varint(b, pos)? as usize;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(decode_range(b, pos)?);
            }
            Value::Multirange(Arc::new(items))
        }
        tag::RECORD => {
            let n = get_varint(b, pos)? as usize;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(decode_value(b, pos)?);
            }
            Value::Record(Arc::new(items))
        }
        tag::BITS => {
            let len = get_varint(b, pos)? as u32;
            Value::Bits(len, take_bytes(b, pos)?.to_vec().into())
        }
        tag::INET => {
            let a = take_bytes(b, pos)?.to_vec();
            Value::Inet(a.into(), take(b, pos, 1)?[0])
        }
        tag::EXT => {
            let k = u16::from_le_bytes(take(b, pos, 2)?.try_into().unwrap());
            Value::Ext(k, Arc::from(take_bytes(b, pos)?))
        }
        tag::MACADDR => Value::Macaddr(take(b, pos, 6)?.try_into().unwrap()),
        tag::MACADDR8 => Value::Macaddr8(take(b, pos, 8)?.try_into().unwrap()),
        tag::TSVECTOR => Value::TsVector(Arc::new(crate::types::tsearch::tsvector_recv(
            take_bytes(b, pos)?,
        )?)),
        tag::TSQUERY => Value::TsQuery(Arc::new(crate::types::tsearch::tsquery_recv(take_bytes(
            b, pos,
        )?)?)),
        tag::GEO => {
            let k = take(b, pos, 1)?[0];
            let n = get_varint(b, pos)? as usize;
            let mut f = Vec::with_capacity(n);
            for _ in 0..n {
                f.push(le!(f64, 8));
            }
            let g = crate::types::geo::Geo::from_floats(k / 2, k % 2 == 1, &f)
                .ok_or_else(|| internal("bad geometric value"))?;
            Value::Geo(Arc::new(g))
        }
        tag::MONEY => Value::Money(le!(i64, 8)),
        tag::LSN => Value::PgLsn(le!(u64, 8)),
        tag::XID8 => Value::Xid8(le!(u64, 8)),
        tag::SNAPSHOT => {
            let s =
                std::str::from_utf8(take_bytes(b, pos)?).map_err(|_| internal("bad snapshot"))?;
            Value::Snapshot(Arc::new(crate::types::snapshot::Snapshot::parse(
                s,
                "pg_snapshot",
            )?))
        }
        tag::TID => {
            let blk = le!(u32, 4);
            Value::Tid(blk, le!(u16, 2))
        }
        // (Readers get rows with the out-of-line values they read put back in:
        // `storage::toast`. A pointer reaching here is a read path that skipped that.)
        tag::EXTERNAL => return Err(internal("an out-of-line value read without its row's storage")),
        other => return Err(internal(format!("unknown value tag {other}"))),
    })
}

pub fn encode_row(row: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + row.len() * 8);
    encode_row_into(&mut out, row);
    out
}

/// `encode_row` appended to `out`.
pub fn encode_row_into(out: &mut Vec<u8>, row: &[Value]) {
    put_varint(out, row.len() as u64);
    for v in row {
        encode_value(out, v);
    }
}

/// Decode a stored row. Rows written before columns were added are shorter; the caller fills
/// the gap with each column's missing value.
pub fn decode_row(b: &[u8]) -> Result<Vec<Value>> {
    let mut pos = 0;
    let n = get_varint(b, &mut pos)? as usize;
    let mut row = Vec::with_capacity(n);
    for _ in 0..n {
        row.push(decode_value(b, &mut pos)?);
    }
    Ok(row)
}

/// The encoded value at `pos` as an exact number, read in place (what `kernel::small_num`
/// gives its value): Some(None) a NULL, None anything but an integer or a finite numeric
/// that fits an i128.
pub fn peek_num(b: &[u8], pos: usize) -> Option<Option<(i128, u32)>> {
    let mut p = pos + 1;
    let le = |n: usize| b.get(pos + 1..pos + 1 + n);
    Some(Some(match *b.get(pos)? {
        tag::NULL => return Some(None),
        tag::INT2 => (i16::from_le_bytes(le(2)?.try_into().ok()?) as i128, 0),
        tag::INT4 => (i32::from_le_bytes(le(4)?.try_into().ok()?) as i128, 0),
        tag::INT8 => (i64::from_le_bytes(le(8)?.try_into().ok()?) as i128, 0),
        tag::NUMERIC => {
            let scale = get_varint(b, &mut p).ok()? as u32;
            let bytes = take_bytes(b, &mut p).ok()?;
            // (Two's complement, little-endian, as `to_signed_bytes_le` wrote it: none
            // longer than an i128's.)
            if bytes.is_empty() || bytes.len() > 16 {
                return if bytes.is_empty() { Some(Some((0, scale))) } else { None };
            }
            let fill = if bytes[bytes.len() - 1] & 0x80 != 0 { 0xFF } else { 0 };
            let mut w = [fill; 16];
            w[..bytes.len()].copy_from_slice(bytes);
            (i128::from_le_bytes(w), scale)
        }
        _ => return None,
    }))
}

/// The string value at `pos` as its stored bytes, in place (Some(None): NULL; None: another
/// value, one stored out of line among them).
#[inline]
pub fn peek_text(b: &[u8], pos: usize) -> Option<Option<&[u8]>> {
    match *b.get(pos)? {
        tag::NULL => Some(None),
        tag::TEXT => {
            let mut p = pos + 1;
            take_bytes(b, &mut p).ok().map(Some)
        }
        _ => None,
    }
}

/// A stored value as COPY TO writes it from its bytes (`read_stored`).
pub enum StoredValue<'a> {
    Null,
    /// An int2, int4 or int8.
    Int(i64),
    Float8(f64),
    /// A string's bytes.
    Text(&'a [u8]),
    /// Any other value (stepped over).
    Other,
}

/// The value at `*pos`, read in place, `pos` moved past it.
pub fn read_stored<'a>(b: &'a [u8], pos: &mut usize) -> Result<StoredValue<'a>> {
    let at = *pos;
    let le = |n: usize| b.get(at + 1..at + 1 + n);
    let v = match b.get(at) {
        Some(&tag::NULL) => StoredValue::Null,
        Some(&tag::INT2) => le(2).map_or(StoredValue::Other, |x| StoredValue::Int(i16::from_le_bytes(x.try_into().unwrap()) as i64)),
        Some(&tag::INT4) => le(4).map_or(StoredValue::Other, |x| StoredValue::Int(i32::from_le_bytes(x.try_into().unwrap()) as i64)),
        Some(&tag::INT8) => le(8).map_or(StoredValue::Other, |x| StoredValue::Int(i64::from_le_bytes(x.try_into().unwrap()))),
        Some(&tag::FLOAT8) => le(8).map_or(StoredValue::Other, |x| StoredValue::Float8(f64::from_le_bytes(x.try_into().unwrap()))),
        Some(&tag::TEXT) => {
            let mut p = at + 1;
            let s = take_bytes(b, &mut p)?;
            *pos = p;
            return Ok(StoredValue::Text(s));
        }
        _ => StoredValue::Other,
    };
    skip_value_fast(b, pos)?;
    Ok(v)
}

/// `skip_value` with the common fixed-size values stepped over in place.
#[inline]
pub fn skip_value_fast(b: &[u8], pos: &mut usize) -> Result<()> {
    let n = match b.get(*pos) {
        Some(&tag::NULL) => 0,
        Some(&tag::INT2) => 2,
        Some(&(tag::INT4 | tag::FLOAT4 | tag::DATE)) => 4,
        Some(&(tag::INT8 | tag::FLOAT8 | tag::TIMESTAMP | tag::TIMESTAMPTZ)) => 8,
        // (A string shorter than 128 bytes: its length in one byte.)
        Some(&tag::TEXT) if b.get(*pos + 1).is_some_and(|&l| l < 0x80) => 1 + b[*pos + 1] as usize,
        _ => return skip_value(b, pos),
    };
    if *pos + 1 + n > b.len() {
        return skip_value(b, pos);
    }
    *pos += 1 + n;
    Ok(())
}

/// Step over one encoded value without building it.
pub fn skip_value(b: &[u8], pos: &mut usize) -> Result<()> {
    let t = *b.get(*pos).ok_or_else(|| internal("truncated row"))?;
    let fixed = match t {
        tag::NULL | tag::FALSE | tag::TRUE | tag::NUM_NAN | tag::NUM_PINF | tag::NUM_NINF => 0,
        tag::CHAR => 1,
        tag::INT2 => 2,
        tag::INT4 | tag::FLOAT4 | tag::DATE | tag::OID => 4,
        tag::TID => 6,
        tag::MACADDR => 6,
        tag::INT8 | tag::FLOAT8 | tag::TIME | tag::TIMESTAMP | tag::TIMESTAMPTZ | tag::MONEY
        | tag::LSN | tag::XID8 | tag::MACADDR8 => 8,
        tag::TIMETZ => 12,
        tag::INTERVAL | tag::UUID => 16,
        tag::TEXT | tag::JSON | tag::JSONB | tag::BYTEA | tag::SNAPSHOT | tag::TSVECTOR
        | tag::TSQUERY => {
            *pos += 1;
            take_bytes(b, pos)?;
            return Ok(());
        }
        tag::NUMERIC | tag::NUMERIC_DEC => {
            *pos += 1;
            get_varint(b, pos)?;
            take_bytes(b, pos)?;
            return Ok(());
        }
        tag::EXT => {
            *pos += 1;
            take(b, pos, 2)?;
            take_bytes(b, pos)?;
            return Ok(());
        }
        tag::EXTERNAL => {
            *pos += 1;
            for _ in 0..3 {
                get_varint(b, pos)?;
            }
            return Ok(());
        }
        // (Its kind, then its floats.)
        tag::GEO => {
            *pos += 2;
            let n = get_varint(b, pos)? as usize;
            take(b, pos, 8 * n)?;
            return Ok(());
        }
        // (Its element type, dimensions, then its items.)
        tag::ARRAY => {
            *pos += 1;
            take(b, pos, 8)?;
            let nd = get_varint(b, pos)? as usize;
            for _ in 0..nd {
                get_varint(b, pos)?;
                take(b, pos, 4)?;
            }
            for _ in 0..get_varint(b, pos)? {
                skip_value_fast(b, pos)?;
            }
            return Ok(());
        }
        // (The rest: decoded and dropped.)
        _ => {
            decode_value(b, pos)?;
            return Ok(());
        }
    };
    take(b, pos, 1 + fixed)?;
    Ok(())
}

/// The value of column `col` of the encoded row `b` when it is an extension type's (its kind
/// and bytes, in place), else None.
pub fn ext_at(b: &[u8], col: usize) -> Option<(u16, &[u8])> {
    let mut pos = 0;
    let n = get_varint(b, &mut pos).ok()? as usize;
    if col >= n {
        return None;
    }
    for _ in 0..col {
        skip_value(b, &mut pos).ok()?;
    }
    ext_value(&b[pos..])
}

/// The encoded value `b` (one value, as `encode_value` writes it: an out-of-line value's
/// stored bytes) when it is an extension type's: its kind and bytes, in place.
pub fn ext_value(b: &[u8]) -> Option<(u16, &[u8])> {
    if *b.first()? != tag::EXT {
        return None;
    }
    let mut pos = 1;
    let k = u16::from_le_bytes(take(b, &mut pos, 2).ok()?.try_into().ok()?);
    Some((k, take_bytes(b, &mut pos).ok()?))
}

/// Whether column `col` of the encoded row `b` is stored out of line.
pub fn external_at(b: &[u8], col: usize) -> bool {
    let mut pos = 0;
    let Ok(n) = get_varint(b, &mut pos) else { return false };
    if col >= n as usize {
        return false;
    }
    for _ in 0..col {
        if skip_value(b, &mut pos).is_err() {
            return false;
        }
    }
    b.get(pos) == Some(&tag::EXTERNAL)
}

/// What `relsize::heap_tuple_len` gives the stored row `b` (its encoded values) taken as
/// `types`, the columns `null` stored as nulls (virtual generated ones), without decoding it.
/// None when a value's size needs it decoded: anything but nulls, booleans, fixed-size numbers,
/// dates and timestamps of the column's own type, ASCII text in a text, varchar or char(n)
/// column, jsonb, tsvectors, fixed-size geometric values and arrays of fixed-size elements;
/// and rows shorter than `types` (they get columns added since).
pub fn stored_tuple_len(b: &[u8], types: &[DataType], null: &[bool]) -> Option<usize> {
    use crate::catalog::relsize::{datum_len_at, tuple_header};
    let mut pos = 0;
    let n = get_varint(b, &mut pos).ok()? as usize;
    if n < types.len() {
        return None;
    }
    // (The values laid out after a header without a null bitmap, in one pass: a bitmap moves
    // them all by a multiple of MAXALIGN, which leaves every value's alignment as it is.)
    let mut off = tuple_header(types.len(), false);
    let mut has_null = false;
    // Each value: None a null, Some(None) fixed-length, Some(Some(payload)) text.
    let mut put = |v: Option<Option<usize>>, ty: &DataType| match v {
        Some(payload) => off += datum_len_at(off, ty, payload),
        None => has_null = true,
    };
    for (i, ty) in types.iter().enumerate() {
        let t = *b.get(pos)?;
        pos += 1;
        // (The encoded payload's size, for a value of the column's own type.)
        let fixed = |ok: bool, size: usize| ok.then_some(size);
        let skip = match ty {
            _ if t == tag::NULL => 0,
            DataType::Bool => fixed(t == tag::TRUE || t == tag::FALSE, 0)?,
            DataType::Int2 => fixed(t == tag::INT2, 2)?,
            DataType::Int4 | DataType::Date => fixed(t == tag::INT4 && matches!(ty, DataType::Int4) || t == tag::DATE && matches!(ty, DataType::Date), 4)?,
            DataType::Int8 => fixed(t == tag::INT8, 8)?,
            DataType::Float4 => fixed(t == tag::FLOAT4, 4)?,
            DataType::Float8 => fixed(t == tag::FLOAT8, 8)?,
            DataType::Timestamp(_) => fixed(t == tag::TIMESTAMP, 8)?,
            DataType::Timestamptz(_) => fixed(t == tag::TIMESTAMPTZ, 8)?,
            DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_) if t == tag::TEXT => {
                let len = get_varint(b, &mut pos).ok()? as usize;
                if !b.get(pos..pos + len)?.is_ascii() {
                    return None;
                }
                pos += len;
                // (A char(n)'s value is stored padded to n characters: `relsize::varlena_payload`.)
                let payload = match ty {
                    DataType::Bpchar(m) if *m > 4 => len.max((*m - 4) as usize),
                    _ => len,
                };
                put((!null.get(i).copied().unwrap_or(false)).then_some(Some(payload)), ty);
                continue;
            }
            // (Varlenas sized from their stored form: jsonb's text, tsvector's send format,
            // an array of fixed-size elements.)
            DataType::Jsonb | DataType::TsVector | DataType::Array(_) if matches!(t, tag::JSONB | tag::TSVECTOR | tag::ARRAY) => {
                let payload = match (ty, t) {
                    (DataType::Jsonb, tag::JSONB) => {
                        let len = get_varint(b, &mut pos).ok()? as usize;
                        let s = std::str::from_utf8(b.get(pos..pos + len)?).ok()?;
                        pos += len;
                        crate::catalog::relsize::jsonb_size(s) - 4
                    }
                    (DataType::TsVector, tag::TSVECTOR) => {
                        let len = get_varint(b, &mut pos).ok()? as usize;
                        let v = b.get(pos..pos + len)?;
                        pos += len;
                        tsvector_payload(v)?
                    }
                    (DataType::Array(e), tag::ARRAY) => stored_array_payload(b, &mut pos, e)?,
                    _ => return None,
                };
                put((!null.get(i).copied().unwrap_or(false)).then_some(Some(payload)), ty);
                continue;
            }
            // (A fixed-size geometric value: its kind, then its floats.)
            DataType::Point | DataType::Lseg | DataType::GBox | DataType::Line | DataType::Circle if t == tag::GEO => {
                pos += 1;
                8 * get_varint(b, &mut pos).ok()? as usize
            }
            _ => return None,
        };
        pos += skip;
        let stored = t != tag::NULL && !null.get(i).copied().unwrap_or(false);
        put(stored.then_some(None), ty);
    }
    if has_null {
        off += tuple_header(types.len(), true) - tuple_header(types.len(), false);
    }
    // (Counted in debug builds only: a parallel build's participants size rows side by side,
    // and one counter's cache line moving between them was half of this function's time.)
    if cfg!(debug_assertions) {
        STORED_SIZES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Some(off)
}

/// Rows `stored_tuple_len` sized, in debug builds (tests read it).
pub static STORED_SIZES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The stored bytes of a fixed-size value tagged `t` in a column of type `ty`, when the tag
/// is that type's own (None: anything else).
fn fixed_stored(ty: &DataType, t: u8) -> Option<usize> {
    match (ty, t) {
        (DataType::Bool, tag::TRUE | tag::FALSE) => Some(0),
        (DataType::Int2, tag::INT2) => Some(2),
        (DataType::Int4, tag::INT4) | (DataType::Date, tag::DATE) | (DataType::Float4, tag::FLOAT4) => Some(4),
        (DataType::Int8, tag::INT8)
        | (DataType::Float8, tag::FLOAT8)
        | (DataType::Timestamp(_), tag::TIMESTAMP)
        | (DataType::Timestamptz(_), tag::TIMESTAMPTZ) => Some(8),
        _ => None,
    }
}

/// `relsize::varlena_payload` of a tsvector stored in its send format `v`.
fn tsvector_payload(v: &[u8]) -> Option<usize> {
    let n = u32::from_be_bytes(v.get(..4)?.try_into().ok()?) as usize;
    let (mut pos, mut strlen) = (4, 0usize);
    for _ in 0..n {
        let word = v.get(pos..)?.iter().position(|&c| c == 0)?;
        strlen += word;
        pos += word + 1;
        let npos = u16::from_be_bytes(v.get(pos..pos + 2)?.try_into().ok()?) as usize;
        pos += 2 + 2 * npos;
        if npos > 0 {
            strlen = (strlen + 1) & !1;
            strlen += 2 + 2 * npos;
        }
    }
    Some(8 + 4 * n + strlen - 4)
}

/// `relsize::varlena_payload` of an array of `elem` stored at `pos` (past its tag), when its
/// elements are of a fixed size (None: anything else).
fn stored_array_payload(b: &[u8], pos: &mut usize, elem: &DataType) -> Option<usize> {
    use crate::catalog::relsize::type_layout;
    let (len, al) = type_layout(elem);
    if len <= 0 {
        return None;
    }
    // (The elements' own type: their tags are checked against the column's below.)
    decode_type(b, pos).ok()?;
    let nd = get_varint(b, pos).ok()? as usize;
    for _ in 0..nd {
        get_varint(b, pos).ok()?;
        *pos += 4;
    }
    let items = get_varint(b, pos).ok()? as usize;
    let mut present = 0usize;
    for _ in 0..items {
        let t = *b.get(*pos)?;
        *pos += 1;
        if t != tag::NULL {
            *pos += fixed_stored(elem, t)?;
            present += 1;
        }
    }
    let align = |n: usize, a: usize| n.div_ceil(a) * a;
    let nulls = if present < items { items.div_ceil(8) } else { 0 };
    let mut n = align(16 + 8 * nd + nulls, 8);
    for _ in 0..present {
        n = align(n, al) + len as usize;
    }
    Some(n - 4)
}

/// `decode_row` building only the values `need` marks (the others NULL; a value past the
/// mask's end isn't needed).
pub fn decode_row_need(b: &[u8], need: &[bool]) -> Result<Vec<Value>> {
    decode_row_need_spare(b, need, 0)
}

/// `decode_row_need` with room for `spare` more values after the row's.
pub fn decode_row_need_spare(b: &[u8], need: &[bool], spare: usize) -> Result<Vec<Value>> {
    let mut pos = 0;
    let n = get_varint(b, &mut pos)? as usize;
    let mut row = Vec::with_capacity(n.max(need.len()) + spare);
    // (The values after the last column needed aren't walked.)
    let last = need[..need.len().min(n)].iter().rposition(|&x| x).map_or(0, |i| i + 1);
    for &want in &need[..last] {
        if want {
            row.push(decode_value(b, &mut pos)?);
        } else {
            skip_value(b, &mut pos)?;
            row.push(Value::Null);
        }
    }
    row.resize(n, Value::Null);
    Ok(row)
}

/// The values of columns `cols` (ascending) of the encoded row `b`, in that order, pushed
/// to `out`, the values between stepped over and none after the last walked; those past the
/// row's own values aren't pushed. The row's count of values.
pub fn decode_cols(b: &[u8], cols: &[u32], out: &mut Vec<Value>) -> Result<usize> {
    let mut pos = 0;
    let n = get_varint(b, &mut pos)? as usize;
    let mut at = 0;
    for &c in cols {
        let c = c as usize;
        if c >= n {
            break;
        }
        while at < c {
            skip_value(b, &mut pos)?;
            at += 1;
        }
        out.push(decode_value(b, &mut pos)?);
        at += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row of jsonb, tsvector, array and point values is sized from its stored bytes as
    /// its decoded values size it.
    #[test]
    fn stored_sizes_of_varlenas() {
        use crate::types::geo::{Geo, parse_point};
        let types = [DataType::Jsonb, DataType::TsVector, DataType::Array(Box::new(DataType::Int4)), DataType::Point, DataType::Array(Box::new(DataType::Int8))];
        let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, &Default::default());
        let rows = [
            (r#"{"user": 7, "kind": "k3", "p12": true}"#, "'w12' 'x3' 'common'", vec![Some(1), Some(2), Some(1003)], "(3.5,7)"),
            ("[1, \"a\", {\"b\": null}]", "a:1,3 bb:2A cc", vec![None, Some(-4)], "(0,0)"),
            ("{}", "", vec![], "(-1e10,2)"),
        ];
        for (j, tsv, arr, p) in rows {
            let ints: Vec<Value> = arr.iter().map(|v| v.map_or(Value::Null, Value::Int4)).collect();
            let row = vec![
                Value::Jsonb(Arc::new(parse_jsonb(j).unwrap())),
                Value::TsVector(Arc::new(crate::types::tsearch::parse_tsvector(tsv).unwrap())),
                Value::Array(Arc::new(ArrayValue::from_vec(DataType::Int4, ints))),
                Value::Geo(Arc::new(Geo::Point(parse_point(p).unwrap()))),
                Value::Array(Arc::new(ArrayValue::from_vec(DataType::Int8, (0..40).map(Value::Int8).collect()))),
            ];
            let enc = encode_row(&row);
            let got = stored_tuple_len(&enc, &types, &[false; 5]);
            let want = crate::catalog::relsize::heap_tuple(&types, &row, &text);
            assert_eq!(got.map(crate::catalog::relsize::heap_tuple_of_len), Some(want), "{j} / {tsv} / {arr:?} / {p}");
            // (Each value stepped over: `skip_value`.)
            for i in 0..row.len() {
                let need: Vec<bool> = (0..row.len()).map(|c| c == i).collect();
                assert_eq!(decode_row_need(&enc, &need).unwrap()[i], row[i]);
            }
        }
    }

    #[test]
    fn roundtrip() {
        let row = vec![
            Value::Null,
            Value::Bool(true),
            Value::Int4(-7),
            Value::Float8(1.5),
            Value::numeric("-123.4500".parse().unwrap()),
            Value::text("héllo"),
            Value::Interval(Interval::new(1, 2, 3)),
            Value::Array(Arc::new(ArrayValue::from_vec(
                DataType::Int4,
                vec![Value::Int4(1), Value::Null],
            ))),
            Value::Jsonb(Arc::new(parse_jsonb(r#"{"a": [1, 2]}"#).unwrap())),
            Value::Uuid(0x1234),
            Value::Char(b'x'),
            Value::Date(5),
            Value::Timetz(1, -2),
            Value::Tid(3, 4),
            Value::bytea(vec![1u8, 2, 3]),
            Value::Int8(-9),
        ];
        let enc = encode_row(&row);
        let dec = decode_row(&enc).unwrap();
        assert_eq!(dec, row);
        // Any subset decoded, the rest skipped.
        for m in 0u32..(1 << row.len()) {
            let need: Vec<bool> = (0..row.len()).map(|i| m & (1 << i) != 0).collect();
            let part = decode_row_need(&enc, &need).unwrap();
            let cols: Vec<u32> = (0..row.len() as u32).filter(|&i| need[i as usize]).collect();
            let mut out = Vec::new();
            assert_eq!(decode_cols(&enc, &cols, &mut out).unwrap(), row.len());
            assert_eq!(out, cols.iter().map(|&i| row[i as usize].clone()).collect::<Vec<_>>());
            for (i, v) in part.iter().enumerate() {
                if need[i] {
                    assert_eq!(*v, row[i]);
                } else {
                    assert!(v.is_null());
                }
            }
        }
        if let Value::Numeric(n) = &dec[4] {
            assert_eq!(n.to_string(), "-123.4500");
        }
    }

    #[test]
    fn numerics_roundtrip() {
        for s in [
            "0", "1", "-1", "127", "128", "-128", "-129", "255", "256", "-32768", "0.01",
            "-0.01", "9223372036854775807", "-9223372036854775808", "9223372036854775808",
            "-9223372036854775809", "1180591620717411303424.5", "-12345678901234567890123.25",
        ] {
            let v = Value::numeric(s.parse().unwrap());
            let dec = decode_row(&encode_row(std::slice::from_ref(&v))).unwrap();
            assert_eq!(dec[0], v, "{s}");
            assert_eq!(format!("{:?}", dec[0]), format!("{v:?}"), "{s}");
        }
    }
}
