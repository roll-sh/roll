//! `Value`: the decoded form of a serialised CQL value.
//!
//! Serialised bytes are the source of truth; `Value` exists for literals, functions, casts and
//! formatting. `decode` checks structure as it goes (it never trusts a count or length beyond
//! the bytes present) but does not apply every validation rule; call `validate` first when the
//! bytes come from a client.
//!
//! An empty (zero-length) value of a non-string type decodes to `Value::Empty`; a null element
//! inside a tuple, UDT or list is `Value::Null`.

use std::net::IpAddr;

use bytes::Bytes;
use num_bigint::BigInt;

use crate::duration::Duration;
use crate::num::Decimal;
use crate::wire::{self, Collection};
use crate::{CqlType, MarshalError};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// An absent element (null tuple / UDT field, null list element in `IN` values).
    Null,
    /// A zero-length value of a type for which that is not a regular value.
    Empty,
    Ascii(String),
    Text(String),
    Blob(Bytes),
    Boolean(bool),
    Tinyint(i8),
    Smallint(i16),
    Int(i32),
    Bigint(i64),
    Counter(i64),
    Float(f32),
    Double(f64),
    Decimal(Decimal),
    Varint(BigInt),
    /// Milliseconds since the epoch.
    Timestamp(i64),
    /// The encoded day number (epoch at 2^31).
    Date(u32),
    /// Nanoseconds since midnight.
    Time(i64),
    Uuid([u8; 16]),
    Timeuuid([u8; 16]),
    Inet(IpAddr),
    Duration(Duration),
    List(Vec<Value>),
    /// Elements in comparator order (as serialised).
    Set(Vec<Value>),
    /// Entries in key comparator order (as serialised).
    Map(Vec<(Value, Value)>),
    /// Components; trailing ones may be missing (shorter than the type).
    Tuple(Vec<Value>),
    /// Fields in declaration order; trailing ones may be missing.
    Udt(Vec<Value>),
    Vector(Vec<Value>),
    /// Bytes of a custom type, or of `empty`.
    Custom(Bytes),
}

fn short(what: &str) -> MarshalError {
    MarshalError::new(format!("Not enough bytes to read a {what}"))
}

/// Decodes serialised bytes of type `ty`.
pub fn decode(ty: &CqlType, b: &[u8]) -> Result<Value, MarshalError> {
    use CqlType as T;
    if b.is_empty() {
        return Ok(match ty {
            T::Ascii => Value::Ascii(String::new()),
            T::Text => Value::Text(String::new()),
            T::Blob => Value::Blob(Bytes::new()),
            T::Custom(_) | T::Empty => Value::Custom(Bytes::new()),
            _ => Value::Empty,
        });
    }
    let fixed = |n: usize, what: &str| -> Result<(), MarshalError> {
        if b.len() == n {
            Ok(())
        } else {
            Err(MarshalError::new(format!(
                "Expected {n} bytes for a {what} ({})",
                b.len()
            )))
        }
    };
    Ok(match ty {
        T::Ascii => {
            if !b.is_ascii() {
                return Err(MarshalError::new("Invalid byte for ascii"));
            }
            Value::Ascii(String::from_utf8_lossy(b).into_owned())
        }
        T::Text => Value::Text(
            String::from_utf8(b.to_vec())
                .map_err(|_| MarshalError::new("String didn't validate."))?,
        ),
        T::Blob => Value::Blob(Bytes::copy_from_slice(b)),
        T::Custom(_) | T::Empty => Value::Custom(Bytes::copy_from_slice(b)),
        T::Boolean => {
            fixed(1, "boolean")?;
            Value::Boolean(b[0] != 0)
        }
        T::Tinyint => {
            fixed(1, "tinyint")?;
            Value::Tinyint(b[0] as i8)
        }
        T::Smallint => {
            fixed(2, "smallint")?;
            Value::Smallint(i16::from_be_bytes([b[0], b[1]]))
        }
        T::Int => {
            fixed(4, "int")?;
            Value::Int(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        }
        T::Bigint | T::Counter => {
            fixed(8, "bigint")?;
            let v = wire::i64_of(b).unwrap_or(0);
            if matches!(ty, T::Counter) {
                Value::Counter(v)
            } else {
                Value::Bigint(v)
            }
        }
        T::Timestamp => {
            fixed(8, "timestamp")?;
            Value::Timestamp(wire::i64_of(b).unwrap_or(0))
        }
        T::Time => {
            fixed(8, "time")?;
            Value::Time(wire::i64_of(b).unwrap_or(0))
        }
        T::Date => {
            fixed(4, "date")?;
            Value::Date(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        }
        T::Float => {
            fixed(4, "float")?;
            Value::Float(f32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        }
        T::Double => {
            fixed(8, "double")?;
            Value::Double(f64::from_bits(wire::i64_of(b).unwrap_or(0) as u64))
        }
        T::Decimal => Value::Decimal(Decimal::from_bytes(b).ok_or_else(|| short("decimal"))?),
        T::Varint => Value::Varint(BigInt::from_signed_bytes_be(b)),
        T::Uuid | T::Timeuuid => {
            let u: [u8; 16] = b.try_into().map_err(|_| {
                MarshalError::new(format!("UUID should be 16 or 0 bytes ({})", b.len()))
            })?;
            if matches!(ty, T::Uuid) {
                Value::Uuid(u)
            } else {
                Value::Timeuuid(u)
            }
        }
        T::Inet => match b.len() {
            4 => Value::Inet(IpAddr::from(<[u8; 4]>::try_from(b).unwrap_or([0; 4]))),
            16 => Value::Inet(IpAddr::from(<[u8; 16]>::try_from(b).unwrap_or([0; 16]))),
            _ => {
                return Err(MarshalError::new(format!(
                    "Expected 4 or 16 byte inetaddress; got {}",
                    wire::hex(b)
                )));
            }
        },
        T::Duration => Value::Duration(Duration::decode(b).ok_or_else(|| short("duration"))?),
        T::List { elem, .. } => Value::List(decode_elements(elem, b, "list")?),
        T::Set { elem, .. } => Value::Set(decode_elements(elem, b, "set")?),
        T::Map { key, value, .. } => {
            let c = Collection::parse(b).ok_or_else(|| short("map"))?;
            let mut out = Vec::new();
            let mut it = c.items(2);
            while let Some(k) = it.next() {
                let k = k.map_err(|_| short("map"))?;
                let v = it
                    .next()
                    .ok_or_else(|| short("map"))?
                    .map_err(|_| short("map"))?;
                let (Some(k), Some(v)) = (k, v) else {
                    return Err(MarshalError::new("Null value read when not allowed"));
                };
                out.push((decode(key, k)?, decode(value, v)?));
            }
            Value::Map(out)
        }
        T::Tuple(ts) => Value::Tuple(decode_fields(ts.iter(), b, "tuple")?),
        T::Udt(def, _) => Value::Udt(decode_fields(def.fields.iter().map(|(_, t)| t), b, "UDT")?),
        T::Vector { elem, dim } => {
            let parts =
                split_vector(elem, *dim, b).map_err(|_| short(&crate::names::cql_name(ty)))?;
            let mut out = Vec::with_capacity(parts.len());
            for p in parts {
                out.push(decode(elem, p)?);
            }
            Value::Vector(out)
        }
    })
}

fn decode_elements(elem: &CqlType, b: &[u8], what: &str) -> Result<Vec<Value>, MarshalError> {
    let c = Collection::parse(b).ok_or_else(|| short(what))?;
    if c.count < 0 {
        return Err(MarshalError::new(format!(
            "The data cannot be deserialized as a {what}"
        )));
    }
    let mut out = Vec::new();
    for item in c.items(1) {
        match item.map_err(|_| short(what))? {
            Some(v) => out.push(decode(elem, v)?),
            None => out.push(Value::Null),
        }
    }
    Ok(out)
}

fn decode_fields<'a>(
    types: impl Iterator<Item = &'a CqlType>,
    b: &[u8],
    what: &str,
) -> Result<Vec<Value>, MarshalError> {
    let mut out = Vec::new();
    let mut fs = wire::fields(b);
    for t in types {
        match fs.next() {
            None => break,
            Some(Ok(Some(v))) => out.push(decode(t, v)?),
            Some(Ok(None)) => out.push(Value::Null),
            Some(Err(_)) => {
                return Err(MarshalError::new(format!(
                    "Not enough bytes to read a {what}"
                )));
            }
        }
    }
    if fs.next().is_some() {
        return Err(MarshalError::new(format!(
            "Invalid remaining data after end of {what} value"
        )));
    }
    Ok(out)
}

/// Splits a vector value into its `dim` element slices (fixed-size elements are packed,
/// variable-size ones carry an unsigned vint length). Errors on short or trailing bytes.
pub(crate) fn split_vector<'a>(
    elem: &CqlType,
    dim: u32,
    b: &'a [u8],
) -> Result<Vec<&'a [u8]>, VectorError> {
    let mut out = Vec::with_capacity((dim as usize).min(b.len() + 1));
    match elem.value_length_if_fixed() {
        Some(len) => {
            let need = len.checked_mul(dim as usize).ok_or(VectorError::Short)?;
            if b.len() < need {
                return Err(VectorError::Short);
            }
            for i in 0..dim as usize {
                out.push(&b[i * len..(i + 1) * len]);
            }
            if b.len() > need {
                return Err(VectorError::Trailing(b.len() - need));
            }
        }
        None => {
            let mut pos = 0;
            for _ in 0..dim {
                if pos >= b.len() {
                    return Err(VectorError::ShortLength);
                }
                let len = wire::read_unsigned_vint(b, &mut pos).ok_or(VectorError::ShortLength)?;
                let len = usize::try_from(len).map_err(|_| VectorError::Short)?;
                if b.len() - pos < len {
                    return Err(VectorError::Short);
                }
                out.push(&b[pos..pos + len]);
                pos += len;
            }
            if pos < b.len() {
                return Err(VectorError::Trailing(b.len() - pos));
            }
        }
    }
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VectorError {
    /// No bytes left for an element's length.
    ShortLength,
    /// Not enough bytes for an element.
    Short,
    /// Bytes left over after the last element.
    Trailing(usize),
}

/// Serialises a value. The value's own variant decides the encoding; `ty` supplies element types
/// for sorting set / map entries and for packing vectors. Sets and maps are sorted with the
/// type's comparator and de-duplicated, as the reference serialisers do.
pub fn encode(v: &Value, ty: &CqlType) -> Bytes {
    let mut out = Vec::new();
    encode_into(v, ty, &mut out);
    Bytes::from(out)
}

pub(crate) fn encode_into(v: &Value, ty: &CqlType, out: &mut Vec<u8>) {
    match v {
        Value::Null | Value::Empty => {}
        Value::Ascii(s) | Value::Text(s) => out.extend_from_slice(s.as_bytes()),
        Value::Blob(b) | Value::Custom(b) => out.extend_from_slice(b),
        Value::Boolean(b) => out.push(*b as u8),
        Value::Tinyint(x) => out.push(*x as u8),
        Value::Smallint(x) => out.extend_from_slice(&x.to_be_bytes()),
        Value::Int(x) => out.extend_from_slice(&x.to_be_bytes()),
        Value::Bigint(x) | Value::Counter(x) | Value::Timestamp(x) | Value::Time(x) => {
            out.extend_from_slice(&x.to_be_bytes())
        }
        Value::Date(x) => out.extend_from_slice(&x.to_be_bytes()),
        Value::Float(x) => out.extend_from_slice(&x.to_be_bytes()),
        Value::Double(x) => out.extend_from_slice(&x.to_be_bytes()),
        Value::Decimal(d) => out.extend_from_slice(&d.to_bytes()),
        Value::Varint(i) => out.extend_from_slice(&i.to_signed_bytes_be()),
        Value::Uuid(u) | Value::Timeuuid(u) => out.extend_from_slice(u),
        Value::Inet(IpAddr::V4(a)) => out.extend_from_slice(&a.octets()),
        Value::Inet(IpAddr::V6(a)) => out.extend_from_slice(&a.octets()),
        Value::Duration(d) => out.extend_from_slice(&d.encode()),
        Value::List(items) => {
            let elem = elem_type(ty);
            out.extend_from_slice(&(items.len() as i32).to_be_bytes());
            for i in items {
                put_element(out, i, &elem);
            }
        }
        Value::Set(items) => {
            let elem = elem_type(ty);
            let mut enc: Vec<Bytes> = items.iter().map(|i| encode(i, &elem)).collect();
            enc.sort_by(|a, b| crate::compare(&elem, a, b));
            enc.dedup_by(|a, b| crate::compare(&elem, a, b).is_eq());
            out.extend_from_slice(&(enc.len() as i32).to_be_bytes());
            for e in &enc {
                wire::put_item(out, Some(e));
            }
        }
        Value::Map(entries) => {
            let (kt, vt) = match ty {
                CqlType::Map { key, value, .. } => ((**key).clone(), (**value).clone()),
                _ => (CqlType::Blob, CqlType::Blob),
            };
            let mut enc: Vec<(Bytes, Bytes)> = entries
                .iter()
                .map(|(k, v)| (encode(k, &kt), encode(v, &vt)))
                .collect();
            // A stable sort keeps the first of equal keys; Cassandra's map literal keeps the last.
            enc.reverse();
            enc.sort_by(|a, b| crate::compare(&kt, &a.0, &b.0));
            enc.dedup_by(|a, b| crate::compare(&kt, &a.0, &b.0).is_eq());
            out.extend_from_slice(&(enc.len() as i32).to_be_bytes());
            for (k, v) in &enc {
                wire::put_item(out, Some(k));
                wire::put_item(out, Some(v));
            }
        }
        Value::Tuple(items) | Value::Udt(items) => {
            let types: Vec<CqlType> = match ty {
                CqlType::Tuple(ts) => ts.clone(),
                CqlType::Udt(def, _) => def.fields.iter().map(|(_, t)| t.clone()).collect(),
                _ => Vec::new(),
            };
            for (i, item) in items.iter().enumerate() {
                let t = types.get(i).cloned().unwrap_or(CqlType::Blob);
                put_element(out, item, &t);
            }
        }
        Value::Vector(items) => {
            let elem = elem_type(ty);
            let fixed = elem.value_length_if_fixed().is_some();
            for i in items {
                let e = encode(i, &elem);
                if !fixed {
                    wire::put_unsigned_vint(out, e.len() as u64);
                }
                out.extend_from_slice(&e);
            }
        }
    }
}

fn elem_type(ty: &CqlType) -> CqlType {
    match ty {
        CqlType::List { elem, .. } | CqlType::Set { elem, .. } | CqlType::Vector { elem, .. } => {
            (**elem).clone()
        }
        _ => CqlType::Blob,
    }
}

fn put_element(out: &mut Vec<u8>, v: &Value, ty: &CqlType) {
    if matches!(v, Value::Null) {
        wire::put_item(out, None);
    } else {
        let start = out.len();
        out.extend_from_slice(&[0; 4]);
        encode_into(v, ty, out);
        let len = (out.len() - start - 4) as i32;
        out[start..start + 4].copy_from_slice(&len.to_be_bytes());
    }
}

/// Serialises already-serialised elements as a list / set value (`CollectionSerializer.pack`).
pub fn pack_collection<'a>(elements: impl IntoIterator<Item = &'a [u8]>) -> Bytes {
    let mut out = vec![0; 4];
    let mut n: i32 = 0;
    for e in elements {
        wire::put_item(&mut out, Some(e));
        n += 1;
    }
    out[..4].copy_from_slice(&n.to_be_bytes());
    Bytes::from(out)
}

/// Serialises already-serialised entries as a map value.
pub fn pack_map<'a>(entries: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> Bytes {
    let mut out = vec![0; 4];
    let mut n: i32 = 0;
    for (k, v) in entries {
        wire::put_item(&mut out, Some(k));
        wire::put_item(&mut out, Some(v));
        n += 1;
    }
    out[..4].copy_from_slice(&n.to_be_bytes());
    Bytes::from(out)
}

/// Serialises tuple / UDT components (`TupleType.buildValue`; `None` is null).
pub fn pack_fields<'a>(fields: impl IntoIterator<Item = Option<&'a [u8]>>) -> Bytes {
    let mut out = Vec::new();
    for f in fields {
        wire::put_item(&mut out, f);
    }
    Bytes::from(out)
}

/// The element slices of a serialised list or set (`None` for null elements).
pub fn collection_elements(b: &[u8]) -> Result<Vec<Option<&[u8]>>, MarshalError> {
    let c = Collection::parse(b).ok_or_else(|| short("list"))?;
    c.items(1).map(|r| r.map_err(|_| short("list"))).collect()
}

/// A serialised map entry: key and value bytes.
pub type MapEntry<'a> = (&'a [u8], &'a [u8]);

/// The entries of a serialised map.
pub fn map_entries(b: &[u8]) -> Result<Vec<MapEntry<'_>>, MarshalError> {
    let c = Collection::parse(b).ok_or_else(|| short("map"))?;
    let mut out = Vec::new();
    let mut it = c.items(2);
    while let Some(k) = it.next() {
        let k = k.map_err(|_| short("map"))?;
        let v = it
            .next()
            .ok_or_else(|| short("map"))?
            .map_err(|_| short("map"))?;
        match (k, v) {
            (Some(k), Some(v)) => out.push((k, v)),
            _ => return Err(MarshalError::new("Null value read when not allowed")),
        }
    }
    Ok(out)
}

/// The component slices of a serialised tuple or UDT (`None` = null); may be shorter than the type.
pub fn tuple_fields(b: &[u8]) -> Result<Vec<Option<&[u8]>>, MarshalError> {
    wire::fields(b)
        .map(|r| r.map_err(|_| MarshalError::new("Not enough bytes to read a tuple")))
        .collect()
}

/// The element slices of a serialised vector.
pub fn vector_elements<'a>(
    elem: &CqlType,
    dim: u32,
    b: &'a [u8],
) -> Result<Vec<&'a [u8]>, MarshalError> {
    split_vector(elem, dim, b)
        .map_err(|_| short(&format!("vector<{}, {dim}>", crate::names::cql_name(elem))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn rt(ty: &CqlType, v: Value) {
        let b = encode(&v, ty);
        assert_eq!(decode(ty, &b).unwrap(), v, "{ty:?}");
    }

    #[test]
    fn scalar_round_trips() {
        rt(&CqlType::Int, Value::Int(-5));
        rt(&CqlType::Bigint, Value::Bigint(i64::MIN));
        rt(&CqlType::Counter, Value::Counter(7));
        rt(&CqlType::Smallint, Value::Smallint(300));
        rt(&CqlType::Tinyint, Value::Tinyint(-128));
        rt(&CqlType::Boolean, Value::Boolean(true));
        rt(&CqlType::Float, Value::Float(1.5));
        rt(&CqlType::Double, Value::Double(-0.25));
        rt(&CqlType::Text, Value::Text("héllo".into()));
        rt(&CqlType::Ascii, Value::Ascii("it's".into()));
        rt(
            &CqlType::Blob,
            Value::Blob(Bytes::from_static(&[0xca, 0xfe])),
        );
        rt(&CqlType::Timestamp, Value::Timestamp(-1));
        rt(&CqlType::Date, Value::Date(1 << 31));
        rt(&CqlType::Time, Value::Time(5));
        rt(&CqlType::Uuid, Value::Uuid([7; 16]));
        rt(&CqlType::Timeuuid, Value::Timeuuid([0x11; 16]));
        rt(&CqlType::Inet, Value::Inet("1.2.3.4".parse().unwrap()));
        rt(&CqlType::Inet, Value::Inet("::1".parse().unwrap()));
        rt(&CqlType::Duration, Value::Duration(Duration::new(1, 2, 3)));
        rt(
            &CqlType::Decimal,
            Value::Decimal(Decimal::parse("-1.50").unwrap()),
        );
        rt(&CqlType::Varint, Value::Varint(BigInt::from(-300)));
        rt(
            &CqlType::Custom("x.Y".into()),
            Value::Custom(Bytes::from_static(b"zz")),
        );
        assert_eq!(decode(&CqlType::Int, &[]).unwrap(), Value::Empty);
        assert_eq!(
            decode(&CqlType::Text, &[]).unwrap(),
            Value::Text(String::new())
        );
        assert_eq!(
            decode(&CqlType::Blob, &[]).unwrap(),
            Value::Blob(Bytes::new())
        );
        assert_eq!(
            decode(&CqlType::Ascii, &[]).unwrap(),
            Value::Ascii(String::new())
        );
        assert_eq!(
            decode(&CqlType::Empty, &[]).unwrap(),
            Value::Custom(Bytes::new())
        );
        assert!(encode(&Value::Empty, &CqlType::Int).is_empty());
    }

    #[test]
    fn malformed_scalars_fail_cleanly() {
        for (t, b) in [
            (CqlType::Int, &[0u8, 0, 1][..]),
            (CqlType::Bigint, &[0][..]),
            (CqlType::Boolean, &[0, 1][..]),
            (CqlType::Smallint, &[0][..]),
            (CqlType::Tinyint, &[0, 0][..]),
            (CqlType::Float, &[0][..]),
            (CqlType::Double, &[0][..]),
            (CqlType::Date, &[0][..]),
            (CqlType::Time, &[0][..]),
            (CqlType::Timestamp, &[0][..]),
            (CqlType::Uuid, &[0][..]),
            (CqlType::Inet, &[0, 1, 2][..]),
            (CqlType::Decimal, &[0, 0, 0][..]),
            (CqlType::Duration, &[0xfe, 0, 0][..]),
            (CqlType::Text, &[0xff][..]),
            (CqlType::Ascii, &[0x80][..]),
        ] {
            assert!(decode(&t, b).is_err(), "{t:?} {b:?}");
        }
    }

    #[test]
    fn collections_encode_like_the_reference() {
        let set = CqlType::set(CqlType::Int, false);
        // Sorted and de-duplicated by the element comparator.
        let b = encode(
            &Value::Set(vec![Value::Int(2), Value::Int(-1), Value::Int(2)]),
            &set,
        );
        assert_eq!(
            &b[..],
            &[
                0, 0, 0, 2, 0, 0, 0, 4, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 4, 0, 0, 0, 2
            ]
        );
        rt(&set, Value::Set(vec![Value::Int(-1), Value::Int(2)]));
        let list = CqlType::list(CqlType::Text, false);
        rt(
            &list,
            Value::List(vec![
                Value::Text("b".into()),
                Value::Text("a".into()),
                Value::Null,
            ]),
        );
        let map = CqlType::map(CqlType::Text, CqlType::Int, false);
        let b = encode(
            &Value::Map(vec![
                (Value::Text("b".into()), Value::Int(1)),
                (Value::Text("a".into()), Value::Int(2)),
                (Value::Text("b".into()), Value::Int(3)),
            ]),
            &map,
        );
        assert_eq!(
            decode(&map, &b).unwrap(),
            Value::Map(vec![
                (Value::Text("a".into()), Value::Int(2)),
                (Value::Text("b".into()), Value::Int(3))
            ])
        );
        assert_eq!(map_entries(&b).unwrap().len(), 2);
        assert_eq!(
            collection_elements(&encode(&Value::List(vec![Value::Null]), &list)).unwrap(),
            vec![None]
        );
        assert!(decode(&list, &[0, 0, 0, 1, 0, 0]).is_err());
        assert!(decode(&list, &[0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(decode(&map, &[0, 0, 0, 1, 0, 0, 0, 1, b'a']).is_err());
        assert!(
            decode(
                &map,
                &[0, 0, 0, 1, 0, 0, 0, 1, b'a', 0xff, 0xff, 0xff, 0xff]
            )
            .is_err()
        );
        assert!(decode(&map, &[0, 0]).is_err());
        assert!(map_entries(&[0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]).is_err());
        assert_eq!(
            pack_collection([&[1u8][..], &[2][..]]),
            Bytes::from_static(&[0, 0, 0, 2, 0, 0, 0, 1, 1, 0, 0, 0, 1, 2])
        );
        assert_eq!(
            pack_map([(&[1u8][..], &[2u8][..])]),
            Bytes::from_static(&[0, 0, 0, 1, 0, 0, 0, 1, 1, 0, 0, 0, 1, 2])
        );
    }

    #[test]
    fn tuples_udts_vectors() {
        let t = CqlType::Tuple(vec![CqlType::Int, CqlType::Text]);
        rt(&t, Value::Tuple(vec![Value::Int(1), Value::Null]));
        rt(&t, Value::Tuple(vec![Value::Int(1)]));
        assert!(decode(&t, &pack_fields([Some(&[0u8, 0, 0, 1][..]), None, None])).is_err());
        assert!(decode(&t, &[0, 0]).is_err());
        assert_eq!(
            tuple_fields(&pack_fields([None, Some(&b"x"[..])])).unwrap(),
            vec![None, Some(&b"x"[..])]
        );
        let u = CqlType::Udt(
            Arc::new(crate::UdtDef::new(
                "ks",
                "u",
                vec![("a".into(), CqlType::Int)],
            )),
            true,
        );
        rt(&u, Value::Udt(vec![Value::Int(3)]));
        let v = CqlType::vector(CqlType::Float, 2);
        let b = encode(
            &Value::Vector(vec![Value::Float(1.0), Value::Float(2.0)]),
            &v,
        );
        assert_eq!(&b[..], &[0x3f, 0x80, 0, 0, 0x40, 0, 0, 0]);
        rt(
            &v,
            Value::Vector(vec![Value::Float(1.0), Value::Float(2.0)]),
        );
        let vt = CqlType::vector(CqlType::Text, 2);
        let b = encode(
            &Value::Vector(vec![Value::Text("a".into()), Value::Text("b".into())]),
            &vt,
        );
        assert_eq!(&b[..], &[1, b'a', 1, b'b']);
        rt(
            &vt,
            Value::Vector(vec![Value::Text("a".into()), Value::Text("b".into())]),
        );
        assert_eq!(
            split_vector(&CqlType::Text, 2, &[1, b'a']),
            Err(VectorError::ShortLength)
        );
        assert_eq!(
            split_vector(&CqlType::Text, 2, &[1, b'a', 5]),
            Err(VectorError::Short)
        );
        assert_eq!(
            split_vector(&CqlType::Text, 1, &[1, b'a', 5]),
            Err(VectorError::Trailing(1))
        );
        assert_eq!(
            split_vector(&CqlType::Float, 2, &[0; 4]),
            Err(VectorError::Short)
        );
        assert_eq!(
            split_vector(&CqlType::Float, 1, &[0; 5]),
            Err(VectorError::Trailing(1))
        );
        // A huge dimension over a tiny value fails without allocating for it.
        assert!(split_vector(&CqlType::Text, u32::MAX, &[0]).is_err());
        assert!(split_vector(&CqlType::Float, u32::MAX, &[0]).is_err());
        assert!(vector_elements(&CqlType::Float, 2, &[0; 8]).is_ok());
        assert!(decode(&v, &[0; 4]).is_err());
    }
}
