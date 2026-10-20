//! Values in Postgres's in-memory (Datum) representation, as serialized statistics hold them
//! (pg_mcv_list): fixed-length types as their raw bytes, varlena types as their data without
//! the length header. Types without a layout here are carried as their text form.

use super::numeric::Numeric;
use super::value::Interval;
use super::{DataType, Value};
use num_bigint::{BigInt, Sign};
use num_traits::Zero;
use std::sync::Arc;

/// How a type's values are laid out: its `typlen` and `typbyval`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub len: i16,
    pub byval: bool,
}

/// The layout of `ty`'s values (`None`: not modelled; such values travel as text).
pub fn layout(ty: &DataType) -> Option<Layout> {
    use DataType as T;
    let (len, byval) = match ty {
        T::Bool | T::Char => (1, true),
        T::Int2 => (2, true),
        T::Int4 | T::Oid | T::Xid | T::Cid | T::Date | T::Float4 | T::Enum(..) => (4, true),
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
        | T::Regcollation => (4, true),
        T::Int8
        | T::Float8
        | T::Money
        | T::Time(_)
        | T::Timestamp(_)
        | T::Timestamptz(_)
        | T::PgLsn
        | T::Xid8 => (8, true),
        T::Name => (64, false),
        T::Uuid | T::Interval(_) => (16, false),
        T::Timetz(_) => (12, false),
        T::Macaddr | T::Tid => (6, false),
        T::Macaddr8 => (8, false),
        T::Text
        | T::Varchar(_)
        | T::Bpchar(_)
        | T::Bytea
        | T::Json
        | T::Xml
        | T::Numeric(_)
        | T::Bit(_)
        | T::Varbit(_)
        | T::Inet
        | T::Cidr => (-1, false),
        _ => return None,
    };
    Some(Layout { len, byval })
}

/// `v` (of type `ty`) as Postgres holds it in memory: for varlena types, the data after the
/// header. `enum_oid` gives an enum label's pg_enum oid.
pub fn encode(v: &Value, ty: &DataType, enum_oid: &dyn Fn(u32, &str) -> u32) -> Vec<u8> {
    use DataType as T;
    let int = |n: i64, len: usize| n.to_le_bytes()[..len].to_vec();
    match (ty, v) {
        (T::Bool, Value::Bool(b)) => vec![*b as u8],
        (T::Char, Value::Char(c)) => vec![*c],
        (T::Float4, Value::Float4(f)) => f.to_bits().to_le_bytes().to_vec(),
        (T::Float8, Value::Float8(f)) => f.to_bits().to_le_bytes().to_vec(),
        (T::Enum(oid, ..), Value::Text(label)) => enum_oid(*oid, label).to_le_bytes().to_vec(),
        (_, Value::Date(d)) => d.to_le_bytes().to_vec(),
        (_, Value::Time(t) | Value::Timestamp(t) | Value::Timestamptz(t) | Value::Money(t)) => {
            t.to_le_bytes().to_vec()
        }
        (_, Value::PgLsn(x) | Value::Xid8(x)) => x.to_le_bytes().to_vec(),
        (T::Name, Value::Text(s)) => {
            let mut b = s.as_bytes().to_vec();
            b.resize(64, 0);
            b
        }
        (T::Uuid, Value::Uuid(u)) => u.to_be_bytes().to_vec(),
        (T::Interval(_), Value::Interval(i)) => [
            i.micros.to_le_bytes().as_slice(),
            &i.days.to_le_bytes(),
            &i.months.to_le_bytes(),
        ]
        .concat(),
        (T::Timetz(_), Value::Timetz(t, z)) => {
            [t.to_le_bytes().as_slice(), &z.to_le_bytes()].concat()
        }
        (T::Macaddr, Value::Macaddr(m)) => m.to_vec(),
        (T::Macaddr8, Value::Macaddr8(m)) => m.to_vec(),
        // ItemPointerData: the block number's high and low halves, then the offset.
        (T::Tid, Value::Tid(block, off)) => [
            ((block >> 16) as u16).to_le_bytes(),
            (*block as u16).to_le_bytes(),
            off.to_le_bytes(),
        ]
        .concat(),
        (T::Bytea, Value::Bytea(b)) => b.to_vec(),
        (T::Numeric(_), Value::Numeric(n)) => numeric_bytes(n),
        (T::Bit(_) | T::Varbit(_), Value::Bits(len, bytes)) => {
            [(*len as i32).to_le_bytes().as_slice(), bytes].concat()
        }
        // inet_struct: family (PGSQL_AF_INET 2, PGSQL_AF_INET6 3), mask bits, the address.
        (T::Inet | T::Cidr, Value::Inet(addr, bits)) => {
            [&[if addr.len() == 4 { 2 } else { 3 }, *bits][..], addr].concat()
        }
        (_, Value::Text(s) | Value::Json(s))
            if matches!(layout(ty), Some(Layout { len: -1, .. })) =>
        {
            s.as_bytes().to_vec()
        }
        _ => match (layout(ty), v.as_i64()) {
            (Some(Layout { byval: true, len }), Some(n)) => int(n, len as usize),
            _ => text_fallback(v, ty),
        },
    }
}

/// Values of types without a modelled layout (or of unexpected shapes): their text form.
fn text_fallback(v: &Value, ty: &DataType) -> Vec<u8> {
    super::text::format_value(v, ty, &super::text::FormatCtx::default()).into_bytes()
}

/// Read back a value `encode` wrote. `enum_label` gives a pg_enum oid's label.
pub fn decode(
    b: &[u8],
    ty: &DataType,
    enum_label: &dyn Fn(u32, u32) -> Option<String>,
) -> Option<Value> {
    use DataType as T;
    let i64_at = |b: &[u8], len: usize| -> i64 {
        let mut buf = [0u8; 8];
        buf[..len].copy_from_slice(&b[..len]);
        // Sign-extend from `len` bytes.
        let shift = 64 - 8 * len as u32;
        (i64::from_le_bytes(buf) << shift) >> shift
    };
    let text = || {
        std::str::from_utf8(b)
            .ok()
            .map(|s| Value::Text(Arc::from(s)))
    };
    Some(match ty {
        T::Bool => Value::Bool(b.first()? != &0),
        T::Char => Value::Char(*b.first()?),
        T::Int2 => Value::Int2(i64_at(b, 2) as i16),
        T::Int4 => Value::Int4(i64_at(b, 4) as i32),
        T::Int8 => Value::Int8(i64_at(b, 8)),
        T::Oid
        | T::Xid
        | T::Cid
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
        | T::Regcollation => Value::Oid(u32::from_le_bytes(b.get(..4)?.try_into().ok()?)),
        T::Date => Value::Date(i64_at(b, 4) as i32),
        T::Float4 => Value::Float4(f32::from_bits(u32::from_le_bytes(
            b.get(..4)?.try_into().ok()?,
        ))),
        T::Float8 => Value::Float8(f64::from_bits(u64::from_le_bytes(
            b.get(..8)?.try_into().ok()?,
        ))),
        T::Money => Value::Money(i64_at(b, 8)),
        T::Time(_) => Value::Time(i64_at(b, 8)),
        T::Timestamp(_) => Value::Timestamp(i64_at(b, 8)),
        T::Timestamptz(_) => Value::Timestamptz(i64_at(b, 8)),
        T::PgLsn => Value::PgLsn(i64_at(b, 8) as u64),
        T::Xid8 => Value::Xid8(i64_at(b, 8) as u64),
        T::Enum(oid, ..) => Value::Text(Arc::from(enum_label(
            *oid,
            u32::from_le_bytes(b.get(..4)?.try_into().ok()?),
        )?)),
        T::Name => Value::Text(Arc::from(
            std::str::from_utf8(&b[..b.iter().position(|c| *c == 0).unwrap_or(b.len())]).ok()?,
        )),
        T::Uuid => Value::Uuid(u128::from_be_bytes(b.get(..16)?.try_into().ok()?)),
        T::Interval(_) => Value::Interval(Interval {
            micros: i64_at(b, 8),
            days: i64_at(&b[8..], 4) as i32,
            months: i64_at(&b[12..], 4) as i32,
        }),
        T::Timetz(_) => Value::Timetz(i64_at(b, 8), i64_at(&b[8..], 4) as i32),
        T::Macaddr => Value::Macaddr(b.get(..6)?.try_into().ok()?),
        T::Macaddr8 => Value::Macaddr8(b.get(..8)?.try_into().ok()?),
        T::Tid => Value::Tid(
            (u16::from_le_bytes([b[0], b[1]]) as u32) << 16
                | u16::from_le_bytes([b[2], b[3]]) as u32,
            u16::from_le_bytes([b[4], b[5]]),
        ),
        T::Bytea => Value::Bytea(Arc::from(b)),
        T::Numeric(_) => Value::numeric(numeric_from_bytes(b)?),
        T::Bit(_) | T::Varbit(_) => Value::Bits(i64_at(b, 4) as u32, Arc::from(&b[4..])),
        T::Inet | T::Cidr => Value::Inet(Arc::from(&b[2..]), *b.get(1)?),
        T::Json => Value::Json(Arc::from(std::str::from_utf8(b).ok()?)),
        T::Text | T::Varchar(_) | T::Bpchar(_) | T::Xml => text()?,
        // Carried as text: parse it back.
        _ => super::text::parse_value(
            std::str::from_utf8(b).ok()?,
            ty,
            &super::text::FormatCtx::default(),
        )
        .ok()?,
    })
}

const NUMERIC_SHORT: u16 = 0x8000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_PINF: u16 = 0xD000;
const NUMERIC_NINF: u16 = 0xF000;
const NUMERIC_NEG: u16 = 0x4000;

/// A numeric's on-disk form (NumericShort when it fits, else NumericLong): base-10000 digits
/// without leading or trailing zero digits.
pub fn numeric_bytes(n: &Numeric) -> Vec<u8> {
    let (int, scale) = match n {
        Numeric::NaN => return NUMERIC_NAN.to_le_bytes().to_vec(),
        Numeric::PosInf => return NUMERIC_PINF.to_le_bytes().to_vec(),
        Numeric::NegInf => return NUMERIC_NINF.to_le_bytes().to_vec(),
        Numeric::Finite { int, scale } => (int, *scale),
    };
    let neg = int.sign() == Sign::Minus;
    let digits_str = int.magnitude().to_string();
    // Split at the decimal point, padded out to whole base-10000 digits on both sides.
    let (ip, fp) = if digits_str.len() as u32 > scale {
        let at = digits_str.len() - scale as usize;
        (digits_str[..at].to_string(), digits_str[at..].to_string())
    } else {
        (
            String::new(),
            format!(
                "{}{}",
                "0".repeat(scale as usize - digits_str.len()),
                digits_str
            ),
        )
    };
    let ip = format!("{}{}", "0".repeat((4 - ip.len() % 4) % 4), ip);
    let fp = format!("{}{}", fp, "0".repeat((4 - fp.len() % 4) % 4));
    let mut digits: Vec<i16> = ip
        .as_bytes()
        .chunks(4)
        .chain(fp.as_bytes().chunks(4))
        .map(|c| std::str::from_utf8(c).unwrap().parse().unwrap())
        .collect();
    let mut weight = ip.len() as i32 / 4 - 1;
    // Strip leading and trailing zeroes.
    let lead = digits.iter().take_while(|d| **d == 0).count();
    digits.drain(..lead);
    weight -= lead as i32;
    while digits.last() == Some(&0) {
        digits.pop();
    }
    let neg = neg && !digits.is_empty();
    if digits.is_empty() {
        weight = 0;
    }
    let mut out = Vec::new();
    if scale <= 0x3F && (-64..=63).contains(&weight) {
        let header = NUMERIC_SHORT
            | if neg { 0x2000 } else { 0 }
            | ((scale as u16) << 7)
            | if weight < 0 { 0x0040 } else { 0 }
            | (weight as u16 & 0x003F);
        out.extend(header.to_le_bytes());
    } else {
        out.extend((if neg { NUMERIC_NEG } else { 0 } | (scale as u16 & 0x3FFF)).to_le_bytes());
        out.extend((weight as i16).to_le_bytes());
    }
    for d in digits {
        out.extend(d.to_le_bytes());
    }
    out
}

/// Read back `numeric_bytes`.
pub fn numeric_from_bytes(b: &[u8]) -> Option<Numeric> {
    let header = u16::from_le_bytes(b.get(..2)?.try_into().ok()?);
    let (neg, scale, weight, rest) = match header & 0xC000 {
        0xC000 => {
            return Some(match header {
                NUMERIC_NAN => Numeric::NaN,
                NUMERIC_PINF => Numeric::PosInf,
                _ => Numeric::NegInf,
            });
        }
        0x8000 => {
            let w = (header & 0x003F) as i32;
            let weight = if header & 0x0040 != 0 { w - 64 } else { w };
            (
                header & 0x2000 != 0,
                ((header >> 7) & 0x3F) as u32,
                weight,
                &b[2..],
            )
        }
        _ => (
            header & NUMERIC_NEG != 0,
            (header & 0x3FFF) as u32,
            i16::from_le_bytes(b.get(2..4)?.try_into().ok()?) as i32,
            &b[4..],
        ),
    };
    let digits: Vec<i64> = rest
        .chunks(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as i64)
        .collect();
    // value = sum(d[i] * 10000^(weight - i)), as an integer scaled by 10^scale.
    let mut int = BigInt::zero();
    for d in &digits {
        int = int * 10000 + d;
    }
    // `int` is scaled by 10000^(ndigits - 1 - weight); rescale to 10^scale.
    let exp10 = 4 * (digits.len() as i64 - 1 - weight as i64);
    let shift = scale as i64 - exp10;
    let int = if shift >= 0 {
        int * BigInt::from(10).pow(shift as u32)
    } else {
        int / BigInt::from(10).pow((-shift) as u32)
    };
    Some(Numeric::from_parts(if neg { -int } else { int }, scale))
}
