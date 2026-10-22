//! Rejecting malformed serialised values, with each reference's message.
//!
//! Cassandra's rules and texts follow the `org.apache.cassandra.serializers.*Serializer.validate`
//! methods (Apache-2.0), including its permissive UTF-8 check (`UTF8Serializer.UTF8Validator`
//! accepts modified-UTF-8 `C0 80`, surrogates and code points above U+10FFFF). ScyllaDB's were
//! collected by binding malformed values on the image: strict UTF-8, empty values accepted for
//! every scalar, no length check on `decimal`, sets and maps not checked for trailing bytes.
//!
//! Where Cassandra fails with an internal error instead of a message (a truncated duration vint,
//! a truncated variable-size vector element), we return the closest message it uses elsewhere.

use crate::marshal::marshal_name;
use crate::value::{VectorError, split_vector};
use crate::wire::{self, Collection, Short};
use crate::{CqlType, Dialect, MarshalError};

/// Checks that `b` is a valid serialised value of `ty`.
pub fn validate(ty: &CqlType, b: &[u8], dialect: Dialect) -> Result<(), MarshalError> {
    match dialect {
        Dialect::Cassandra => cassandra(ty, b),
        Dialect::Scylla => scylla(ty, b),
    }
}

fn m(s: impl Into<String>) -> MarshalError {
    MarshalError::new(s)
}

fn cassandra(ty: &CqlType, b: &[u8]) -> Result<(), MarshalError> {
    use CqlType as T;
    let n = b.len();
    match ty {
        T::Ascii => match b.iter().find(|&&x| x >= 0x80) {
            Some(&x) => Err(m(format!("Invalid byte for ascii: {}", x as i8))),
            None => Ok(()),
        },
        T::Text => {
            if cassandra_utf8(b) {
                Ok(())
            } else {
                Err(m("String didn't validate."))
            }
        }
        T::Blob | T::Varint | T::Custom(_) => Ok(()),
        T::Empty if n == 0 => Ok(()),
        T::Empty => Err(m("EmptyType only accept empty values")),
        T::Boolean if n > 1 => Err(m(format!("Expected 1 or 0 byte value ({n})"))),
        T::Tinyint if n != 1 => Err(m(format!("Expected 1 byte for a tinyint ({n})"))),
        T::Smallint if n != 2 => Err(m(format!("Expected 2 bytes for a smallint ({n})"))),
        T::Int if n != 4 && n != 0 => Err(m(format!("Expected 4 or 0 byte int ({n})"))),
        T::Bigint | T::Counter if n != 8 && n != 0 => {
            Err(m(format!("Expected 8 or 0 byte long ({n})")))
        }
        T::Float if n != 4 && n != 0 => {
            Err(m(format!("Expected 4 or 0 byte value for a float ({n})")))
        }
        T::Double if n != 8 && n != 0 => {
            Err(m(format!("Expected 8 or 0 byte value for a double ({n})")))
        }
        T::Decimal if n != 0 && n < 4 => Err(m(format!("Expected 0 or at least 4 bytes ({n})"))),
        T::Timestamp if n != 8 && n != 0 => {
            Err(m(format!("Expected 8 or 0 byte long for date ({n})")))
        }
        T::Date if n != 4 => Err(m(format!("Expected 4 byte long for date ({n})"))),
        T::Time if n != 8 => Err(m(format!("Expected 8 byte long for time ({n})"))),
        T::Uuid | T::Timeuuid if n != 16 && n != 0 => {
            Err(m(format!("UUID should be 16 or 0 bytes ({n})")))
        }
        T::Timeuuid if n == 16 && b[6] & 0xf0 != 0x10 => {
            // Cassandra reports the high nibble of the first byte, not the version.
            Err(m(format!(
                "Invalid version for TimeUUID type: 0x{:x}",
                b[0] >> 4
            )))
        }
        T::Inet if n != 0 && n != 4 && n != 16 => Err(m(format!(
            "Expected 4 or 16 byte inetaddress; got {}",
            wire::hex(b)
        ))),
        T::Duration => crate::Duration::validate(b, Dialect::Cassandra),
        T::List { elem, .. } => {
            if wire::i32_at(b, 0).is_some_and(|c| c >= 0) && list_readable(b).is_err() {
                return Err(m("Not enough bytes to read a list"));
            }
            cassandra_elements(b, &[elem], "list")
        }
        T::Set { elem, .. } => cassandra_elements(b, &[elem], "set"),
        T::Map { key, value, .. } => cassandra_elements(b, &[key, value], "map"),
        T::Tuple(ts) => {
            let mut pos = 0;
            for (i, t) in ts.iter().enumerate() {
                if pos == n {
                    return Ok(());
                }
                match wire::read_item(b, &mut pos) {
                    Err(Short::Len { .. }) => {
                        return Err(m(format!(
                            "Not enough bytes to read size of {i}th component"
                        )));
                    }
                    Err(Short::Body { .. }) => {
                        return Err(m(format!("Not enough bytes to read {i}th component")));
                    }
                    Ok(wire::Item::Null) => {}
                    Ok(wire::Item::Value(v)) => cassandra(t, v)?,
                }
            }
            if pos != n {
                return Err(m("Invalid remaining data after end of tuple value"));
            }
            Ok(())
        }
        T::Udt(def, _) => {
            let mut pos = 0;
            for (i, (name, t)) in def.fields.iter().enumerate() {
                if pos == n {
                    return Ok(());
                }
                match wire::read_item(b, &mut pos) {
                    Err(Short::Len { .. }) => {
                        return Err(m(format!(
                            "Not enough bytes to read size of {i}th field {name}"
                        )));
                    }
                    Err(Short::Body { .. }) => {
                        return Err(m(format!("Not enough bytes to read {i}th field {name}")));
                    }
                    Ok(wire::Item::Null) => {}
                    Ok(wire::Item::Value(v)) => cassandra(t, v).map_err(|e| {
                        m(format!(
                            "Failure validating the {i}th field {name}; {}",
                            e.message
                        ))
                    })?,
                }
            }
            if pos != n {
                return Err(m("Invalid remaining data after end of UDT value"));
            }
            Ok(())
        }
        T::Vector { elem, dim } => {
            if n == 0 {
                return Err(m("Invalid empty vector value"));
            }
            let name = crate::names::cql_name(ty);
            match split_vector(elem, *dim, b) {
                Ok(parts) => parts.into_iter().try_for_each(|p| cassandra(elem, p)),
                Err(VectorError::Trailing(k)) => Err(m(format!(
                    "Unexpected {k} extraneous bytes after {name} value"
                ))),
                Err(_) => Err(m(format!("Not enough bytes to read a {name}"))),
            }
        }
        _ => Ok(()),
    }
}

/// Whether a list's elements can all be read, nulls included: both references report a
/// truncated list (a count larger than its elements, as from a random 8-byte value) before a
/// null element in it. Each element takes 4 bytes or more, so this is bounded by the value.
fn list_readable(b: &[u8]) -> Result<(), wire::Short> {
    let Some(count) = wire::i32_at(b, 0) else {
        return Err(wire::Short::Len { got: b.len() });
    };
    let mut pos = 4;
    for _ in 0..count.max(0) {
        wire::read_item(b, &mut pos)?;
    }
    Ok(())
}

/// `ListSerializer` / `SetSerializer` / `MapSerializer.validate`.
fn cassandra_elements(b: &[u8], types: &[&CqlType], what: &str) -> Result<(), MarshalError> {
    let not_enough = || m(format!("Not enough bytes to read a {what}"));
    let c = Collection::parse(b).ok_or_else(not_enough)?;
    if c.count < 0 {
        return Err(m(format!("The data cannot be deserialized as a {what}")));
    }
    let mut pos = 4;
    for _ in 0..c.count {
        for t in types {
            match wire::read_item(b, &mut pos) {
                Err(_) => return Err(not_enough()),
                Ok(wire::Item::Null) => return Err(m("Null value read when not allowed")),
                Ok(wire::Item::Value(v)) => cassandra(t, v)?,
            }
        }
    }
    if pos != b.len() {
        return Err(m(format!("Unexpected extraneous bytes after {what} value")));
    }
    Ok(())
}

/// `UTF8Serializer.UTF8Validator`: a byte-level state machine more permissive than UTF-8.
fn cassandra_utf8(b: &[u8]) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum S {
        Start,
        Two,
        Two80,
        ThreeA0bf,
        Three80bf1,
        Three80bf2,
        Four90bf,
        Four80bf3,
    }
    let mut st = S::Start;
    for &x in b {
        let sb = x as i8;
        st = match st {
            S::Start => {
                if sb >= 0 {
                    S::Start
                } else if (sb >> 5) == -2 {
                    if x == 0xc0 {
                        S::Two80
                    } else if x & 0x1e == 0 {
                        return false;
                    } else {
                        S::Two
                    }
                } else if (sb >> 4) == -2 {
                    if x == 0xe0 {
                        S::ThreeA0bf
                    } else {
                        S::Three80bf2
                    }
                } else if (sb >> 3) == -2 {
                    if x == 0xf0 { S::Four90bf } else { S::Four80bf3 }
                } else {
                    return false;
                }
            }
            S::Two | S::Three80bf1 => {
                if x & 0xc0 != 0x80 {
                    return false;
                }
                S::Start
            }
            S::Two80 => {
                if x != 0x80 {
                    return false;
                }
                S::Start
            }
            S::ThreeA0bf => {
                if x & 0xe0 == 0x80 {
                    return false;
                }
                S::Three80bf1
            }
            S::Three80bf2 => {
                if x & 0xc0 != 0x80 {
                    return false;
                }
                S::Three80bf1
            }
            S::Four90bf => {
                if x & 0x30 == 0 {
                    return false;
                }
                S::Three80bf2
            }
            S::Four80bf3 => {
                if x & 0xc0 != 0x80 {
                    return false;
                }
                S::Three80bf2
            }
        };
    }
    st == S::Start
}

fn s(msg: impl std::fmt::Display) -> MarshalError {
    MarshalError::scylla(msg)
}

fn short_msg(e: Short) -> MarshalError {
    match e {
        Short::Len { got } => s(format!(
            "read_simple - not enough bytes (expected 4, got {got})"
        )),
        Short::Body { requested, got } => s(format!(
            "read_simple_bytes - not enough bytes (requested {requested}, got {got})"
        )),
    }
}

fn scylla(ty: &CqlType, b: &[u8]) -> Result<(), MarshalError> {
    use CqlType as T;
    let n = b.len();
    let class = |t: &CqlType| marshal_name(t, false, Dialect::Scylla);
    match ty {
        T::Ascii if !b.is_ascii() => Err(s(
            "Validation failed - non-ASCII character in an ASCII string",
        )),
        T::Text => match std::str::from_utf8(b) {
            Ok(_) => Ok(()),
            Err(e) => Err(s(format!(
                "Validation failed - non-UTF8 character in a UTF8 string, at byte offset {}",
                e.valid_up_to()
            ))),
        },
        T::Boolean if n > 1 => Err(s(format!("Validation failed for boolean, got {n} bytes"))),
        T::Tinyint if n > 1 => Err(s(format!("Expected 1 byte for a tinyint ({n})"))),
        T::Smallint if n != 2 && n != 0 => Err(s(format!("Expected 2 bytes for a smallint ({n})"))),
        T::Int | T::Bigint | T::Counter
            if n != 0 && n != ty.value_length_if_fixed().unwrap_or(8) =>
        {
            Err(s(format!(
                "Validation failed for type {}: got {n} bytes",
                class(ty)
            )))
        }
        T::Float if n != 4 && n != 0 => {
            Err(s(format!("Expected 4 bytes for a floating type, got {n}")))
        }
        T::Double if n != 8 && n != 0 => {
            Err(s(format!("Expected 8 bytes for a floating type, got {n}")))
        }
        T::Timestamp if n != 8 && n != 0 => Err(s(format!(
            "Validation failed for timestamp - got {n} bytes"
        ))),
        T::Date if n != 4 && n != 0 => Err(s(format!("Expected 4 byte long for date ({n})"))),
        T::Time if n != 8 && n != 0 => Err(s(format!("Expected 8 byte long for time ({n})"))),
        T::Uuid if n != 16 && n != 0 => {
            Err(s(format!("Validation failed for uuid - got {n} bytes")))
        }
        T::Timeuuid if n != 16 && n != 0 => {
            Err(s(format!("Validation failed for timeuuid - got {n} bytes")))
        }
        T::Timeuuid if n == 16 && b[6] >> 4 != 1 => {
            Err(s(format!("Unsupported UUID version ({})", b[6] >> 4)))
        }
        T::Inet if n != 0 && n != 4 && n != 16 => Err(s(format!(
            "Validation failed for inet_addr - got {n} bytes"
        ))),
        T::Empty if n != 0 => Err(s(format!(
            "Validation failed for type {}: got {n} bytes",
            class(ty)
        ))),
        T::Duration => crate::Duration::validate(b, Dialect::Scylla),
        T::List { elem, .. } => {
            list_readable(b).map_err(short_msg)?;
            let rest = scylla_elements(b, &[elem], "Invalid NULL element in list")?;
            if !rest.is_empty() {
                let count = wire::i32_at(b, 0).unwrap_or(0) as u32;
                return Err(s(format!(
                    "Validation failed for type {}: bytes remaining after reading all {count} elements of the list -> [{}]",
                    class(ty),
                    wire::hex(rest)
                )));
            }
            Ok(())
        }
        T::Set { elem, .. } => {
            scylla_elements(b, &[elem], "null is not supported inside collections").map(|_| ())
        }
        T::Map { key, value, .. } => {
            scylla_elements(b, &[key, value], "null is not supported inside collections")
                .map(|_| ())
        }
        T::Tuple(ts) => scylla_fields(ty, ts.iter(), b),
        T::Udt(def, _) => scylla_fields(ty, def.fields.iter().map(|(_, t)| t), b),
        T::Vector { elem, dim } => {
            let trailing = |k: usize| {
                s(format!(
                    "Validation failed for type {}: bytes remaining after reading all {dim} elements of the vector -> [{}]",
                    class(ty),
                    wire::hex(&b[n - k..])
                ))
            };
            if elem.value_length_if_fixed().is_some() {
                let parts = match split_vector(elem, *dim, b) {
                    Ok(p) => p,
                    Err(VectorError::Trailing(k)) => return Err(trailing(k)),
                    Err(_) => return Err(m("Not enough bytes to read a vector element")),
                };
                return parts.into_iter().try_for_each(|p| scylla(elem, p));
            }
            // Variable-size elements are checked as they are read; an empty one is a null.
            let mut pos = 0;
            for _ in 0..*dim {
                if pos >= n {
                    return Err(m("Not enough bytes to read a vector element length"));
                }
                let len = wire::read_unsigned_vint(b, &mut pos)
                    .ok_or_else(|| m("Not enough bytes to read a vector element length"))?;
                if len == 0 {
                    return Err(m("null/unset is not supported inside vectors"));
                }
                if ((n - pos) as u64) < len {
                    return Err(m("Not enough bytes to read a vector element"));
                }
                let len = len as usize;
                scylla(elem, &b[pos..pos + len])?;
                pos += len;
            }
            if pos < n {
                return Err(trailing(n - pos));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Reads `count` elements (the count taken as unsigned), validating each; returns the bytes
/// after them.
fn scylla_elements<'a>(
    b: &'a [u8],
    types: &[&CqlType],
    null_msg: &str,
) -> Result<&'a [u8], MarshalError> {
    let Some(count) = wire::i32_at(b, 0) else {
        return Err(short_msg(Short::Len { got: b.len() }));
    };
    let mut pos = 4;
    for _ in 0..count as u32 {
        for t in types {
            match wire::read_item(b, &mut pos) {
                Err(e) => return Err(short_msg(e)),
                Ok(wire::Item::Null) => return Err(m(null_msg)),
                Ok(wire::Item::Value(v)) => scylla(t, v)?,
            }
        }
    }
    Ok(&b[pos..])
}

fn scylla_fields<'t>(
    ty: &CqlType,
    types: impl ExactSizeIterator<Item = &'t CqlType>,
    b: &[u8],
) -> Result<(), MarshalError> {
    let expected = types.len();
    let types: Vec<&CqlType> = types.collect();
    let mut pos = 0;
    let mut got = 0;
    while pos < b.len() {
        match wire::read_item(b, &mut pos) {
            Err(e) => return Err(short_msg(e)),
            Ok(wire::Item::Null) => {}
            Ok(wire::Item::Value(v)) => {
                if let Some(t) = types.get(got) {
                    scylla(t, v)?;
                }
            }
        }
        got += 1;
    }
    if got > expected {
        return Err(s(format!(
            "Value of type {} contained too many fields (expected {expected}, got {got})",
            marshal_name(ty, false, Dialect::Scylla)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const C: Dialect = Dialect::Cassandra;
    const S: Dialect = Dialect::Scylla;

    fn h(s: &str) -> Vec<u8> {
        wire::unhex(s).unwrap()
    }

    fn check(ty: &CqlType, d: Dialect, hex: &str) -> Result<(), String> {
        validate(ty, &h(hex), d).map_err(|e| e.message)
    }

    fn addr() -> CqlType {
        CqlType::Udt(
            Arc::new(crate::UdtDef::new(
                "types_t",
                "addr",
                vec![
                    ("street".into(), CqlType::Text),
                    ("Zip".into(), CqlType::Int),
                ],
            )),
            true,
        )
    }

    #[test]
    fn cassandra_scalars() {
        // Raw bound values on cassandra:5.0.9.
        use CqlType as T;
        for (t, hex, msg) in [
            (T::Int, "000001", "Expected 4 or 0 byte int (3)"),
            (T::Int, "0000000001", "Expected 4 or 0 byte int (5)"),
            (T::Bigint, "00", "Expected 8 or 0 byte long (1)"),
            (T::Counter, "00", "Expected 8 or 0 byte long (1)"),
            (
                T::Float,
                "000000",
                "Expected 4 or 0 byte value for a float (3)",
            ),
            (
                T::Double,
                "00",
                "Expected 8 or 0 byte value for a double (1)",
            ),
            (T::Date, "", "Expected 4 byte long for date (0)"),
            (T::Date, "000000", "Expected 4 byte long for date (3)"),
            (T::Time, "", "Expected 8 byte long for time (0)"),
            (T::Time, "00", "Expected 8 byte long for time (1)"),
            (T::Timestamp, "00", "Expected 8 or 0 byte long for date (1)"),
            (T::Uuid, "00", "UUID should be 16 or 0 bytes (1)"),
            (T::Timeuuid, "00", "UUID should be 16 or 0 bytes (1)"),
            (
                T::Timeuuid,
                "00000000000020008000000000000000",
                "Invalid version for TimeUUID type: 0x0",
            ),
            (
                T::Inet,
                "000102",
                "Expected 4 or 16 byte inetaddress; got 000102",
            ),
            (T::Boolean, "0102", "Expected 1 or 0 byte value (2)"),
            (T::Tinyint, "", "Expected 1 byte for a tinyint (0)"),
            (T::Tinyint, "0001", "Expected 1 byte for a tinyint (2)"),
            (T::Smallint, "", "Expected 2 bytes for a smallint (0)"),
            (T::Smallint, "00", "Expected 2 bytes for a smallint (1)"),
            (T::Decimal, "000000", "Expected 0 or at least 4 bytes (3)"),
            (T::Decimal, "00", "Expected 0 or at least 4 bytes (1)"),
            (
                T::Duration,
                "",
                "Expected at least 3 bytes for a duration (0)",
            ),
            (T::Ascii, "80", "Invalid byte for ascii: -128"),
            (T::Text, "c0", "String didn't validate."),
            (T::Text, "ff", "String didn't validate."),
            (T::Text, "6162ff", "String didn't validate."),
            (T::Empty, "00", "EmptyType only accept empty values"),
        ] {
            assert_eq!(check(&t, C, hex), Err(msg.to_string()), "{t:?} {hex}");
        }
        for (t, hex) in [
            (T::Inet, ""),
            (T::Boolean, ""),
            (T::Boolean, "ff"),
            (T::Int, ""),
            (T::Varint, ""),
            (T::Varint, "00ff"),
            (T::Duration, "000001"),
            (T::Duration, "020000"),
            (T::Text, "c080"),
            (T::Text, "eda080"),
            (T::Text, "f4908080"),
            (T::Text, "68c3a96c6c6f"),
            (T::Time, "ffffffffffffffff"),
            (T::Timeuuid, "50554d6e29bb11e5b345feff819cdc9f"),
            (T::Timeuuid, ""),
            (T::Blob, "ff"),
            (T::Empty, ""),
        ] {
            assert_eq!(check(&t, C, hex), Ok(()), "{t:?} {hex}");
        }
    }

    #[test]
    fn scylla_scalars() {
        use CqlType as T;
        for (t, hex, msg) in [
            (
                T::Int,
                "000001",
                "Validation failed for type org.apache.cassandra.db.marshal.Int32Type: got 3 bytes",
            ),
            (
                T::Int,
                "0000000001",
                "Validation failed for type org.apache.cassandra.db.marshal.Int32Type: got 5 bytes",
            ),
            (
                T::Bigint,
                "00",
                "Validation failed for type org.apache.cassandra.db.marshal.LongType: got 1 bytes",
            ),
            (
                T::Float,
                "000000",
                "Expected 4 bytes for a floating type, got 3",
            ),
            (
                T::Double,
                "00",
                "Expected 8 bytes for a floating type, got 1",
            ),
            (T::Date, "000000", "Expected 4 byte long for date (3)"),
            (T::Time, "00", "Expected 8 byte long for time (1)"),
            (
                T::Timestamp,
                "00",
                "Validation failed for timestamp - got 1 bytes",
            ),
            (T::Uuid, "00", "Validation failed for uuid - got 1 bytes"),
            (
                T::Timeuuid,
                "00",
                "Validation failed for timeuuid - got 1 bytes",
            ),
            (
                T::Timeuuid,
                "00000000000020008000000000000000",
                "Unsupported UUID version (2)",
            ),
            (
                T::Inet,
                "000102",
                "Validation failed for inet_addr - got 3 bytes",
            ),
            (
                T::Boolean,
                "0102",
                "Validation failed for boolean, got 2 bytes",
            ),
            (T::Tinyint, "0001", "Expected 1 byte for a tinyint (2)"),
            (T::Smallint, "00", "Expected 2 bytes for a smallint (1)"),
            (
                T::Duration,
                "",
                "Expected at least 3 bytes for a duration, got 0",
            ),
            (
                T::Ascii,
                "80",
                "Validation failed - non-ASCII character in an ASCII string",
            ),
            (
                T::Text,
                "c0",
                "Validation failed - non-UTF8 character in a UTF8 string, at byte offset 0",
            ),
            (
                T::Text,
                "c080",
                "Validation failed - non-UTF8 character in a UTF8 string, at byte offset 0",
            ),
            (
                T::Text,
                "eda080",
                "Validation failed - non-UTF8 character in a UTF8 string, at byte offset 0",
            ),
            (
                T::Text,
                "f4908080",
                "Validation failed - non-UTF8 character in a UTF8 string, at byte offset 0",
            ),
            (
                T::Text,
                "6162ff",
                "Validation failed - non-UTF8 character in a UTF8 string, at byte offset 2",
            ),
            (
                T::Text,
                "61c3a9ff",
                "Validation failed - non-UTF8 character in a UTF8 string, at byte offset 3",
            ),
        ] {
            assert_eq!(
                check(&t, S, hex),
                Err(format!("marshaling error: {msg}")),
                "{t:?} {hex}"
            );
        }
        for (t, hex) in [
            (T::Date, ""),
            (T::Time, ""),
            (T::Tinyint, ""),
            (T::Smallint, ""),
            (T::Decimal, "000000"),
            (T::Decimal, "00"),
            (T::Duration, "fe0000"),
            (T::Inet, ""),
            (T::Boolean, ""),
            (T::Varint, ""),
        ] {
            assert_eq!(check(&t, S, hex), Ok(()), "{t:?} {hex}");
        }
    }

    #[test]
    fn collections() {
        let l = CqlType::list(CqlType::Int, false);
        let st = CqlType::set(CqlType::Int, false);
        let mp = CqlType::map(CqlType::Int, CqlType::Text, false);
        for (t, hex, msg) in [
            (&l, "", "Not enough bytes to read a list"),
            (&l, "00", "Not enough bytes to read a list"),
            (&l, "00000001", "Not enough bytes to read a list"),
            (
                &l,
                "0000000100000004000000",
                "Not enough bytes to read a list",
            ),
            (&l, "00000001ffffffff", "Null value read when not allowed"),
            (&l, "7fffffffffffffff", "Not enough bytes to read a list"),
            (&st, "7fffffffffffffff", "Null value read when not allowed"),
            (
                &l,
                "000000010000000400000001ff",
                "Unexpected extraneous bytes after list value",
            ),
            (&l, "ffffffff", "The data cannot be deserialized as a list"),
            (
                &l,
                "ffffffff0000000400000001",
                "The data cannot be deserialized as a list",
            ),
            (
                &l,
                "00000000ff",
                "Unexpected extraneous bytes after list value",
            ),
            (&l, "00000001000000030000ff", "Expected 4 or 0 byte int (3)"),
            (
                &st,
                "000000020000000400000002000000040000000100",
                "Unexpected extraneous bytes after set value",
            ),
            (
                &st,
                "00000001000000040000000100000004",
                "Unexpected extraneous bytes after set value",
            ),
            (&st, "00000001ffffffff", "Null value read when not allowed"),
            (&st, "", "Not enough bytes to read a set"),
            (
                &mp,
                "00000001000000040000000100000001aa",
                "String didn't validate.",
            ),
            (
                &mp,
                "0000000100000004000000010000000561",
                "Not enough bytes to read a map",
            ),
            (
                &mp,
                "0000000100000004000000010000000161ff",
                "Unexpected extraneous bytes after map value",
            ),
            (
                &mp,
                "00000001ffffffff00000001",
                "Null value read when not allowed",
            ),
            (
                &mp,
                "0000000100000004000000017fffffff",
                "Not enough bytes to read a map",
            ),
        ] {
            assert_eq!(check(t, C, hex), Err(msg.to_string()), "{t:?} {hex}");
        }
        assert_eq!(check(&l, C, "00000000"), Ok(()));

        let list_class =
            "org.apache.cassandra.db.marshal.ListType(org.apache.cassandra.db.marshal.Int32Type)";
        for (t, hex, msg) in [
            (&l, "", "marshaling error: read_simple - not enough bytes (expected 4, got 0)".to_string()),
            (&l, "00", "marshaling error: read_simple - not enough bytes (expected 4, got 1)".into()),
            (&l, "00000001", "marshaling error: read_simple - not enough bytes (expected 4, got 0)".into()),
            (&l, "0000000100000004000000", "marshaling error: read_simple_bytes - not enough bytes (requested 4, got 3)".into()),
            (&l, "ffffffff", "marshaling error: read_simple - not enough bytes (expected 4, got 0)".into()),
            (&l, "ffffffff0000000400000001", "marshaling error: read_simple - not enough bytes (expected 4, got 0)".into()),
            (&l, "00000001ffffffff", "Invalid NULL element in list".into()),
            (&l, "7fffffffffffffff", "marshaling error: read_simple - not enough bytes (expected 4, got 0)".into()),
            (
                &l,
                "000000010000000400000001ff",
                format!("marshaling error: Validation failed for type {list_class}: bytes remaining after reading all 1 elements of the list -> [ff]"),
            ),
            (
                &l,
                "00000000ff",
                format!("marshaling error: Validation failed for type {list_class}: bytes remaining after reading all 0 elements of the list -> [ff]"),
            ),
            (&st, "00000001ffffffff", "null is not supported inside collections".into()),
            (&mp, "00000001ffffffff00000001", "null is not supported inside collections".into()),
            (
                &mp,
                "00000001000000040000000100000001aa",
                "marshaling error: Validation failed - non-UTF8 character in a UTF8 string, at byte offset 0".into(),
            ),
            (&mp, "0000000100000004000000010000000561", "marshaling error: read_simple_bytes - not enough bytes (requested 5, got 1)".into()),
            (&mp, "0000000100000004000000017fffffff", "marshaling error: read_simple_bytes - not enough bytes (requested 2147483647, got 0)".into()),
        ] {
            assert_eq!(check(t, S, hex), Err(msg), "{t:?} {hex}");
        }
        // ScyllaDB does not check sets and maps for trailing bytes, order or duplicates.
        for (t, hex) in [
            (&st, "000000020000000400000002000000040000000100"),
            (&st, "00000001000000040000000100000004"),
            (&mp, "0000000100000004000000010000000161ff"),
            (&l, "00000000"),
        ] {
            assert_eq!(check(t, S, hex), Ok(()), "{t:?} {hex}");
        }
    }

    #[test]
    fn tuples_and_udts() {
        let t = CqlType::Tuple(vec![CqlType::Int, CqlType::Text]);
        for (hex, msg) in [
            ("00", "Not enough bytes to read size of 0th component"),
            (
                "0000000400000001000000016100",
                "Invalid remaining data after end of tuple value",
            ),
            (
                "000000040000000100",
                "Not enough bytes to read size of 1th component",
            ),
            (
                "0000000400000001ffffffffffffffff",
                "Invalid remaining data after end of tuple value",
            ),
            ("0000000500000001", "Not enough bytes to read 0th component"),
            ("00000003000000", "Expected 4 or 0 byte int (3)"),
        ] {
            assert_eq!(check(&t, C, hex), Err(msg.into()), "{hex}");
        }
        for hex in ["", "ffffffff", "00000004000000010000000161"] {
            assert_eq!(check(&t, C, hex), Ok(()));
            assert_eq!(check(&t, S, hex), Ok(()));
        }
        let tuple_class = "org.apache.cassandra.db.marshal.TupleType(org.apache.cassandra.db.marshal.Int32Type, org.apache.cassandra.db.marshal.UTF8Type)";
        assert_eq!(
            check(&t, S, "00"),
            Err("marshaling error: read_simple - not enough bytes (expected 4, got 1)".into())
        );
        assert_eq!(
            check(&t, S, "0000000400000001000000016100"),
            Err("marshaling error: read_simple - not enough bytes (expected 4, got 1)".into())
        );
        assert_eq!(
            check(&t, S, "0000000400000001ffffffffffffffff"),
            Err(format!(
                "marshaling error: Value of type {tuple_class} contained too many fields (expected 2, got 3)"
            ))
        );

        let a = addr();
        for (hex, msg) in [
            (
                "000000016100",
                "Not enough bytes to read size of 1th field Zip",
            ),
            ("0000000561", "Not enough bytes to read 0th field street"),
            (
                "00000001ff00000001",
                "Failure validating the 0th field street; String didn't validate.",
            ),
            (
                "000000016100000003000000",
                "Failure validating the 1th field Zip; Expected 4 or 0 byte int (3)",
            ),
            (
                "000000016100000004000000010000000100",
                "Invalid remaining data after end of UDT value",
            ),
            (
                "00000001610000000400000001ffffffff",
                "Invalid remaining data after end of UDT value",
            ),
        ] {
            assert_eq!(check(&a, C, hex), Err(msg.into()), "{hex}");
        }
        assert_eq!(check(&a, C, "ffffffff"), Ok(()));
        let udt_class = "org.apache.cassandra.db.marshal.FrozenType(org.apache.cassandra.db.marshal.UserType(types_t,61646472,737472656574:org.apache.cassandra.db.marshal.UTF8Type,5a6970:org.apache.cassandra.db.marshal.Int32Type))";
        for (hex, msg) in [
            ("000000016100", "marshaling error: read_simple - not enough bytes (expected 4, got 1)".to_string()),
            ("0000000561", "marshaling error: read_simple_bytes - not enough bytes (requested 5, got 1)".into()),
            ("00000001ff00000001", "marshaling error: Validation failed - non-UTF8 character in a UTF8 string, at byte offset 0".into()),
            (
                "000000016100000003000000",
                "marshaling error: Validation failed for type org.apache.cassandra.db.marshal.Int32Type: got 3 bytes".into(),
            ),
            ("000000016100000004000000010000000100", format!("marshaling error: Value of type {udt_class} contained too many fields (expected 2, got 3)")),
            ("00000001610000000400000001ffffffff", format!("marshaling error: Value of type {udt_class} contained too many fields (expected 2, got 3)")),
        ] {
            assert_eq!(check(&a, S, hex), Err(msg), "{hex}");
        }
        assert_eq!(check(&a, S, "ffffffff"), Ok(()));
    }

    #[test]
    fn vectors() {
        let v = CqlType::vector(CqlType::Float, 2);
        let vt = CqlType::vector(CqlType::Text, 2);
        for (t, hex, msg) in [
            (&v, "", "Invalid empty vector value"),
            (
                &v,
                "0000803f",
                "Not enough bytes to read a vector<float, 2>",
            ),
            (
                &v,
                "0000803f0000803f0000803f00",
                "Unexpected 5 extraneous bytes after vector<float, 2> value",
            ),
            (&vt, "", "Invalid empty vector value"),
            (&vt, "0161", "Not enough bytes to read a vector<text, 2>"),
            (&vt, "00", "Not enough bytes to read a vector<text, 2>"),
            (
                &vt,
                "01610162ff",
                "Unexpected 1 extraneous bytes after vector<text, 2> value",
            ),
            (&vt, "016101", "Not enough bytes to read a vector<text, 2>"),
            (&vt, "01ff01ff", "String didn't validate."),
        ] {
            assert_eq!(check(t, C, hex), Err(msg.into()), "{t:?} {hex}");
        }
        assert_eq!(check(&vt, C, "0000"), Ok(()));
        assert_eq!(check(&vt, C, "01610162"), Ok(()));
        assert_eq!(check(&v, C, "0000803f0000803f"), Ok(()));
        let vclass = "org.apache.cassandra.db.marshal.VectorType(org.apache.cassandra.db.marshal.FloatType, 2)";
        let tclass = "org.apache.cassandra.db.marshal.VectorType(org.apache.cassandra.db.marshal.UTF8Type, 2)";
        for (t, hex, msg) in [
            (
                &v,
                "",
                "Not enough bytes to read a vector element".to_string(),
            ),
            (
                &v,
                "0000803f",
                "Not enough bytes to read a vector element".into(),
            ),
            (
                &v,
                "0000803f0000803f0000803f00",
                format!(
                    "marshaling error: Validation failed for type {vclass}: bytes remaining after reading all 2 elements of the vector -> [0000803f00]"
                ),
            ),
            (
                &vt,
                "",
                "Not enough bytes to read a vector element length".into(),
            ),
            (
                &vt,
                "0161",
                "Not enough bytes to read a vector element length".into(),
            ),
            (
                &vt,
                "016101",
                "Not enough bytes to read a vector element".into(),
            ),
            (
                &vt,
                "05",
                "Not enough bytes to read a vector element".into(),
            ),
            (
                &vt,
                "02610262",
                "Not enough bytes to read a vector element".into(),
            ),
            (
                &vt,
                "00",
                "null/unset is not supported inside vectors".into(),
            ),
            (
                &vt,
                "0000",
                "null/unset is not supported inside vectors".into(),
            ),
            (
                &vt,
                "01610162ff",
                format!(
                    "marshaling error: Validation failed for type {tclass}: bytes remaining after reading all 2 elements of the vector -> [ff]"
                ),
            ),
        ] {
            assert_eq!(check(t, S, hex), Err(msg), "{t:?} {hex}");
        }
        assert_eq!(check(&v, S, "0000803f0000803f"), Ok(()));
    }
}
