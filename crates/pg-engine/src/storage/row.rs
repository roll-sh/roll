//! Row encoding: a self-describing binary format (tag + payload per value).

use super::sst::{get_varint, put_varint};
use crate::error::{Result, internal};
use crate::types::array::ArrayValue;
use crate::types::json::parse_jsonb;
use crate::types::numeric::Numeric;
use crate::types::{DataType, Interval, Value};
use num_bigint::BigInt;
use std::sync::Arc;

mod tag {
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
}

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
            Numeric::Finite { int, scale } => {
                out.push(tag::NUMERIC);
                put_varint(out, *scale as u64);
                put_bytes(out, &int.to_signed_bytes_le());
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
                BigInt::from(i64::from_le_bytes(w))
            } else {
                BigInt::from_signed_bytes_le(bytes)
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
        tag::NUMERIC => {
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
    if *b.get(pos)? != tag::EXT {
        return None;
    }
    pos += 1;
    let k = u16::from_le_bytes(take(b, &mut pos, 2).ok()?.try_into().ok()?);
    Some((k, take_bytes(b, &mut pos).ok()?))
}

/// What `relsize::heap_tuple_len` gives the stored row `b` (its encoded values) taken as
/// `types`, the columns `null` stored as nulls (virtual generated ones), without decoding it.
/// None when a value's size needs it decoded: anything but nulls, booleans, fixed-size numbers,
/// dates and timestamps of the column's own type and ASCII text in a text or varchar column;
/// and rows shorter than `types` (they get columns added since).
pub fn stored_tuple_len(b: &[u8], types: &[DataType], null: &[bool]) -> Option<usize> {
    use crate::catalog::relsize::{datum_len_at, tuple_header};
    let mut pos = 0;
    let n = get_varint(b, &mut pos).ok()? as usize;
    if n < types.len() {
        return None;
    }
    // Each value: None a null, Some(None) fixed-length, Some(Some(payload)) text.
    let mut vals: Vec<Option<Option<usize>>> = Vec::with_capacity(types.len());
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
            DataType::Text | DataType::Varchar(_) if t == tag::TEXT => {
                let len = get_varint(b, &mut pos).ok()? as usize;
                if !b.get(pos..pos + len)?.is_ascii() {
                    return None;
                }
                pos += len;
                vals.push((!null.get(i).copied().unwrap_or(false)).then_some(Some(len)));
                continue;
            }
            _ => return None,
        };
        pos += skip;
        let stored = t != tag::NULL && !null.get(i).copied().unwrap_or(false);
        vals.push(stored.then_some(None));
    }
    let mut off = tuple_header(types.len(), vals.iter().any(Option::is_none));
    for (v, ty) in vals.iter().zip(types) {
        if let Some(payload) = v {
            off += datum_len_at(off, ty, *payload);
        }
    }
    STORED_SIZES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(off)
}

/// Rows `stored_tuple_len` sized (tests read it).
pub static STORED_SIZES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `decode_row` building only the values `need` marks (the others NULL; a value past the
/// mask's end isn't needed).
pub fn decode_row_need(b: &[u8], need: &[bool]) -> Result<Vec<Value>> {
    decode_row_need_spare(b, need, 0)
}

/// `decode_row_need` with room for `spare` more values after the row's.
pub fn decode_row_need_spare(b: &[u8], need: &[bool], spare: usize) -> Result<Vec<Value>> {
    let mut pos = 0;
    let n = get_varint(b, &mut pos)? as usize;
    let mut row = Vec::with_capacity(n + spare);
    for i in 0..n {
        if need.get(i).copied().unwrap_or(false) {
            row.push(decode_value(b, &mut pos)?);
        } else {
            skip_value(b, &mut pos)?;
            row.push(Value::Null);
        }
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

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
