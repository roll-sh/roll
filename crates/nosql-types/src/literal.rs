//! CQL constants and string conversions.
//!
//! - [`from_string`] is `AbstractType.fromString`: the text form each type accepts (used by
//!   literals, JSON strings and `textAs…`-style conversions), with the reference's errors.
//! - [`literal_to_bytes`] is `Constants.Literal.prepare`: which kinds of constant a column type
//!   accepts (`Invalid STRING constant (x) for "c" of type int` otherwise), then `from_string`.
//!
//! Follows `org.apache.cassandra.cql3.Constants` and the marshal types' `fromString` (Apache-2.0).
//! ScyllaDB differences (from the image): integer constants are not accepted for `time` or
//! `duration`, hex constants only for `blob`, empty strings are accepted for `date` and `time`,
//! out-of-range integers report `Value out of range for type …`, and `varint` text with a leading
//! zero is octal.

use bytes::Bytes;

use crate::num::{Decimal, parse_varint};
use crate::wire::unhex;
use crate::{CqlType, Dialect, MarshalError, float, inet, temporal, timeuuid};

/// The lexical kind of a CQL constant (`Constants.Type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiteralKind {
    String,
    Integer,
    Uuid,
    Float,
    Boolean,
    Hex,
    Duration,
}

impl LiteralKind {
    pub fn name(self) -> &'static str {
        match self {
            LiteralKind::String => "STRING",
            LiteralKind::Integer => "INTEGER",
            LiteralKind::Uuid => "UUID",
            LiteralKind::Float => "FLOAT",
            LiteralKind::Boolean => "BOOLEAN",
            LiteralKind::Hex => "HEX",
            LiteralKind::Duration => "DURATION",
        }
    }
}

/// Whether a constant of `kind` may be assigned to a column of type `ty` (`testAssignment`).
pub fn is_assignable(kind: LiteralKind, ty: &CqlType, dialect: Dialect) -> bool {
    use CqlType as T;
    use LiteralKind as K;
    let scylla = dialect.is_scylla();
    match ty {
        // A tuple takes a string constant through `TupleType.fromString` (`'1:a'`).
        T::Tuple(_) => kind == K::String,
        T::List { .. } | T::Set { .. } | T::Map { .. } | T::Udt(..) | T::Vector { .. } => false,
        T::Custom(_) => true,
        _ => match kind {
            K::String => matches!(
                ty,
                T::Ascii | T::Text | T::Inet | T::Date | T::Time | T::Timestamp
            ),
            K::Integer => match ty {
                T::Time | T::Duration => !scylla,
                T::Bigint
                | T::Counter
                | T::Date
                | T::Decimal
                | T::Double
                | T::Float
                | T::Int
                | T::Smallint => true,
                T::Timestamp | T::Tinyint | T::Varint => true,
                _ => false,
            },
            K::Uuid => matches!(ty, T::Uuid | T::Timeuuid),
            K::Float => matches!(ty, T::Decimal | T::Double | T::Float),
            K::Boolean => matches!(ty, T::Boolean),
            // Cassandra's preferred type for hex constants is `tinyint` (an exact match).
            K::Hex => matches!(ty, T::Blob) || (!scylla && matches!(ty, T::Tinyint)),
            K::Duration => matches!(ty, T::Duration),
        },
    }
}

/// `Constants.Literal.prepare`: the serialised value of a constant assigned to `receiver`.
pub fn literal_to_bytes(
    kind: LiteralKind,
    text: &str,
    receiver: &str,
    ty: &CqlType,
    dialect: Dialect,
) -> Result<Bytes, MarshalError> {
    if !is_assignable(kind, ty, dialect) {
        return Err(MarshalError::new(format!(
            "Invalid {} constant ({text}) for \"{receiver}\" of type {}",
            kind.name(),
            crate::names::cql_name_in(ty, dialect)
        )));
    }
    if kind == LiteralKind::Hex {
        let digits = text.get(2..).unwrap_or("");
        return hex_bytes(digits, dialect);
    }
    if matches!(ty, CqlType::Counter) {
        return from_string(&CqlType::Bigint, text, dialect);
    }
    from_string(ty, text, dialect)
}

fn hex_bytes(digits: &str, dialect: Dialect) -> Result<Bytes, MarshalError> {
    match unhex(digits) {
        Some(b) => Ok(Bytes::from(b)),
        None => Err(match dialect {
            Dialect::Cassandra => {
                MarshalError::new(format!("cannot parse '{digits}' as hex bytes"))
            }
            Dialect::Scylla if !digits.len().is_multiple_of(2) => {
                MarshalError::new("A hex string representing bytes must have an even length")
            }
            Dialect::Scylla => MarshalError::new(format!("Non-hex characters in {digits}")),
        }),
    }
}

/// Java's `Byte/Short/Integer/Long.parseX`: an optional sign and decimal digits.
fn java_integer(s: &str) -> Option<i128> {
    let d = s.strip_prefix(['-', '+']).unwrap_or(s);
    if d.is_empty() || d.len() > 30 || !d.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn fixed_int(ty: &CqlType, s: &str, dialect: Dialect) -> Result<Bytes, MarshalError> {
    let (min, max, java, class): (i128, i128, &str, &str) = match ty {
        CqlType::Tinyint => (i8::MIN as i128, i8::MAX as i128, "byte", "ByteType"),
        CqlType::Smallint => (i16::MIN as i128, i16::MAX as i128, "short", "ShortType"),
        CqlType::Int => (i32::MIN as i128, i32::MAX as i128, "int", "Int32Type"),
        _ => (i64::MIN as i128, i64::MAX as i128, "long", "LongType"),
    };
    let v = match dialect {
        Dialect::Cassandra => java_integer(s)
            .filter(|v| (min..=max).contains(v))
            .ok_or_else(|| MarshalError::new(format!("Unable to make {java} from '{s}'")))?,
        Dialect::Scylla => {
            let v = java_integer(s)
                .ok_or_else(|| MarshalError::scylla(format!("Invalid number format '{s}'")))?;
            if !(i64::MIN as i128..=i64::MAX as i128).contains(&v) {
                return Err(MarshalError::scylla(format!("Invalid number format '{s}'")));
            }
            if !(min..=max).contains(&v) {
                return Err(MarshalError::scylla(format!(
                    "Value out of range for type org.apache.cassandra.db.marshal.{class}: '{s}'"
                )));
            }
            v
        }
    };
    Ok(Bytes::from(match ty {
        CqlType::Tinyint => vec![v as i8 as u8],
        CqlType::Smallint => (v as i16).to_be_bytes().to_vec(),
        CqlType::Int => (v as i32).to_be_bytes().to_vec(),
        _ => (v as i64).to_be_bytes().to_vec(),
    }))
}

/// ScyllaDB's varint text: decimal, or octal with a leading zero (`-00012` is -10).
fn scylla_varint(s: &str) -> Option<num_bigint::BigInt> {
    let (neg, d) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if d.len() > 1 && d.starts_with('0') {
        if !d.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            return None;
        }
        let v = num_bigint::BigInt::parse_bytes(d.as_bytes(), 8)?;
        return Some(if neg { -v } else { v });
    }
    parse_varint(s)
}

/// `AbstractType.fromString`: the serialised value of `s` read as `ty`.
pub fn from_string(ty: &CqlType, s: &str, dialect: Dialect) -> Result<Bytes, MarshalError> {
    use CqlType as T;
    let scylla = dialect.is_scylla();
    let err = |m: String| {
        if scylla {
            MarshalError::scylla(m)
        } else {
            MarshalError::new(m)
        }
    };
    // Every scalar except `date` and `time` (Cassandra) maps the empty string to an empty value.
    if s.is_empty()
        && !matches!(ty, T::Ascii | T::Text | T::Blob | T::Boolean | T::Empty)
        && (scylla || !matches!(ty, T::Date | T::Time))
    {
        return Ok(Bytes::new());
    }
    Ok(match ty {
        T::Text => Bytes::copy_from_slice(s.as_bytes()),
        T::Ascii => {
            if let Some(c) = s.chars().find(|c| !c.is_ascii()) {
                return Err(if scylla {
                    MarshalError::scylla(format!(
                        "Invalid ASCII character in string literal: '{s}'"
                    ))
                } else {
                    MarshalError::new(format!(
                        "Invalid ASCII character in string literal: java.nio.charset.UnmappableCharacterException: Input length = {}",
                        c.len_utf16()
                    ))
                });
            }
            Bytes::copy_from_slice(s.as_bytes())
        }
        T::Blob | T::Custom(_) => hex_bytes(s, dialect)?,
        T::Empty => {
            if !s.is_empty() {
                return Err(err(format!("'{s}' is not empty")));
            }
            Bytes::new()
        }
        T::Boolean => {
            if s.is_empty() || s.eq_ignore_ascii_case("false") {
                Bytes::from_static(&[0])
            } else if s.eq_ignore_ascii_case("true") {
                Bytes::from_static(&[1])
            } else {
                return Err(err(format!("Unable to make boolean from '{s}'")));
            }
        }
        T::Tinyint | T::Smallint | T::Int | T::Bigint | T::Counter => fixed_int(ty, s, dialect)?,
        T::Varint => {
            let v = if scylla {
                scylla_varint(s)
            } else {
                parse_varint(s)
            };
            let v = v.ok_or_else(|| err(format!("unable to make int from '{s}'")))?;
            Bytes::from(v.to_signed_bytes_be())
        }
        T::Decimal => {
            let d = Decimal::parse(s)
                .ok_or_else(|| err(format!("unable to make BigDecimal from '{s}'")))?;
            Bytes::from(d.to_bytes())
        }
        T::Float => {
            let v = if scylla {
                float::parse_scylla_float(s)
            } else {
                float::parse_java_float(s)
            };
            let v = v.ok_or_else(|| {
                if scylla {
                    MarshalError::scylla(format!("Invalid number format '{s}'"))
                } else {
                    MarshalError::new(format!("Unable to make float from '{s}'"))
                }
            })?;
            Bytes::copy_from_slice(&v.to_be_bytes())
        }
        T::Double => {
            let v = if scylla {
                float::parse_scylla_double(s)
            } else {
                float::parse_java_double(s)
            };
            let v = v.ok_or_else(|| {
                if scylla {
                    MarshalError::scylla(format!("Invalid number format '{s}'"))
                } else {
                    MarshalError::new(format!("Unable to make double from '{s}'"))
                }
            })?;
            Bytes::copy_from_slice(&v.to_be_bytes())
        }
        T::Uuid | T::Timeuuid => {
            let u = timeuuid::parse_uuid(s).ok_or_else(|| match (ty, dialect) {
                (_, Dialect::Scylla) => {
                    MarshalError::scylla(format!("Cannot parse uuid from '{s}'"))
                }
                (T::Uuid, _) => MarshalError::new(format!("Unable to make UUID from '{s}'")),
                _ => MarshalError::new(format!("Unknown timeuuid representation: {s}")),
            })?;
            if matches!(ty, T::Timeuuid) && timeuuid::version(&u) != 1 {
                return Err(if scylla {
                    MarshalError::scylla(format!(
                        "Unsupported UUID version ({})",
                        timeuuid::version(&u)
                    ))
                } else {
                    MarshalError::new("TimeUUID supports only version 1 UUIDs")
                });
            }
            Bytes::copy_from_slice(&u)
        }
        T::Inet => match inet::parse_inet(s, dialect) {
            Some(b) => Bytes::from(b),
            None => {
                return Err(if scylla {
                    MarshalError::scylla(format!("Failed to parse inet_addr from '{s}'"))
                } else {
                    MarshalError::new(format!("Unable to make inet address from '{s}'"))
                });
            }
        },
        T::Timestamp => {
            Bytes::copy_from_slice(&temporal::parse_timestamp(s, dialect)?.to_be_bytes())
        }
        T::Date => Bytes::copy_from_slice(&temporal::parse_date(s, dialect)?.to_be_bytes()),
        T::Time => Bytes::copy_from_slice(&temporal::parse_time(s, dialect)?.to_be_bytes()),
        T::Duration => Bytes::from(crate::Duration::parse(s, dialect)?.encode()),
        // `TupleType.fromString`: fields split on unescaped ':' (`\:` and `\@` escape), `@`
        // for null, fewer fields than the type allowed.
        T::Tuple(types) => {
            let mut fields: Vec<String> = Vec::new();
            let mut cur = String::new();
            let mut prev = '\0';
            for c in s.chars() {
                if c == ':' && prev != '\\' {
                    fields.push(std::mem::take(&mut cur));
                } else {
                    cur.push(c);
                }
                prev = c;
            }
            fields.push(cur);
            if fields.len() > types.len() {
                return Err(err(format!(
                    "Invalid tuple literal: too many elements. Type {} expects {} but got {}",
                    crate::names::cql_name_in(ty, dialect),
                    types.len(),
                    fields.len()
                )));
            }
            let mut values: Vec<Option<Bytes>> = Vec::with_capacity(fields.len());
            for (f, t) in fields.iter().zip(types) {
                if f == "@" {
                    values.push(None);
                    continue;
                }
                let f = f.replace("\\:", ":").replace("\\@", "@");
                values.push(Some(from_string(t, &f, dialect)?));
            }
            crate::value::pack_fields(values.iter().map(|v| v.as_deref()))
        }
        T::List { .. } | T::Set { .. } | T::Map { .. } | T::Udt(..) | T::Vector { .. } => {
            return Err(err(format!(
                "Cannot parse a {} from a string",
                crate::names::cql_name_in(ty, dialect)
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use LiteralKind as K;

    const C: Dialect = Dialect::Cassandra;
    const S: Dialect = Dialect::Scylla;

    fn lit(kind: K, text: &str, col: &str, ty: &CqlType, d: Dialect) -> Result<Vec<u8>, String> {
        literal_to_bytes(kind, text, col, ty, d)
            .map(|b| b.to_vec())
            .map_err(|e| e.message)
    }

    #[test]
    fn assignment_errors_both_dialects() {
        use CqlType as T;
        // INSERT … VALUES (…) on both images.
        for (kind, text, col, ty) in [
            (K::Integer, "1", "t", T::Text),
            (K::String, "true", "b", T::Boolean),
            (K::String, "NaN", "f", T::Float),
            (K::Float, "1.5", "vi", T::Varint),
            (K::Float, "1.5", "i", T::Int),
            (K::Boolean, "true", "i", T::Int),
            (K::Boolean, "true", "t", T::Text),
            (K::Hex, "0x01", "t", T::Text),
            (K::String, "cafe", "bl", T::Blob),
            (K::String, "", "dec", T::Decimal),
            (K::String, "", "u", T::Uuid),
            (K::String, "", "du", T::Duration),
        ] {
            for d in [C, S] {
                let e = lit(kind, text, col, &ty, d).unwrap_err();
                assert_eq!(
                    e,
                    format!(
                        "Invalid {} constant ({text}) for \"{col}\" of type {}",
                        kind.name(),
                        crate::names::cql_name(&ty)
                    )
                );
            }
        }
        assert_eq!(
            lit(K::Integer, "86400000000000", "tm", &T::Time, S).unwrap_err(),
            "Invalid INTEGER constant (86400000000000) for \"tm\" of type time"
        );
        assert_eq!(
            lit(K::Integer, "5", "du", &T::Duration, S).unwrap_err(),
            "Invalid INTEGER constant (5) for \"du\" of type duration"
        );
        assert_eq!(
            lit(K::Integer, "5", "du", &T::Duration, C).unwrap_err(),
            "Unable to convert '5' to a duration"
        );
        assert_eq!(
            lit(K::Hex, "0x01", "ti", &T::Tinyint, S).unwrap_err(),
            "Invalid HEX constant (0x01) for \"ti\" of type tinyint"
        );
        // Cassandra takes the hex bytes as is.
        assert_eq!(lit(K::Hex, "0x01", "ti", &T::Tinyint, C).unwrap(), vec![1]);
        assert_eq!(
            lit(K::Integer, "5", "tm", &T::Time, C).unwrap(),
            5i64.to_be_bytes().to_vec()
        );
        assert_eq!(
            lit(K::Integer, "5", "dt", &T::Date, S).unwrap(),
            5u32.to_be_bytes().to_vec()
        );
        assert_eq!(
            lit(K::Integer, "5", "ts", &T::Timestamp, S).unwrap(),
            5i64.to_be_bytes().to_vec()
        );
        assert_eq!(
            lit(K::Integer, "7", "c", &T::Counter, C).unwrap(),
            7i64.to_be_bytes().to_vec()
        );
        assert_eq!(
            lit(K::Hex, "0x", "bl", &T::Blob, S).unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(
            lit(K::Hex, "0xabc", "bl", &T::Blob, C).unwrap_err(),
            "cannot parse 'abc' as hex bytes"
        );
        assert_eq!(
            lit(K::Hex, "0xabc", "bl", &T::Blob, S).unwrap_err(),
            "A hex string representing bytes must have an even length"
        );
        assert_eq!(
            lit(K::Duration, "P1Y", "du", &T::Duration, S).unwrap(),
            crate::Duration::new(12, 0, 0).encode()
        );
        assert_eq!(
            lit(
                K::Uuid,
                "50554d6e-29bb-11e5-b345-feff819cdc9f",
                "u",
                &T::Uuid,
                C
            )
            .unwrap()
            .len(),
            16
        );
        assert!(lit(K::Integer, "1", "c", &T::list(T::Int, false), C).is_err());
        assert_eq!(
            lit(K::String, "x", "c", &T::Custom("a.B".into()), C).unwrap_err(),
            "cannot parse 'x' as hex bytes"
        );
    }

    #[test]
    fn integer_ranges() {
        use CqlType as T;
        for (ty, text, c, s) in [
            (
                T::Tinyint,
                "128",
                "Unable to make byte from '128'",
                "Value out of range for type org.apache.cassandra.db.marshal.ByteType: '128'",
            ),
            (
                T::Smallint,
                "40000",
                "Unable to make short from '40000'",
                "Value out of range for type org.apache.cassandra.db.marshal.ShortType: '40000'",
            ),
            (
                T::Smallint,
                "-32769",
                "Unable to make short from '-32769'",
                "Value out of range for type org.apache.cassandra.db.marshal.ShortType: '-32769'",
            ),
            (
                T::Int,
                "2147483648",
                "Unable to make int from '2147483648'",
                "Value out of range for type org.apache.cassandra.db.marshal.Int32Type: '2147483648'",
            ),
            (
                T::Bigint,
                "9223372036854775808",
                "Unable to make long from '9223372036854775808'",
                "Invalid number format '9223372036854775808'",
            ),
            (
                T::Int,
                "abc",
                "Unable to make int from 'abc'",
                "Invalid number format 'abc'",
            ),
            (
                T::Int,
                " 5",
                "Unable to make int from ' 5'",
                "Invalid number format ' 5'",
            ),
            (
                T::Int,
                "1.0",
                "Unable to make int from '1.0'",
                "Invalid number format '1.0'",
            ),
            (
                T::Int,
                "x",
                "Unable to make int from 'x'",
                "Invalid number format 'x'",
            ),
        ] {
            assert_eq!(
                from_string(&ty, text, C).unwrap_err().message,
                c,
                "{ty:?} {text}"
            );
            assert_eq!(
                from_string(&ty, text, S).unwrap_err().message,
                format!("marshaling error: {s}"),
                "{ty:?} {text}"
            );
        }
        for d in [C, S] {
            assert_eq!(
                &from_string(&T::Bigint, "-9223372036854775808", d).unwrap()[..],
                &i64::MIN.to_be_bytes()
            );
            assert_eq!(&from_string(&T::Tinyint, "-128", d).unwrap()[..], &[0x80]);
            assert_eq!(&from_string(&T::Int, "-0", d).unwrap()[..], &[0, 0, 0, 0]);
            assert_eq!(&from_string(&T::Int, "+5", d).unwrap()[..], &[0, 0, 0, 5]);
            assert!(from_string(&T::Int, "", d).unwrap().is_empty());
        }
        assert_eq!(&from_string(&T::Varint, "-00012", C).unwrap()[..], &[0xf4]);
        assert_eq!(&from_string(&T::Varint, "-00012", S).unwrap()[..], &[0xf6]);
        assert_eq!(
            from_string(&T::Varint, "1.5", C).unwrap_err().message,
            "unable to make int from '1.5'"
        );
        assert_eq!(
            from_string(&T::Varint, "1.5", S).unwrap_err().message,
            "marshaling error: unable to make int from '1.5'"
        );
        assert_eq!(
            from_string(&T::Varint, "09", S).unwrap_err().message,
            "marshaling error: unable to make int from '09'"
        );
    }

    #[test]
    fn floats_and_decimals() {
        use CqlType as T;
        assert_eq!(
            &from_string(&T::Float, "1e40", C).unwrap()[..],
            &f32::INFINITY.to_be_bytes()
        );
        assert_eq!(
            from_string(&T::Float, "1e40", S).unwrap_err().message,
            "marshaling error: Invalid number format '1e40'"
        );
        assert_eq!(
            from_string(&T::Double, "1e400", S).unwrap_err().message,
            "marshaling error: Invalid number format '1e400'"
        );
        assert_eq!(
            &from_string(&T::Float, "-1e-50", S).unwrap()[..],
            &(-0.0f32).to_be_bytes()
        );
        assert_eq!(
            &from_string(&T::Double, "1e-400", S).unwrap()[..],
            &0.0f64.to_be_bytes()
        );
        assert!(
            f64::from_be_bytes(
                from_string(&T::Double, "-NaN", C).unwrap()[..]
                    .try_into()
                    .unwrap()
            )
            .is_nan()
        );
        assert!(
            f64::from_be_bytes(
                from_string(&T::Double, "-NaN", S).unwrap()[..]
                    .try_into()
                    .unwrap()
            )
            .is_nan()
        );
        assert_eq!(
            from_string(&T::Float, "abc", C).unwrap_err().message,
            "Unable to make float from 'abc'"
        );
        assert_eq!(
            from_string(&T::Double, "abc", C).unwrap_err().message,
            "Unable to make double from 'abc'"
        );
        assert_eq!(
            from_string(&T::Double, "0x10", S).unwrap_err().message,
            "marshaling error: Invalid number format '0x10'"
        );
        for d in [C, S] {
            assert_eq!(
                from_string(&T::Decimal, "1.5e3", d).unwrap()[..],
                Decimal::parse("1.5e3").unwrap().to_bytes()[..]
            );
        }
        assert_eq!(
            from_string(&T::Decimal, "NaN", C).unwrap_err().message,
            "unable to make BigDecimal from 'NaN'"
        );
        assert_eq!(
            from_string(&T::Decimal, "Infinity", S).unwrap_err().message,
            "marshaling error: unable to make BigDecimal from 'Infinity'"
        );
    }

    #[test]
    fn other_types() {
        use CqlType as T;
        assert_eq!(
            from_string(&T::Ascii, "aé", C).unwrap_err().message,
            "Invalid ASCII character in string literal: java.nio.charset.UnmappableCharacterException: Input length = 1"
        );
        assert_eq!(
            from_string(&T::Ascii, "aé", S).unwrap_err().message,
            "marshaling error: Invalid ASCII character in string literal: 'aé'"
        );
        assert_eq!(&from_string(&T::Ascii, "abc", S).unwrap()[..], b"abc");
        assert_eq!(&from_string(&T::Text, "", C).unwrap()[..], b"");
        assert_eq!(&from_string(&T::Boolean, "TRUE", C).unwrap()[..], &[1]);
        assert_eq!(&from_string(&T::Boolean, "", C).unwrap()[..], &[0]);
        assert_eq!(
            from_string(&T::Boolean, "yes", C).unwrap_err().message,
            "Unable to make boolean from 'yes'"
        );
        assert_eq!(
            from_string(&T::Timeuuid, "00000000-0000-0000-0000-000000000000", C)
                .unwrap_err()
                .message,
            "TimeUUID supports only version 1 UUIDs"
        );
        assert_eq!(
            from_string(&T::Timeuuid, "00000000-0000-0000-0000-000000000000", S)
                .unwrap_err()
                .message,
            "marshaling error: Unsupported UUID version (0)"
        );
        assert_eq!(
            from_string(&T::Timeuuid, "550e8400-e29b-41d4-a716-446655440000", S)
                .unwrap_err()
                .message,
            "marshaling error: Unsupported UUID version (4)"
        );
        assert_eq!(
            from_string(&T::Uuid, "zzz", C).unwrap_err().message,
            "Unable to make UUID from 'zzz'"
        );
        assert_eq!(
            from_string(&T::Uuid, "zzz", S).unwrap_err().message,
            "marshaling error: Cannot parse uuid from 'zzz'"
        );
        assert_eq!(
            from_string(&T::Timeuuid, "zzz", C).unwrap_err().message,
            "Unknown timeuuid representation: zzz"
        );
        assert_eq!(
            from_string(&T::Inet, "1.2.3.256", C).unwrap_err().message,
            "Unable to make inet address from '1.2.3.256'"
        );
        assert_eq!(
            from_string(&T::Inet, "1.2.3", S).unwrap_err().message,
            "marshaling error: Failed to parse inet_addr from '1.2.3'"
        );
        assert!(from_string(&T::Inet, "", C).unwrap().is_empty());
        assert!(from_string(&T::Timestamp, "", C).unwrap().is_empty());
        assert!(from_string(&T::Date, "", S).unwrap().is_empty());
        assert!(from_string(&T::Time, "", S).unwrap().is_empty());
        assert_eq!(
            from_string(&T::Date, "", C).unwrap_err().message,
            "Unable to coerce '' to a formatted date (long)"
        );
        assert_eq!(
            from_string(&T::Time, "", C).unwrap_err().message,
            "(TimeType) Unable to coerce '' to a formatted time (long)"
        );
        assert_eq!(
            from_string(&T::Duration, "xyz", S).unwrap_err().message,
            "marshaling error: Unable to convert 'xyz' to a duration"
        );
        assert_eq!(
            from_string(&T::Empty, "x", C).unwrap_err().message,
            "'x' is not empty"
        );
        assert!(from_string(&T::Empty, "", C).unwrap().is_empty());
        assert_eq!(
            &from_string(&T::Blob, "CAFE", C).unwrap()[..],
            &[0xca, 0xfe]
        );
        assert_eq!(
            from_string(&T::Blob, "zz", S).unwrap_err().message,
            "Non-hex characters in zz"
        );
        assert!(from_string(&T::list(T::Int, false), "[1]", C).is_err());
        assert!(from_string(&T::Timestamp, "garbage", C).is_err());
    }
}

#[cfg(test)]
mod assign_tests {
    use super::*;

    #[test]
    fn assignability_table() {
        use CqlType as T;
        use LiteralKind as K;
        let c = Dialect::Cassandra;
        let s = Dialect::Scylla;
        for (k, n) in [
            (K::String, "STRING"),
            (K::Integer, "INTEGER"),
            (K::Uuid, "UUID"),
            (K::Float, "FLOAT"),
            (K::Boolean, "BOOLEAN"),
            (K::Hex, "HEX"),
            (K::Duration, "DURATION"),
        ] {
            assert_eq!(k.name(), n);
        }
        for t in [T::Ascii, T::Text, T::Inet, T::Date, T::Time, T::Timestamp] {
            assert!(is_assignable(K::String, &t, c), "{t:?}");
        }
        for t in [
            T::Int,
            T::Blob,
            T::Uuid,
            T::Boolean,
            T::Duration,
            T::Decimal,
        ] {
            assert!(!is_assignable(K::String, &t, c), "{t:?}");
        }
        for t in [
            T::Bigint,
            T::Counter,
            T::Date,
            T::Decimal,
            T::Double,
            T::Float,
            T::Int,
            T::Smallint,
            T::Timestamp,
            T::Tinyint,
            T::Varint,
        ] {
            assert!(
                is_assignable(K::Integer, &t, c) && is_assignable(K::Integer, &t, s),
                "{t:?}"
            );
        }
        assert!(is_assignable(K::Integer, &T::Time, c) && !is_assignable(K::Integer, &T::Time, s));
        assert!(
            is_assignable(K::Integer, &T::Duration, c)
                && !is_assignable(K::Integer, &T::Duration, s)
        );
        assert!(is_assignable(K::Uuid, &T::Timeuuid, c) && !is_assignable(K::Uuid, &T::Text, c));
        assert!(is_assignable(K::Float, &T::Decimal, s) && !is_assignable(K::Float, &T::Int, s));
        assert!(
            is_assignable(K::Boolean, &T::Boolean, s) && !is_assignable(K::Boolean, &T::Text, s)
        );
        assert!(
            is_assignable(K::Duration, &T::Duration, s) && !is_assignable(K::Duration, &T::Time, s)
        );
        assert!(is_assignable(K::Hex, &T::Tinyint, c) && !is_assignable(K::Hex, &T::Tinyint, s));
        assert!(is_assignable(K::String, &T::Custom("x".into()), s));
        for t in [T::list(T::Int, false), T::vector(T::Float, 1)] {
            assert!(!is_assignable(K::String, &t, c));
        }
        assert!(is_assignable(K::String, &T::Tuple(vec![T::Int]), c));
        assert!(!is_assignable(K::Integer, &T::Tuple(vec![T::Int]), c));
    }

    /// `TupleType.fromString`, checked on both references (`tuple_type_test.testTupleFromString`).
    #[test]
    fn tuple_from_string() {
        let t = CqlType::Tuple(vec![CqlType::Int, CqlType::Text]);
        let i = |v: i32| v.to_be_bytes().to_vec();
        for d in [Dialect::Scylla, Dialect::Cassandra] {
            let f = |s: &str| from_string(&t, s, d).map(|b| b.to_vec());
            let fields =
                |fs: &[Option<&[u8]>]| crate::value::pack_fields(fs.iter().copied()).to_vec();
            assert_eq!(f("0:0"), Ok(fields(&[Some(&i(0)), Some(b"0")])));
            assert_eq!(f("1"), Ok(fields(&[Some(&i(1))])));
            assert_eq!(f("1:1\\:1"), Ok(fields(&[Some(&i(1)), Some(b"1:1")])));
            assert_eq!(f("@:1"), Ok(fields(&[None, Some(b"1")])));
        }
        let msg = |s: &str, d| from_string(&t, s, d).unwrap_err().message;
        assert_eq!(
            msg("1:2:3", Dialect::Scylla),
            "marshaling error: Invalid tuple literal: too many elements. Type frozen<tuple<int, text>> expects 2 but got 3"
        );
        assert_eq!(
            msg("1:2:3", Dialect::Cassandra),
            "Invalid tuple literal: too many elements. Type frozen<tuple<int, text>> expects 2 but got 3"
        );
        assert_eq!(
            msg("x:2", Dialect::Scylla),
            "marshaling error: Invalid number format 'x'"
        );
        assert_eq!(
            msg("x:2", Dialect::Cassandra),
            "Unable to make int from 'x'"
        );
    }
}
