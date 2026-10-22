//! Text forms of values.
//!
//! - [`to_text`]: what `CAST(x AS text)` returns (Cassandra: `TypeSerializer.toCQLLiteralNoQuote`
//!   via `CastFcts`, Apache-2.0; ScyllaDB: from the image).
//! - [`to_cql_literal`]: a CQL literal that reads back as the same value (Cassandra's
//!   `CQL3Type.toCQLLiteral`, as DESCRIBE uses it): strings and temporal values quoted, blobs as
//!   `0x…`, collections, tuples and UDTs in literal syntax, empty values of most types as `null`.
//! - [`to_cqlsh`]: what cqlsh prints (client-side, so identical for both references): floats with
//!   5 and doubles with 12 significant digits, `True`/`False`, timestamps as
//!   `2026-10-20 12:00:00.123000+0000`, nested strings quoted, null components as `None`.

use crate::inet::format_inet;
use crate::num::Decimal;
use crate::temporal::{
    format_date, format_date_cqlsh, format_time, format_timestamp, format_timestamp_cqlsh,
};
use crate::timeuuid::format_uuid;
use crate::value::{decode, split_vector};
use crate::wire::{self, Collection};
use crate::{CqlType, Dialect, MarshalError, float};

fn uuid16(b: &[u8]) -> Result<[u8; 16], MarshalError> {
    b.try_into()
        .map_err(|_| MarshalError::new(format!("UUID should be 16 or 0 bytes ({})", b.len())))
}

fn exact<const N: usize>(b: &[u8], what: &str) -> Result<[u8; N], MarshalError> {
    b.try_into()
        .map_err(|_| MarshalError::new(format!("Expected {N} bytes for a {what} ({})", b.len())))
}

/// Text of a scalar value (no quoting). `None` for the compound types.
pub(crate) fn scalar_text(
    ty: &CqlType,
    b: &[u8],
    dialect: Dialect,
) -> Result<Option<String>, MarshalError> {
    use CqlType as T;
    let scylla = dialect.is_scylla();
    Ok(Some(match ty {
        T::Ascii | T::Text => String::from_utf8_lossy(b).into_owned(),
        T::Blob | T::Custom(_) | T::Empty => format!("0x{}", wire::hex(b)),
        T::Boolean => (exact::<1>(b, "boolean")?[0] != 0).to_string(),
        T::Tinyint => (exact::<1>(b, "tinyint")?[0] as i8).to_string(),
        T::Smallint => i16::from_be_bytes(exact(b, "smallint")?).to_string(),
        T::Int => i32::from_be_bytes(exact(b, "int")?).to_string(),
        T::Bigint | T::Counter => i64::from_be_bytes(exact(b, "bigint")?).to_string(),
        T::Float => {
            let x = f32::from_be_bytes(exact(b, "float")?);
            if scylla {
                float::scylla_float(x)
            } else {
                float::java_float(x)
            }
        }
        T::Double => {
            let x = f64::from_be_bytes(exact(b, "double")?);
            if scylla {
                float::scylla_double(x)
            } else {
                float::java_double(x)
            }
        }
        T::Decimal => {
            let d = Decimal::from_bytes(b)
                .ok_or_else(|| MarshalError::new("Not enough bytes to read a decimal"))?;
            if scylla {
                d.to_scylla_string()
            } else {
                d.to_java_string()
            }
        }
        T::Varint => crate::num::varint_from_bytes(b).to_string(),
        T::Timestamp => format_timestamp(i64::from_be_bytes(exact(b, "timestamp")?), 'T', dialect),
        T::Date => format_date(u32::from_be_bytes(exact(b, "date")?), dialect),
        T::Time => format_time(i64::from_be_bytes(exact(b, "time")?)),
        T::Uuid | T::Timeuuid => format_uuid(&uuid16(b)?),
        T::Inet => format_inet(b, dialect).ok_or_else(|| {
            MarshalError::new(format!(
                "Expected 4 or 16 byte inetaddress; got {}",
                wire::hex(b)
            ))
        })?,
        T::Duration => crate::Duration::decode(b)
            .ok_or_else(|| MarshalError::new("Not enough bytes to read a duration"))?
            .to_text(),
        _ => return Ok(None),
    }))
}

/// `CAST(x AS text)` / `CAST(x AS ascii)`. `None` (null) for an empty value of a type where empty
/// means null. Compound types print as their CQL literal.
pub fn to_text(ty: &CqlType, b: &[u8], dialect: Dialect) -> Result<Option<String>, MarshalError> {
    if b.is_empty() && ty.is_empty_value_meaningless() {
        return Ok(None);
    }
    match scalar_text(ty, b, dialect)? {
        Some(s) => Ok(Some(s)),
        None => to_cql_literal(ty, b, dialect).map(Some),
    }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A CQL literal for the value (`CQL3Type.toCQLLiteral`).
pub fn to_cql_literal(ty: &CqlType, b: &[u8], dialect: Dialect) -> Result<String, MarshalError> {
    let mut out = String::new();
    literal_into(&mut out, ty, b, dialect)?;
    Ok(out)
}

fn literal_into(
    out: &mut String,
    ty: &CqlType,
    b: &[u8],
    dialect: Dialect,
) -> Result<(), MarshalError> {
    use CqlType as T;
    if b.is_empty() && !matches!(ty, T::Ascii | T::Text | T::Blob | T::Custom(_)) {
        out.push_str("null");
        return Ok(());
    }
    match ty {
        T::Ascii | T::Text => out.push_str(&quote(&String::from_utf8_lossy(b))),
        T::Timestamp | T::Date | T::Time | T::Inet => {
            out.push_str(&quote(&scalar_text(ty, b, dialect)?.unwrap_or_default()));
        }
        T::List { elem, .. } | T::Set { elem, .. } => {
            let (open, close) = if matches!(ty, T::List { .. }) {
                ('[', ']')
            } else {
                ('{', '}')
            };
            let c = Collection::parse(b)
                .ok_or_else(|| MarshalError::new("Not enough bytes to read a list"))?;
            out.push(open);
            for (i, item) in c.items(1).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                match item.map_err(|_| MarshalError::new("Not enough bytes to read a list"))? {
                    Some(v) => literal_into(out, elem, v, dialect)?,
                    None => out.push_str("null"),
                }
            }
            out.push(close);
        }
        T::Map { key, value, .. } => {
            let entries = crate::value::map_entries(b)?;
            out.push('{');
            for (i, (k, v)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                literal_into(out, key, k, dialect)?;
                out.push_str(": ");
                literal_into(out, value, v, dialect)?;
            }
            out.push('}');
        }
        T::Tuple(ts) => {
            out.push('(');
            for (i, (t, f)) in ts.iter().zip(wire::fields(b)).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                match f.map_err(|_| {
                    MarshalError::new(format!("Not enough bytes to read {i}th component"))
                })? {
                    Some(v) => literal_into(out, t, v, dialect)?,
                    None => out.push_str("null"),
                }
            }
            out.push(')');
        }
        T::Udt(def, _) => {
            out.push('{');
            for (i, ((name, t), f)) in def.fields.iter().zip(wire::fields(b)).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&crate::names::quote_identifier(name, dialect));
                out.push_str(": ");
                match f.map_err(|_| {
                    MarshalError::new(format!("Not enough bytes to read {i}th field {name}"))
                })? {
                    Some(v) => literal_into(out, t, v, dialect)?,
                    None => out.push_str("null"),
                }
            }
            out.push('}');
        }
        T::Vector { elem, dim } => {
            let parts = split_vector(elem, *dim, b).map_err(|_| {
                MarshalError::new(format!(
                    "Not enough bytes to read a {}",
                    crate::names::cql_name(ty)
                ))
            })?;
            out.push('[');
            for (i, p) in parts.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                literal_into(out, elem, p, dialect)?;
            }
            out.push(']');
        }
        _ => out.push_str(&scalar_text(ty, b, dialect)?.unwrap_or_default()),
    }
    Ok(())
}

/// Python's `'%.*g' % (precision, x)`.
fn percent_g(x: f64, precision: usize) -> String {
    let p = precision.max(1);
    let sci = format!("{:.*e}", p - 1, x);
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if exp < -4 || exp >= p as i32 {
        let mant = if mant.contains('.') {
            mant.trim_end_matches('0').trim_end_matches('.')
        } else {
            mant
        };
        format!("{mant}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        let decimals = (p as i32 - 1 - exp).max(0) as usize;
        let s = format!("{:.*}", decimals, x);
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    }
}

/// cqlsh's `format_floating_point_type`.
fn cqlsh_float(x: f64, precision: usize) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    let mut p = precision as i64;
    if x.abs() > f64::EPSILON {
        let exponent = x.abs().log10() as i64;
        if (-4..p).contains(&exponent) {
            p += exponent + 1;
        }
    }
    percent_g(x, p.max(1) as usize)
}

/// The text cqlsh shows for a value (top level: strings unquoted, empty values blank).
pub fn to_cqlsh(ty: &CqlType, b: &[u8]) -> String {
    cqlsh(ty, b, false)
}

fn cqlsh(ty: &CqlType, b: &[u8], nested: bool) -> String {
    use CqlType as T;
    if b.is_empty() && !matches!(ty, T::Ascii | T::Text | T::Blob) {
        return String::new();
    }
    let fallback = || format!("0x{}", wire::hex(b));
    match ty {
        T::Ascii | T::Text => {
            let s = String::from_utf8_lossy(b);
            if nested { quote(&s) } else { s.into_owned() }
        }
        T::Boolean => if b[0] != 0 { "True" } else { "False" }.into(),
        T::Float => match <[u8; 4]>::try_from(b) {
            Ok(a) => cqlsh_float(f32::from_be_bytes(a) as f64, 5),
            Err(_) => fallback(),
        },
        T::Double => match <[u8; 8]>::try_from(b) {
            Ok(a) => cqlsh_float(f64::from_be_bytes(a), 12),
            Err(_) => fallback(),
        },
        T::Timestamp => match <[u8; 8]>::try_from(b) {
            Ok(a) => format_timestamp_cqlsh(i64::from_be_bytes(a)),
            Err(_) => fallback(),
        },
        T::Date => match <[u8; 4]>::try_from(b) {
            Ok(a) => format_date_cqlsh(u32::from_be_bytes(a)),
            Err(_) => fallback(),
        },
        T::Inet => format_inet(b, Dialect::Scylla).unwrap_or_else(fallback),
        T::Decimal => Decimal::from_bytes(b)
            .map(|d| d.to_java_string())
            .unwrap_or_else(fallback),
        T::List { elem, .. } | T::Set { elem, .. } => {
            let (open, close) = if matches!(ty, T::List { .. }) {
                ("[", "]")
            } else {
                ("{", "}")
            };
            let Some(c) = Collection::parse(b) else {
                return fallback();
            };
            let parts: Vec<String> = c
                .items(1)
                .map(|i| match i {
                    Ok(Some(v)) => cqlsh(elem, v, true),
                    _ => "None".into(),
                })
                .collect();
            format!("{open}{}{close}", parts.join(", "))
        }
        T::Map { key, value, .. } => match crate::value::map_entries(b) {
            Ok(es) => {
                let parts: Vec<String> = es
                    .iter()
                    .map(|(k, v)| format!("{}: {}", cqlsh(key, k, true), cqlsh(value, v, true)))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            Err(_) => fallback(),
        },
        T::Tuple(ts) => {
            let fs: Vec<_> = wire::fields(b).collect();
            let parts: Vec<String> = ts
                .iter()
                .enumerate()
                .map(|(i, t)| match fs.get(i) {
                    Some(Ok(Some(v))) => cqlsh(t, v, true),
                    _ => "None".into(),
                })
                .collect();
            format!("({})", parts.join(", "))
        }
        T::Udt(def, _) => {
            let fs: Vec<_> = wire::fields(b).collect();
            let parts: Vec<String> = def
                .fields
                .iter()
                .enumerate()
                .map(|(i, (n, t))| {
                    let v = match fs.get(i) {
                        Some(Ok(Some(v))) => cqlsh(t, v, true),
                        _ => "None".into(),
                    };
                    format!("{n}: {v}")
                })
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        T::Vector { elem, dim } => match split_vector(elem, *dim, b) {
            Ok(ps) => format!(
                "[{}]",
                ps.iter()
                    .map(|p| cqlsh(elem, p, true))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(_) => fallback(),
        },
        _ => match decode(ty, b) {
            Ok(_) => scalar_text(ty, b, Dialect::Cassandra)
                .ok()
                .flatten()
                .unwrap_or_else(fallback),
            Err(_) => fallback(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Value, encode};
    use std::sync::Arc;

    const C: Dialect = Dialect::Cassandra;
    const S: Dialect = Dialect::Scylla;

    fn enc(ty: &CqlType, v: Value) -> Vec<u8> {
        encode(&v, ty).to_vec()
    }

    #[test]
    fn cast_to_text_both_dialects() {
        // `CAST(x AS text)` on both images (row k=1 of the formatting table).
        use CqlType as T;
        let cases: Vec<(CqlType, Value, &str, &str)> = vec![
            (T::Float, Value::Float(1.1), "1.1", "1.1"),
            (
                T::Float,
                Value::Float(123456789.0),
                "1.23456792E8",
                "1.2345679e+08",
            ),
            (T::Double, Value::Double(1e7), "1.0E7", "10000000"),
            (
                T::Double,
                Value::Double(f64::NEG_INFINITY),
                "-Infinity",
                "-inf",
            ),
            (T::Float, Value::Float(f32::NAN), "NaN", "nan"),
            (
                T::Decimal,
                Value::Decimal(Decimal::parse("1.10").unwrap()),
                "1.10",
                "1.1",
            ),
            (
                T::Decimal,
                Value::Decimal(Decimal::parse("1e7").unwrap()),
                "1E+7",
                "10000000",
            ),
            (
                T::Varint,
                Value::Varint("12345678901234567890".parse().unwrap()),
                "12345678901234567890",
                "12345678901234567890",
            ),
            (
                T::Timestamp,
                Value::Timestamp(1_792_497_600_123),
                "2026-10-20T12:00:00.123Z",
                "2026-10-20T12:00:00.123Z",
            ),
            (
                T::Date,
                Value::Date((1u32 << 31) + 20_746),
                "2026-10-20",
                "2026-10-20",
            ),
            (
                T::Time,
                Value::Time(45_296_123_456_789),
                "12:34:56.123456789",
                "12:34:56.123456789",
            ),
            (
                T::Inet,
                Value::Inet("::1".parse().unwrap()),
                "0:0:0:0:0:0:0:1",
                "::1",
            ),
            (
                T::Inet,
                Value::Inet("fe80::1:2".parse().unwrap()),
                "fe80:0:0:0:0:0:1:2",
                "fe80::1:2",
            ),
            (T::Boolean, Value::Boolean(true), "true", "true"),
            (
                T::Bigint,
                Value::Bigint(-9_000_000_000),
                "-9000000000",
                "-9000000000",
            ),
            (T::Tinyint, Value::Tinyint(-5), "-5", "-5"),
            (T::Smallint, Value::Smallint(300), "300", "300"),
            (
                T::Duration,
                Value::Duration(crate::Duration::new(14, 25, 1)),
                "1y2mo25d1ns",
                "1y2mo25d1ns",
            ),
            (
                T::Blob,
                Value::Blob(bytes::Bytes::from_static(&[0xca, 0xfe])),
                "0xcafe",
                "0xcafe",
            ),
            (T::Text, Value::Text("it's".into()), "it's", "it's"),
        ];
        for (t, v, c, s) in cases {
            let b = enc(&t, v);
            assert_eq!(to_text(&t, &b, C).unwrap().as_deref(), Some(c), "{t:?}");
            assert_eq!(to_text(&t, &b, S).unwrap().as_deref(), Some(s), "{t:?}");
        }
        let u = crate::timeuuid::parse_uuid("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            to_text(&T::Uuid, &u, C).unwrap().unwrap(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        // Empty values of "empty means null" types cast to null; strings to "".
        assert_eq!(to_text(&T::Int, &[], C).unwrap(), None);
        assert_eq!(to_text(&T::Text, &[], C).unwrap().as_deref(), Some(""));
        assert_eq!(
            to_text(&T::Tinyint, &[], S),
            Err(MarshalError::new("Expected 1 bytes for a tinyint (0)"))
        );
        assert!(to_text(&T::Int, &[1], C).is_err());
        assert!(to_text(&T::Inet, &[1], C).is_err());
        assert!(to_text(&T::Uuid, &[1], C).is_err());
        assert!(to_text(&T::Decimal, &[1], C).is_err());
        assert!(to_text(&T::Duration, &[0xfe], C).is_err());
    }

    #[test]
    fn cql_literals() {
        use CqlType as T;
        assert_eq!(to_cql_literal(&T::Text, b"it's", C).unwrap(), "'it''s'");
        assert_eq!(to_cql_literal(&T::Text, b"", C).unwrap(), "''");
        assert_eq!(to_cql_literal(&T::Int, &[], C).unwrap(), "null");
        assert_eq!(
            to_cql_literal(&T::Int, &5i32.to_be_bytes(), C).unwrap(),
            "5"
        );
        assert_eq!(to_cql_literal(&T::Blob, &[0xca], C).unwrap(), "0xca");
        assert_eq!(
            to_cql_literal(&T::Custom("x.Y".into()), &[1], C).unwrap(),
            "0x01"
        );
        assert_eq!(
            to_cql_literal(&T::Timestamp, &0i64.to_be_bytes(), C).unwrap(),
            "'1970-01-01T00:00:00.000Z'"
        );
        assert_eq!(
            to_cql_literal(&T::Inet, &[1, 2, 3, 4], C).unwrap(),
            "'1.2.3.4'"
        );
        assert_eq!(
            to_cql_literal(&T::Duration, &crate::Duration::new(0, 1, 0).encode(), C).unwrap(),
            "1d"
        );
        let lt = T::list(T::Text, false);
        let b = enc(
            &lt,
            Value::List(vec![Value::Text("a".into()), Value::Text("b'".into())]),
        );
        assert_eq!(to_cql_literal(&lt, &b, C).unwrap(), "['a', 'b''']");
        let st = T::set(T::Int, true);
        assert_eq!(
            to_cql_literal(
                &st,
                &enc(&st, Value::Set(vec![Value::Int(2), Value::Int(1)])),
                C
            )
            .unwrap(),
            "{1, 2}"
        );
        let mt = T::map(T::Text, T::Double, false);
        let b = enc(
            &mt,
            Value::Map(vec![(Value::Text("x".into()), Value::Double(1.5))]),
        );
        assert_eq!(to_cql_literal(&mt, &b, C).unwrap(), "{'x': 1.5}");
        let tt = T::Tuple(vec![T::Int, T::Text]);
        let b = enc(
            &tt,
            Value::Tuple(vec![Value::Null, Value::Text("q".into())]),
        );
        assert_eq!(to_cql_literal(&tt, &b, C).unwrap(), "(null, 'q')");
        let ut = T::Udt(
            Arc::new(crate::UdtDef::new(
                "k",
                "u",
                vec![("Zip".into(), T::Int), ("s".into(), T::Text)],
            )),
            true,
        );
        let b = enc(&ut, Value::Udt(vec![Value::Int(1)]));
        assert_eq!(to_cql_literal(&ut, &b, C).unwrap(), "{\"Zip\": 1}");
        let vt = T::vector(T::Float, 2);
        let b = enc(
            &vt,
            Value::Vector(vec![Value::Float(1.0), Value::Float(2.5)]),
        );
        assert_eq!(to_cql_literal(&vt, &b, C).unwrap(), "[1.0, 2.5]");
        assert_eq!(to_cql_literal(&vt, &b, S).unwrap(), "[1, 2.5]");
        assert_eq!(to_text(&vt, &b, S).unwrap().unwrap(), "[1, 2.5]");
        assert!(to_cql_literal(&lt, &[0, 0, 0, 1], C).is_err());
        assert!(to_cql_literal(&vt, &[0, 0], C).is_err());
        assert!(to_cql_literal(&mt, &[0, 0], C).is_err());
    }

    #[test]
    fn cqlsh_output() {
        // What cqlsh printed for the values above (same on both images).
        use CqlType as T;
        let f = |x: f32| to_cqlsh(&T::Float, &x.to_be_bytes());
        let d = |x: f64| to_cqlsh(&T::Double, &x.to_be_bytes());
        assert_eq!(f(f32::NEG_INFINITY), "-Infinity");
        assert_eq!(f(-1.5), "-1.5");
        assert_eq!(f(-0.0), "-0");
        assert_eq!(f(0.0), "0");
        assert_eq!(f(f32::NAN), "NaN");
        assert_eq!(d(-1e-300), "-1e-300");
        assert_eq!(d(1e300), "1e+300");
        assert_eq!(d(1234567.0), "1234567");
        assert_eq!(f(1.1), "1.1");
        assert_eq!(to_cqlsh(&T::Float, &[]), "");
        assert_eq!(to_cqlsh(&T::Boolean, &[0]), "False");
        assert_eq!(to_cqlsh(&T::Boolean, &[1]), "True");
        assert_eq!(
            to_cqlsh(&T::Timestamp, &1_792_497_600_123i64.to_be_bytes()),
            "2026-10-20 12:00:00.123000+0000"
        );
        assert_eq!(to_cqlsh(&T::Date, &u32::MAX.to_be_bytes()), "2147483647");
        assert_eq!(
            to_cqlsh(
                &T::Inet,
                &"::1.2.3.4".parse::<std::net::Ipv6Addr>().unwrap().octets()
            ),
            "::1.2.3.4"
        );
        assert_eq!(
            to_cqlsh(&T::Decimal, &Decimal::parse("-1e10").unwrap().to_bytes()),
            "-1E+10"
        );
        assert_eq!(to_cqlsh(&T::Text, b"x"), "x");
        assert_eq!(to_cqlsh(&T::Blob, &[0xca]), "0xca");
        assert_eq!(to_cqlsh(&T::Int, &7i32.to_be_bytes()), "7");
        assert_eq!(
            to_cqlsh(&T::Time, &0i64.to_be_bytes()),
            "00:00:00.000000000"
        );
        let tt = T::Tuple(vec![T::Int, T::Text]);
        let b = enc(
            &tt,
            Value::Tuple(vec![Value::Null, Value::Text("a".into())]),
        );
        assert_eq!(to_cqlsh(&tt, &b), "(None, 'a')");
        let lt = T::list(T::Int, true);
        assert_eq!(
            to_cqlsh(
                &lt,
                &enc(&lt, Value::List(vec![Value::Int(-1), Value::Int(5)]))
            ),
            "[-1, 5]"
        );
        let mt = T::map(T::Int, T::Text, false);
        assert_eq!(
            to_cqlsh(
                &mt,
                &enc(
                    &mt,
                    Value::Map(vec![(Value::Int(1), Value::Text("a".into()))])
                )
            ),
            "{1: 'a'}"
        );
        let st = T::set(T::Text, false);
        assert_eq!(
            to_cqlsh(&st, &enc(&st, Value::Set(vec![Value::Text("b".into())]))),
            "{'b'}"
        );
        let ut = T::Udt(
            Arc::new(crate::UdtDef::new(
                "k",
                "u",
                vec![("street".into(), T::Text), ("Zip".into(), T::Int)],
            )),
            true,
        );
        assert_eq!(
            to_cqlsh(&ut, &enc(&ut, Value::Udt(vec![Value::Text("st".into())]))),
            "{street: 'st', Zip: None}"
        );
        let vt = T::vector(T::Int, 2);
        assert_eq!(
            to_cqlsh(
                &vt,
                &enc(&vt, Value::Vector(vec![Value::Int(1), Value::Int(-1)]))
            ),
            "[1, -1]"
        );
        assert_eq!(to_cqlsh(&lt, &[0, 0]), "0x0000");
        assert_eq!(to_cqlsh(&T::Int, &[1]), "0x01");
        assert_eq!(percent_g(0.0001, 5), "0.0001");
        assert_eq!(percent_g(123456.0, 5), "1.2346e+05");
    }
}
