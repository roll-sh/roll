//! The AMQP 1.0 type system (part 1 of the spec): primitive, compound and described values,
//! with the compact encodings on the way out and every encoding accepted on the way in.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::fmt;

/// An AMQP 1.0 value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Ubyte(u8),
    Ushort(u16),
    Uint(u32),
    Ulong(u64),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Decimal32([u8; 4]),
    Decimal64([u8; 8]),
    Decimal128([u8; 16]),
    /// A UTF-32 code point (kept raw: not every u32 is a `char`).
    Char(u32),
    /// Milliseconds since the Unix epoch.
    Timestamp(i64),
    Uuid([u8; 16]),
    Binary(Bytes),
    String(String),
    Symbol(String),
    List(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Array(Vec<Value>),
    Described(Box<Value>, Box<Value>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError(pub String);

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CodecError {}

pub type CodecResult<T> = Result<T, CodecError>;

fn err<T>(msg: impl Into<String>) -> CodecResult<T> {
    Err(CodecError(msg.into()))
}

const MAX_DEPTH: usize = 64;

impl Value {
    pub fn str(s: impl Into<String>) -> Self {
        Value::String(s.into())
    }

    pub fn sym(s: impl Into<String>) -> Self {
        Value::Symbol(s.into())
    }

    pub fn described(code: u64, v: Value) -> Self {
        Value::Described(Box::new(Value::Ulong(code)), Box::new(v))
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// String or symbol contents.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) | Value::Symbol(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Any integer type that fits a u64.
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Value::Ubyte(v) => Some(v as u64),
            Value::Ushort(v) => Some(v as u64),
            Value::Uint(v) => Some(v as u64),
            Value::Ulong(v) => Some(v),
            Value::Byte(v) => u64::try_from(v).ok(),
            Value::Short(v) => u64::try_from(v).ok(),
            Value::Int(v) => u64::try_from(v).ok(),
            Value::Long(v) => u64::try_from(v).ok(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Byte(v) => Some(v as i64),
            Value::Short(v) => Some(v as i64),
            Value::Int(v) => Some(v as i64),
            Value::Long(v) => Some(v),
            Value::Timestamp(v) => Some(v),
            _ => self.as_u64().and_then(|v| i64::try_from(v).ok()),
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        self.as_u64().and_then(|v| u32::try_from(v).ok())
    }

    /// The descriptor code of a described value (symbolic descriptors are resolved by the
    /// caller).
    pub fn descriptor(&self) -> Option<(&Value, &Value)> {
        match self {
            Value::Described(d, v) => Some((d, v)),
            _ => None,
        }
    }

    // ── encoding ──

    pub fn encode(&self, out: &mut BytesMut) {
        match self {
            Value::Null => out.put_u8(0x40),
            Value::Bool(true) => out.put_u8(0x41),
            Value::Bool(false) => out.put_u8(0x42),
            Value::Ubyte(v) => {
                out.put_u8(0x50);
                out.put_u8(*v);
            }
            Value::Ushort(v) => {
                out.put_u8(0x60);
                out.put_u16(*v);
            }
            Value::Uint(0) => out.put_u8(0x43),
            Value::Uint(v) if *v < 256 => {
                out.put_u8(0x52);
                out.put_u8(*v as u8);
            }
            Value::Uint(v) => {
                out.put_u8(0x70);
                out.put_u32(*v);
            }
            Value::Ulong(0) => out.put_u8(0x44),
            Value::Ulong(v) if *v < 256 => {
                out.put_u8(0x53);
                out.put_u8(*v as u8);
            }
            Value::Ulong(v) => {
                out.put_u8(0x80);
                out.put_u64(*v);
            }
            Value::Byte(v) => {
                out.put_u8(0x51);
                out.put_i8(*v);
            }
            Value::Short(v) => {
                out.put_u8(0x61);
                out.put_i16(*v);
            }
            Value::Int(v) if (-128..128).contains(v) => {
                out.put_u8(0x54);
                out.put_i8(*v as i8);
            }
            Value::Int(v) => {
                out.put_u8(0x71);
                out.put_i32(*v);
            }
            Value::Long(v) if (-128..128).contains(v) => {
                out.put_u8(0x55);
                out.put_i8(*v as i8);
            }
            Value::Long(v) => {
                out.put_u8(0x81);
                out.put_i64(*v);
            }
            Value::Described(d, v) => {
                out.put_u8(0x00);
                d.encode(out);
                v.encode(out);
            }
            Value::List(items) if items.is_empty() => out.put_u8(0x45),
            Value::Array(items) => encode_array(items, out),
            v => {
                out.put_u8(v.wide_code());
                v.encode_payload(out);
            }
        }
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut b = BytesMut::new();
        self.encode(&mut b);
        b.freeze()
    }

    /// The fixed-width constructor used inside arrays (and for types with one encoding).
    fn wide_code(&self) -> u8 {
        match self {
            Value::Null => 0x40,
            Value::Bool(_) => 0x56,
            Value::Ubyte(_) => 0x50,
            Value::Ushort(_) => 0x60,
            Value::Uint(_) => 0x70,
            Value::Ulong(_) => 0x80,
            Value::Byte(_) => 0x51,
            Value::Short(_) => 0x61,
            Value::Int(_) => 0x71,
            Value::Long(_) => 0x81,
            Value::Float(_) => 0x72,
            Value::Double(_) => 0x82,
            Value::Decimal32(_) => 0x74,
            Value::Decimal64(_) => 0x84,
            Value::Decimal128(_) => 0x94,
            Value::Char(_) => 0x73,
            Value::Timestamp(_) => 0x83,
            Value::Uuid(_) => 0x98,
            Value::Binary(b) if b.len() < 256 => 0xa0,
            Value::Binary(_) => 0xb0,
            Value::String(s) if s.len() < 256 => 0xa1,
            Value::String(_) => 0xb1,
            Value::Symbol(s) if s.len() < 256 => 0xa3,
            Value::Symbol(_) => 0xb3,
            Value::List(_) => 0xd0,
            Value::Map(_) => 0xd1,
            Value::Array(_) => 0xf0,
            Value::Described(..) => 0x00,
        }
    }

    /// The payload for constructor `code` (as chosen by `wide_code` or an array constructor).
    fn encode_payload_as(&self, code: u8, out: &mut BytesMut) {
        match (self, code) {
            (Value::Binary(b), 0xb0) => {
                out.put_u32(b.len() as u32);
                out.put_slice(b);
            }
            (Value::String(s), 0xb1) | (Value::Symbol(s), 0xb3) => {
                out.put_u32(s.len() as u32);
                out.put_slice(s.as_bytes());
            }
            _ => self.encode_payload(out),
        }
    }

    fn encode_payload(&self, out: &mut BytesMut) {
        match self {
            Value::Null => {}
            Value::Bool(b) => out.put_u8(*b as u8),
            Value::Ubyte(v) => out.put_u8(*v),
            Value::Ushort(v) => out.put_u16(*v),
            Value::Uint(v) => out.put_u32(*v),
            Value::Ulong(v) => out.put_u64(*v),
            Value::Byte(v) => out.put_i8(*v),
            Value::Short(v) => out.put_i16(*v),
            Value::Int(v) => out.put_i32(*v),
            Value::Long(v) => out.put_i64(*v),
            Value::Float(v) => out.put_f32(*v),
            Value::Double(v) => out.put_f64(*v),
            Value::Decimal32(b) => out.put_slice(b),
            Value::Decimal64(b) => out.put_slice(b),
            Value::Decimal128(b) => out.put_slice(b),
            Value::Char(c) => out.put_u32(*c),
            Value::Timestamp(v) => out.put_i64(*v),
            Value::Uuid(b) => out.put_slice(b),
            Value::Binary(b) => put_var(out, b),
            Value::String(s) | Value::Symbol(s) => put_var(out, s.as_bytes()),
            Value::List(items) => {
                let at = out.len();
                out.put_u32(0);
                out.put_u32(items.len() as u32);
                for v in items {
                    v.encode(out);
                }
                let n = (out.len() - at - 4) as u32;
                out[at..at + 4].copy_from_slice(&n.to_be_bytes());
            }
            Value::Map(pairs) => {
                let at = out.len();
                out.put_u32(0);
                out.put_u32(pairs.len() as u32 * 2);
                for (k, v) in pairs {
                    k.encode(out);
                    v.encode(out);
                }
                let n = (out.len() - at - 4) as u32;
                out[at..at + 4].copy_from_slice(&n.to_be_bytes());
            }
            Value::Array(items) => {
                // Only reached for nested arrays (constructor 0xf0).
                let mut inner = BytesMut::new();
                encode_array(items, &mut inner);
                // Re-frame the array8/array32 we just wrote as array32 payload.
                let mut b = inner.freeze();
                let code = b.get_u8();
                let (count, body) = if code == 0xe0 {
                    let _size = b.get_u8();
                    (b.get_u8() as u32, b)
                } else {
                    let _size = b.get_u32();
                    (b.get_u32(), b)
                };
                out.put_u32(body.len() as u32 + 4);
                out.put_u32(count);
                out.put_slice(&body);
            }
            Value::Described(d, v) => {
                d.encode(out);
                v.encode(out);
            }
        }
    }
}

fn put_var(out: &mut BytesMut, b: &[u8]) {
    if b.len() < 256 {
        out.put_u8(b.len() as u8);
    } else {
        out.put_u32(b.len() as u32);
    }
    out.put_slice(b);
}

fn encode_array(items: &[Value], out: &mut BytesMut) {
    // Variable-width element types use the 32-bit form unless every element fits 8 bits.
    let code = match items.first() {
        None => 0xa3,
        Some(first) => {
            let mut c = first.wide_code();
            if matches!(c, 0xa0 | 0xa1 | 0xa3) && items.iter().any(|v| v.wide_code() != c) {
                c += 0x10;
            }
            c
        }
    };
    let mut body = BytesMut::new();
    if code == 0x00 {
        // Described elements share the first element's descriptor and inner constructor.
        if let Some(Value::Described(d, v)) = items.first() {
            body.put_u8(0x00);
            d.encode(&mut body);
            let inner = v.wide_code();
            body.put_u8(inner);
            for it in items {
                if let Value::Described(_, v) = it {
                    v.encode_payload_as(inner, &mut body);
                }
            }
        }
    } else {
        body.put_u8(code);
        for v in items {
            v.encode_payload_as(code, &mut body);
        }
    }
    if body.len() + 1 < 256 && items.len() < 256 {
        out.put_u8(0xe0);
        out.put_u8((body.len() + 1) as u8);
        out.put_u8(items.len() as u8);
    } else {
        out.put_u8(0xf0);
        out.put_u32((body.len() + 4) as u32);
        out.put_u32(items.len() as u32);
    }
    out.put_slice(&body);
}

// ── decoding ──

fn need(buf: &Bytes, n: usize) -> CodecResult<()> {
    if buf.remaining() < n { err("truncated AMQP value") } else { Ok(()) }
}

fn take(buf: &mut Bytes, n: usize) -> CodecResult<Bytes> {
    need(buf, n)?;
    Ok(buf.split_to(n))
}

fn utf8(b: Bytes) -> CodecResult<String> {
    String::from_utf8(b.to_vec()).map_err(|_| CodecError("invalid UTF-8 in string".into()))
}

impl Value {
    /// Decode one value from the front of `buf`.
    pub fn decode(buf: &mut Bytes) -> CodecResult<Value> {
        decode_value(buf, 0)
    }
}

fn decode_value(buf: &mut Bytes, depth: usize) -> CodecResult<Value> {
    need(buf, 1)?;
    let code = buf.get_u8();
    if code == 0x00 {
        if depth > MAX_DEPTH {
            return err("AMQP value nested too deeply");
        }
        let d = decode_value(buf, depth + 1)?;
        let v = decode_value(buf, depth + 1)?;
        return Ok(Value::Described(Box::new(d), Box::new(v)));
    }
    decode_with(code, buf, depth)
}

fn decode_with(code: u8, buf: &mut Bytes, depth: usize) -> CodecResult<Value> {
    if depth > MAX_DEPTH {
        return err("AMQP value nested too deeply");
    }
    macro_rules! fixed {
        ($n:expr, $e:expr) => {{
            need(buf, $n)?;
            $e
        }};
    }
    Ok(match code {
        0x40 => Value::Null,
        0x41 => Value::Bool(true),
        0x42 => Value::Bool(false),
        0x56 => fixed!(1, Value::Bool(buf.get_u8() != 0)),
        0x50 => fixed!(1, Value::Ubyte(buf.get_u8())),
        0x60 => fixed!(2, Value::Ushort(buf.get_u16())),
        0x70 => fixed!(4, Value::Uint(buf.get_u32())),
        0x52 => fixed!(1, Value::Uint(buf.get_u8() as u32)),
        0x43 => Value::Uint(0),
        0x80 => fixed!(8, Value::Ulong(buf.get_u64())),
        0x53 => fixed!(1, Value::Ulong(buf.get_u8() as u64)),
        0x44 => Value::Ulong(0),
        0x51 => fixed!(1, Value::Byte(buf.get_i8())),
        0x61 => fixed!(2, Value::Short(buf.get_i16())),
        0x71 => fixed!(4, Value::Int(buf.get_i32())),
        0x54 => fixed!(1, Value::Int(buf.get_i8() as i32)),
        0x81 => fixed!(8, Value::Long(buf.get_i64())),
        0x55 => fixed!(1, Value::Long(buf.get_i8() as i64)),
        0x72 => fixed!(4, Value::Float(buf.get_f32())),
        0x82 => fixed!(8, Value::Double(buf.get_f64())),
        0x74 => {
            let mut b = [0u8; 4];
            take(buf, 4)?.copy_to_slice(&mut b);
            Value::Decimal32(b)
        }
        0x84 => {
            let mut b = [0u8; 8];
            take(buf, 8)?.copy_to_slice(&mut b);
            Value::Decimal64(b)
        }
        0x94 => {
            let mut b = [0u8; 16];
            take(buf, 16)?.copy_to_slice(&mut b);
            Value::Decimal128(b)
        }
        0x73 => fixed!(4, Value::Char(buf.get_u32())),
        0x83 => fixed!(8, Value::Timestamp(buf.get_i64())),
        0x98 => {
            let mut b = [0u8; 16];
            take(buf, 16)?.copy_to_slice(&mut b);
            Value::Uuid(b)
        }
        0xa0 => {
            let n = fixed!(1, buf.get_u8() as usize);
            Value::Binary(take(buf, n)?)
        }
        0xb0 => {
            let n = fixed!(4, buf.get_u32() as usize);
            Value::Binary(take(buf, n)?)
        }
        0xa1 => {
            let n = fixed!(1, buf.get_u8() as usize);
            Value::String(utf8(take(buf, n)?)?)
        }
        0xb1 => {
            let n = fixed!(4, buf.get_u32() as usize);
            Value::String(utf8(take(buf, n)?)?)
        }
        0xa3 => {
            let n = fixed!(1, buf.get_u8() as usize);
            Value::Symbol(utf8(take(buf, n)?)?)
        }
        0xb3 => {
            let n = fixed!(4, buf.get_u32() as usize);
            Value::Symbol(utf8(take(buf, n)?)?)
        }
        0x45 => Value::List(vec![]),
        0xc0 | 0xd0 | 0xc1 | 0xd1 => {
            let wide = code & 0x10 != 0;
            let (size, count) = if wide {
                need(buf, 8)?;
                let s = buf.get_u32() as usize;
                (s.checked_sub(4).ok_or(CodecError("bad compound size".into()))?, buf.get_u32() as usize)
            } else {
                need(buf, 2)?;
                let s = buf.get_u8() as usize;
                (s.checked_sub(1).ok_or(CodecError("bad compound size".into()))?, buf.get_u8() as usize)
            };
            let mut body = take(buf, size)?;
            if count > body.len() {
                return err("compound count exceeds its size");
            }
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(decode_value(&mut body, depth + 1)?);
            }
            if code & 0x01 == 0 {
                Value::List(items)
            } else {
                if count % 2 != 0 {
                    return err("map with an odd number of elements");
                }
                let mut pairs = Vec::with_capacity(count / 2);
                let mut it = items.into_iter();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    pairs.push((k, v));
                }
                Value::Map(pairs)
            }
        }
        0xe0 | 0xf0 => {
            let (size, count) = if code == 0xf0 {
                need(buf, 8)?;
                let s = buf.get_u32() as usize;
                (s.checked_sub(4).ok_or(CodecError("bad array size".into()))?, buf.get_u32() as usize)
            } else {
                need(buf, 2)?;
                let s = buf.get_u8() as usize;
                (s.checked_sub(1).ok_or(CodecError("bad array size".into()))?, buf.get_u8() as usize)
            };
            let mut body = take(buf, size)?;
            need(&body, 1)?;
            let mut ctor = body.get_u8();
            let mut descriptor = None;
            if ctor == 0x00 {
                descriptor = Some(decode_value(&mut body, depth + 1)?);
                need(&body, 1)?;
                ctor = body.get_u8();
            }
            let zero_width = matches!(ctor, 0x40..=0x45);
            if (zero_width && count > 1 << 20) || (!zero_width && count > body.len()) {
                return err("array count exceeds its size");
            }
            let mut items = Vec::with_capacity(count.min(4096));
            for _ in 0..count {
                let v = decode_with(ctor, &mut body, depth + 1)?;
                items.push(match &descriptor {
                    Some(d) => Value::Described(Box::new(d.clone()), Box::new(v)),
                    None => v,
                });
            }
            Value::Array(items)
        }
        c => return err(format!("unknown AMQP type constructor 0x{c:02x}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(v: Value) {
        let mut b = v.to_bytes();
        assert_eq!(Value::decode(&mut b).unwrap(), v);
        assert!(b.is_empty());
    }

    #[test]
    fn roundtrip_primitives() {
        for v in [
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::Ubyte(7),
            Value::Ushort(300),
            Value::Uint(0),
            Value::Uint(200),
            Value::Uint(70_000),
            Value::Ulong(0),
            Value::Ulong(9),
            Value::Ulong(1 << 40),
            Value::Byte(-3),
            Value::Short(-300),
            Value::Int(-5),
            Value::Int(1 << 20),
            Value::Long(-1),
            Value::Long(1 << 40),
            Value::Float(1.5),
            Value::Double(-2.25),
            Value::Char('x' as u32),
            Value::Timestamp(1_700_000_000_000),
            Value::Uuid([7; 16]),
            Value::Binary(Bytes::from_static(b"\x00\x01")),
            Value::Binary(Bytes::from(vec![1u8; 300])),
            Value::str("héllo"),
            Value::str("x".repeat(400)),
            Value::sym("amqp:accepted:list"),
        ] {
            rt(v);
        }
    }

    #[test]
    fn roundtrip_compound() {
        rt(Value::List(vec![]));
        rt(Value::List(vec![Value::Uint(1), Value::str("a"), Value::Null]));
        rt(Value::Map(vec![(Value::sym("k"), Value::Long(5)), (Value::str("s"), Value::List(vec![Value::Bool(true)]))]));
        rt(Value::Array(vec![Value::sym("a"), Value::sym("bb")]));
        rt(Value::Array(vec![Value::sym("a"), Value::sym("b".repeat(300))]));
        rt(Value::Array(vec![Value::Uint(1), Value::Uint(1 << 30)]));
        rt(Value::Array(vec![Value::Map(vec![(Value::sym("q"), Value::str("x"))])]));
        rt(Value::Array(vec![Value::described(0x24, Value::List(vec![])), Value::described(0x24, Value::List(vec![]))]));
        rt(Value::described(0x10, Value::List(vec![Value::str("c"), Value::Null, Value::Uint(65536)])));
        rt(Value::List((0..300).map(Value::Int).collect()));
    }

    #[test]
    fn decodes_long_forms() {
        // list32 with a uint0 and a str32.
        let mut b = BytesMut::new();
        b.put_u8(0xd0);
        b.put_u32(4 + 1 + 1 + 4 + 2);
        b.put_u32(2);
        b.put_u8(0x43);
        b.put_u8(0xb1);
        b.put_u32(2);
        b.put_slice(b"hi");
        assert_eq!(Value::decode(&mut b.freeze()).unwrap(), Value::List(vec![Value::Uint(0), Value::str("hi")]));
    }

    #[test]
    fn rejects_garbage() {
        for bad in [&[0xa1u8, 10, b'a'][..], &[0xff], &[0xd0, 0, 0, 0, 1, 0, 0, 0, 9], &[0x00], &[0xc1, 3, 1, 0x40, 0x40]] {
            assert!(Value::decode(&mut Bytes::copy_from_slice(bad)).is_err(), "{bad:?}");
        }
        let mut deep = vec![0x00u8; 200];
        deep.push(0x40);
        assert!(Value::decode(&mut Bytes::from(deep)).is_err());
    }
}
