//! AMQP 0-9-1 field values and tables (RabbitMQ's dialect of the 0-9-1 errata), with the wire
//! codec shared by the AMQP server, the storage format and the management API's JSON views.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde_json::{Map, Number, Value};
use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum FieldValue {
    Bool(bool),
    I8(i8),
    U8(u8),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    F32(f32),
    F64(f64),
    Decimal(u8, u32),
    /// Short strings only appear inside tables from some clients; we decode them but always
    /// encode strings as long strings, like RabbitMQ.
    ShortStr(String),
    LongStr(Bytes),
    Timestamp(u64),
    Table(FieldTable),
    Array(Vec<FieldValue>),
    Bytes(Bytes),
    Void,
}

/// An ordered AMQP table. Lookups are linear, which beats hashing for the handful of entries
/// real messages carry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FieldTable(pub Vec<(String, FieldValue)>);

#[derive(Debug, thiserror::Error)]
#[error("malformed frame: {0}")]
pub struct DecodeError(pub &'static str);

pub type DecodeResult<T> = Result<T, DecodeError>;

impl FieldValue {
    pub fn str(s: impl Into<String>) -> Self {
        FieldValue::LongStr(Bytes::from(s.into()))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            FieldValue::LongStr(b) | FieldValue::Bytes(b) => std::str::from_utf8(b).ok(),
            FieldValue::ShortStr(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        Some(match *self {
            FieldValue::I8(v) => v as i64,
            FieldValue::U8(v) => v as i64,
            FieldValue::I16(v) => v as i64,
            FieldValue::U16(v) => v as i64,
            FieldValue::I32(v) => v as i64,
            FieldValue::U32(v) => v as i64,
            FieldValue::I64(v) => v,
            FieldValue::Timestamp(v) => v as i64,
            FieldValue::F32(v) if v.fract() == 0.0 => v as i64,
            FieldValue::F64(v) if v.fract() == 0.0 => v as i64,
            // Some clients send numeric arguments as strings ("x-max-length": "100").
            FieldValue::LongStr(_) | FieldValue::ShortStr(_) => return self.as_str()?.trim().parse().ok(),
            _ => return None,
        })
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            FieldValue::Bool(b) => Some(*b),
            FieldValue::LongStr(_) | FieldValue::ShortStr(_) => match self.as_str()? {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            v => v.as_i64().map(|n| n != 0),
        }
    }

    pub fn as_table(&self) -> Option<&FieldTable> {
        match self {
            FieldValue::Table(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[FieldValue]> {
        match self {
            FieldValue::Array(a) => Some(a),
            _ => None,
        }
    }

    /// RabbitMQ's equivalence check for redeclarations: numbers compare by value whatever their
    /// width, strings by content.
    pub fn equivalent(&self, other: &FieldValue) -> bool {
        match (self.as_i64(), other.as_i64()) {
            (Some(a), Some(b)) if !matches!(self, FieldValue::LongStr(_) | FieldValue::ShortStr(_)) => a == b,
            _ => match (self.as_str(), other.as_str()) {
                (Some(a), Some(b)) => a == b,
                _ => self == other,
            },
        }
    }

    fn tag(&self) -> u8 {
        match self {
            FieldValue::Bool(_) => b't',
            FieldValue::I8(_) => b'b',
            FieldValue::U8(_) => b'B',
            FieldValue::I16(_) => b's',
            FieldValue::U16(_) => b'u',
            FieldValue::I32(_) => b'I',
            FieldValue::U32(_) => b'i',
            FieldValue::I64(_) => b'l',
            FieldValue::F32(_) => b'f',
            FieldValue::F64(_) => b'd',
            FieldValue::Decimal(..) => b'D',
            FieldValue::ShortStr(_) | FieldValue::LongStr(_) => b'S',
            FieldValue::Timestamp(_) => b'T',
            FieldValue::Table(_) => b'F',
            FieldValue::Array(_) => b'A',
            FieldValue::Bytes(_) => b'x',
            FieldValue::Void => b'V',
        }
    }

    pub fn encode(&self, out: &mut BytesMut) {
        out.put_u8(self.tag());
        match self {
            FieldValue::Bool(b) => out.put_u8(*b as u8),
            FieldValue::I8(v) => out.put_i8(*v),
            FieldValue::U8(v) => out.put_u8(*v),
            FieldValue::I16(v) => out.put_i16(*v),
            FieldValue::U16(v) => out.put_u16(*v),
            FieldValue::I32(v) => out.put_i32(*v),
            FieldValue::U32(v) => out.put_u32(*v),
            FieldValue::I64(v) => out.put_i64(*v),
            FieldValue::F32(v) => out.put_f32(*v),
            FieldValue::F64(v) => out.put_f64(*v),
            FieldValue::Decimal(s, v) => {
                out.put_u8(*s);
                out.put_u32(*v);
            }
            FieldValue::ShortStr(s) => put_longstr(out, s.as_bytes()),
            FieldValue::LongStr(b) | FieldValue::Bytes(b) => put_longstr(out, b),
            FieldValue::Timestamp(v) => out.put_u64(*v),
            FieldValue::Table(t) => t.encode(out),
            FieldValue::Array(a) => {
                let at = out.len();
                out.put_u32(0);
                for v in a {
                    v.encode(out);
                }
                let n = (out.len() - at - 4) as u32;
                out[at..at + 4].copy_from_slice(&n.to_be_bytes());
            }
            FieldValue::Void => {}
        }
    }

    pub fn decode(buf: &mut Bytes) -> DecodeResult<Self> {
        let tag = get_u8(buf)?;
        Ok(match tag {
            b't' => FieldValue::Bool(get_u8(buf)? != 0),
            b'b' => FieldValue::I8(get_u8(buf)? as i8),
            b'B' => FieldValue::U8(get_u8(buf)?),
            b's' => FieldValue::I16(need(buf, 2)?.get_i16()),
            b'u' => FieldValue::U16(need(buf, 2)?.get_u16()),
            b'I' => FieldValue::I32(need(buf, 4)?.get_i32()),
            b'i' => FieldValue::U32(need(buf, 4)?.get_u32()),
            b'l' | b'L' => FieldValue::I64(need(buf, 8)?.get_i64()),
            b'f' => FieldValue::F32(need(buf, 4)?.get_f32()),
            b'd' => FieldValue::F64(need(buf, 8)?.get_f64()),
            b'D' => {
                let s = get_u8(buf)?;
                FieldValue::Decimal(s, need(buf, 4)?.get_u32())
            }
            b'S' => FieldValue::LongStr(get_longstr(buf)?),
            b'T' => FieldValue::Timestamp(need(buf, 8)?.get_u64()),
            b'F' => FieldValue::Table(FieldTable::decode(buf)?),
            b'A' => {
                let n = need(buf, 4)?.get_u32() as usize;
                let mut inner = take(buf, n)?;
                let mut items = Vec::new();
                while inner.has_remaining() {
                    items.push(FieldValue::decode(&mut inner)?);
                }
                FieldValue::Array(items)
            }
            b'x' => FieldValue::Bytes(get_longstr(buf)?),
            b'V' => FieldValue::Void,
            _ => return Err(DecodeError("unknown field type")),
        })
    }

    /// JSON view, as the management API renders values.
    pub fn to_json(&self) -> Value {
        match self {
            FieldValue::Bool(b) => Value::Bool(*b),
            FieldValue::F32(v) => Number::from_f64(*v as f64).map(Value::Number).unwrap_or(Value::Null),
            FieldValue::F64(v) => Number::from_f64(*v).map(Value::Number).unwrap_or(Value::Null),
            FieldValue::Decimal(s, v) => Number::from_f64(*v as f64 / 10f64.powi(*s as i32)).map(Value::Number).unwrap_or(Value::Null),
            FieldValue::ShortStr(s) => Value::String(s.clone()),
            FieldValue::LongStr(b) | FieldValue::Bytes(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
            FieldValue::Table(t) => t.to_json(),
            FieldValue::Array(a) => Value::Array(a.iter().map(|v| v.to_json()).collect()),
            FieldValue::Void => Value::Null,
            v => Value::Number(v.as_i64().unwrap_or(0).into()),
        }
    }

    /// From management-API JSON (numbers become signed longs, like RabbitMQ does).
    pub fn from_json(v: &Value) -> Self {
        match v {
            Value::Null => FieldValue::Void,
            Value::Bool(b) => FieldValue::Bool(*b),
            Value::Number(n) => match n.as_i64() {
                Some(i) => FieldValue::I64(i),
                None => FieldValue::F64(n.as_f64().unwrap_or(0.0)),
            },
            Value::String(s) => FieldValue::str(s.clone()),
            Value::Array(a) => FieldValue::Array(a.iter().map(FieldValue::from_json).collect()),
            Value::Object(o) => FieldValue::Table(FieldTable::from_json_map(o)),
        }
    }
}

impl From<&str> for FieldValue {
    fn from(s: &str) -> Self {
        FieldValue::str(s)
    }
}
impl From<String> for FieldValue {
    fn from(s: String) -> Self {
        FieldValue::str(s)
    }
}
impl From<i64> for FieldValue {
    fn from(v: i64) -> Self {
        FieldValue::I64(v)
    }
}
impl From<bool> for FieldValue {
    fn from(v: bool) -> Self {
        FieldValue::Bool(v)
    }
}
impl From<FieldTable> for FieldValue {
    fn from(v: FieldTable) -> Self {
        FieldValue::Table(v)
    }
}

impl FieldTable {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self, key: &str) -> Option<&FieldValue> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut FieldValue> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(|v| v.as_str())
    }
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| v.as_i64())
    }
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(|v| v.as_bool())
    }
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<FieldValue>) {
        let key = key.into();
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }
    pub fn remove(&mut self, key: &str) -> Option<FieldValue> {
        let i = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(i).1)
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &FieldValue)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Same keys with equivalent values, ignoring order.
    pub fn equivalent(&self, other: &FieldTable) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k).is_some_and(|o| v.equivalent(o)))
    }

    pub fn encode(&self, out: &mut BytesMut) {
        let at = out.len();
        out.put_u32(0);
        for (k, v) in &self.0 {
            put_shortstr(out, k);
            v.encode(out);
        }
        let n = (out.len() - at - 4) as u32;
        out[at..at + 4].copy_from_slice(&n.to_be_bytes());
    }

    pub fn decode(buf: &mut Bytes) -> DecodeResult<Self> {
        let n = need(buf, 4)?.get_u32() as usize;
        let mut inner = take(buf, n)?;
        let mut out = Vec::new();
        while inner.has_remaining() {
            let k = get_shortstr(&mut inner)?;
            let v = FieldValue::decode(&mut inner)?;
            out.push((k, v));
        }
        Ok(FieldTable(out))
    }

    pub fn encoded_len(&self) -> usize {
        let mut b = BytesMut::new();
        self.encode(&mut b);
        b.len()
    }

    pub fn to_json(&self) -> Value {
        Value::Object(self.0.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
    }

    pub fn from_json(v: &Value) -> Self {
        match v {
            Value::Object(o) => Self::from_json_map(o),
            _ => Self::default(),
        }
    }

    pub fn from_json_map(o: &Map<String, Value>) -> Self {
        FieldTable(o.iter().map(|(k, v)| (k.clone(), FieldValue::from_json(v))).collect())
    }
}

impl fmt::Display for FieldTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_json())
    }
}

impl<K: Into<String>, V: Into<FieldValue>> FromIterator<(K, V)> for FieldTable {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        FieldTable(iter.into_iter().map(|(k, v)| (k.into(), v.into())).collect())
    }
}

// ── primitives ──

pub fn need(buf: &mut Bytes, n: usize) -> DecodeResult<&mut Bytes> {
    if buf.remaining() < n { Err(DecodeError("truncated")) } else { Ok(buf) }
}

pub fn take(buf: &mut Bytes, n: usize) -> DecodeResult<Bytes> {
    if buf.remaining() < n {
        return Err(DecodeError("truncated"));
    }
    Ok(buf.split_to(n))
}

pub fn get_u8(buf: &mut Bytes) -> DecodeResult<u8> {
    Ok(need(buf, 1)?.get_u8())
}
pub fn get_u16(buf: &mut Bytes) -> DecodeResult<u16> {
    Ok(need(buf, 2)?.get_u16())
}
pub fn get_u32(buf: &mut Bytes) -> DecodeResult<u32> {
    Ok(need(buf, 4)?.get_u32())
}
pub fn get_u64(buf: &mut Bytes) -> DecodeResult<u64> {
    Ok(need(buf, 8)?.get_u64())
}

pub fn get_shortstr(buf: &mut Bytes) -> DecodeResult<String> {
    let n = get_u8(buf)? as usize;
    let b = take(buf, n)?;
    // RabbitMQ accepts any bytes; be lenient with clients that send Latin-1.
    Ok(String::from_utf8(b.to_vec()).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

pub fn get_longstr(buf: &mut Bytes) -> DecodeResult<Bytes> {
    let n = get_u32(buf)? as usize;
    take(buf, n)
}

pub fn put_shortstr(out: &mut BytesMut, s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(255);
    out.put_u8(n as u8);
    out.put_slice(&b[..n]);
}

pub fn put_longstr(out: &mut BytesMut, b: &[u8]) {
    out.put_u32(b.len() as u32);
    out.put_slice(b);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_roundtrip() {
        let mut t = FieldTable::new();
        t.insert("a", 1i64);
        t.insert("b", "x");
        t.insert("c", FieldValue::Array(vec![FieldValue::Bool(true), FieldValue::I32(-3)]));
        let mut nested = FieldTable::new();
        nested.insert("d", FieldValue::Timestamp(7));
        t.insert("n", nested);
        let mut b = BytesMut::new();
        t.encode(&mut b);
        let mut bytes = b.freeze();
        assert_eq!(FieldTable::decode(&mut bytes).unwrap(), t);
        assert!(!bytes.has_remaining());
    }

    #[test]
    fn equivalence_ignores_width_and_order() {
        let a: FieldTable = [("x-max-length", FieldValue::I32(10)), ("x-q", FieldValue::str("quorum"))].into_iter().collect();
        let b: FieldTable = [("x-q", FieldValue::str("quorum")), ("x-max-length", FieldValue::I64(10))].into_iter().collect();
        assert!(a.equivalent(&b));
        let c: FieldTable = [("x-max-length", FieldValue::I64(11))].into_iter().collect();
        assert!(!a.equivalent(&c));
    }

    #[test]
    fn json_roundtrip() {
        let j = serde_json::json!({"a": 1, "b": "s", "c": [true, 2.5], "d": {"e": null}});
        assert_eq!(FieldTable::from_json(&j).to_json(), j);
    }
}
