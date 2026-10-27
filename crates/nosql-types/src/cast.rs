//! `CAST(x AS type)` and the temporal conversion functions.
//!
//! Cassandra's table and conversions follow `cql3.functions.CastFcts` and `TimeFcts`
//! (Apache-2.0): numeric types convert with Java's `Number` narrowing (`(int) 1e10f` saturates
//! to `2147483647`, `NaN` becomes 0, `decimal` / `varint` keep the low bits), `decimal` comes from
//! `Float.toString` / `Double.toString` text, `varint` from `longValue()`; casts to text use each
//! type's `toCQLLiteralNoQuote`. A cast whose source is not a parameter type of any `cast_as_<t>`
//! overload still resolves when some overload's parameter type is value-compatible with it
//! (`CAST(ts AS bigint)` goes through `cast_as_bigint(varint)`).
//!
//! ScyllaDB's conversions (from the image) are C++ ones: out-of-range or `NaN` floats convert to
//! the integer minimum, integers widen to `decimal` with scale 1, `varint` conversions are exact,
//! and unsupported casts report `<from class> cannot be cast to <to class>`.

use bytes::Bytes;
use num_bigint::BigInt;
use num_traits::ToPrimitive;

use crate::num::{Decimal, bigint_low_u64, bigint_to_f64};
use crate::{CqlType, Dialect, MarshalError, float, temporal, timeuuid};

const NUMERIC: &[CqlType] = &[
    CqlType::Tinyint,
    CqlType::Smallint,
    CqlType::Int,
    CqlType::Bigint,
    CqlType::Float,
    CqlType::Double,
    CqlType::Decimal,
    CqlType::Counter,
    CqlType::Varint,
];

fn is_numeric(t: &CqlType) -> bool {
    NUMERIC.contains(t)
}

/// The source types `cast_as_<to>` has an overload for (Cassandra's `CastFcts` table).
pub fn cassandra_cast_sources(to: &CqlType) -> Vec<CqlType> {
    use CqlType as T;
    let mut out = Vec::new();
    match to {
        T::Tinyint
        | T::Smallint
        | T::Int
        | T::Bigint
        | T::Float
        | T::Double
        | T::Decimal
        | T::Varint => {
            out.extend(NUMERIC.iter().filter(|t| *t != to).cloned());
        }
        T::Ascii | T::Text => {
            out.extend(NUMERIC.iter().cloned());
            out.extend([
                T::Inet,
                T::Boolean,
                T::Timeuuid,
                T::Timestamp,
                T::Date,
                T::Time,
                T::Uuid,
            ]);
            if matches!(to, T::Text) {
                out.push(T::Ascii);
            }
        }
        T::Date => out.extend([T::Timeuuid, T::Timestamp]),
        T::Timestamp => out.extend([T::Timeuuid, T::Date]),
        _ => {}
    }
    out
}

/// How a `CAST` resolves.
#[derive(Clone, Debug, PartialEq)]
pub enum CastPlan {
    /// The value is returned unchanged (casting to its own type).
    Identity,
    /// Convert, reading the argument as this type (`from` itself, or a value-compatible
    /// overload's parameter type).
    Convert { param: CqlType },
}

/// Resolves `CAST(<column of type from> AS <to>)`. `to_written` is the type name as written
/// (`varchar` resolves to no Cassandra function). The error is the reference's message.
pub fn plan(
    column: &str,
    from: &CqlType,
    to: &CqlType,
    to_written: &str,
    dialect: Dialect,
) -> Result<CastPlan, MarshalError> {
    if from == to {
        return Ok(CastPlan::Identity);
    }
    match dialect {
        Dialect::Cassandra => {
            let sources = if to_written.eq_ignore_ascii_case("varchar") {
                Vec::new()
            } else {
                cassandra_cast_sources(to)
            };
            if sources.is_empty() {
                return Err(MarshalError::new(format!(
                    "{column} cannot be cast to {}",
                    to_written.to_lowercase()
                )));
            }
            if sources.contains(from) {
                return Ok(CastPlan::Convert {
                    param: from.clone(),
                });
            }
            let weak: Vec<&CqlType> = sources
                .iter()
                .filter(|p| p.is_value_compatible_with(from))
                .collect();
            match weak.as_slice() {
                [one] => Ok(CastPlan::Convert {
                    param: (*one).clone(),
                }),
                _ => {
                    let name = format!("system.cast_as_{}", crate::names::cql_name(to));
                    let sigs: Vec<String> = sources
                        .iter()
                        .map(|p| {
                            format!(
                                "{name} : ({}) -> {}",
                                crate::names::cql_name(p),
                                crate::names::cql_name(to)
                            )
                        })
                        .collect();
                    Err(MarshalError::new(format!(
                        "Invalid call to function {name}, none of its type signatures match (known type signatures: {})",
                        sigs.join(", ")
                    )))
                }
            }
        }
        Dialect::Scylla => {
            use CqlType as T;
            let ok = match (from, to) {
                (f, t) if is_numeric(f) && is_numeric(t) && !matches!(t, T::Counter) => true,
                (f, T::Text | T::Ascii) => {
                    is_numeric(f)
                        || matches!(
                            f,
                            T::Ascii
                                | T::Text
                                | T::Boolean
                                | T::Inet
                                | T::Uuid
                                | T::Timeuuid
                                | T::Timestamp
                                | T::Date
                                | T::Time
                        )
                }
                (T::Timestamp | T::Timeuuid, T::Date) => true,
                (T::Date | T::Timeuuid, T::Timestamp) => true,
                _ => false,
            };
            if ok {
                Ok(CastPlan::Convert {
                    param: from.clone(),
                })
            } else {
                Err(MarshalError::new(format!(
                    "{} cannot be cast to {}",
                    crate::marshal::marshal_name(from, false, dialect),
                    crate::marshal::marshal_name(to, false, dialect)
                )))
            }
        }
    }
}

/// A numeric argument, decoded.
enum Num {
    Long(i64),
    Float(f32),
    Double(f64),
    Decimal(Decimal),
    Varint(BigInt),
}

fn read_num(t: &CqlType, b: &[u8]) -> Option<Num> {
    Some(match t {
        CqlType::Tinyint => Num::Long(*b.first()? as i8 as i64),
        CqlType::Smallint => Num::Long(i16::from_be_bytes(b.try_into().ok()?) as i64),
        CqlType::Int => Num::Long(i32::from_be_bytes(b.try_into().ok()?) as i64),
        CqlType::Bigint | CqlType::Counter | CqlType::Timestamp => {
            Num::Long(i64::from_be_bytes(b.try_into().ok()?))
        }
        CqlType::Float => Num::Float(f32::from_be_bytes(b.try_into().ok()?)),
        CqlType::Double => Num::Double(f64::from_be_bytes(b.try_into().ok()?)),
        CqlType::Decimal => Num::Decimal(Decimal::from_bytes(b)?),
        CqlType::Varint => Num::Varint(BigInt::from_signed_bytes_be(b)),
        _ => return None,
    })
}

/// Java's `(long) d`: saturating, NaN to 0.
fn java_long(d: f64) -> i64 {
    d as i64
}

/// Java's `(int) d`.
fn java_int(d: f64) -> i32 {
    d as i32
}

/// x86 `cvttsd2si`: the integer minimum for NaN and out-of-range values.
fn x86_i32(d: f64) -> i32 {
    if d.is_nan() || !(-2_147_483_648.0..2_147_483_648.0).contains(&d) {
        i32::MIN
    } else {
        d as i32
    }
}

fn x86_i64(d: f64) -> i64 {
    if d.is_nan() || !(-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&d) {
        i64::MIN
    } else {
        d as i64
    }
}

fn low_i64(v: &BigInt) -> i64 {
    bigint_low_u64(v) as i64
}

/// The integer value of a number, as `to` (tinyint … bigint) receives it.
fn to_integer(n: &Num, to: &CqlType, dialect: Dialect) -> i64 {
    let narrow = |v: i64| match to {
        CqlType::Tinyint => v as i8 as i64,
        CqlType::Smallint => v as i16 as i64,
        CqlType::Int => v as i32 as i64,
        _ => v,
    };
    let wide = matches!(to, CqlType::Bigint | CqlType::Counter);
    match n {
        Num::Long(v) => narrow(*v),
        Num::Float(f) => float_integer(*f as f64, wide, dialect, narrow),
        Num::Double(d) => float_integer(*d, wide, dialect, narrow),
        Num::Decimal(d) => narrow(d.to_i64_wrapping()),
        Num::Varint(v) => narrow(low_i64(v)),
    }
}

fn float_integer(d: f64, wide: bool, dialect: Dialect, narrow: impl Fn(i64) -> i64) -> i64 {
    match (dialect, wide) {
        (Dialect::Cassandra, true) => java_long(d),
        (Dialect::Cassandra, false) => narrow(java_int(d) as i64),
        (Dialect::Scylla, true) => x86_i64(d),
        (Dialect::Scylla, false) => narrow(x86_i32(d) as i64),
    }
}

fn to_f64(n: &Num) -> f64 {
    match n {
        Num::Long(v) => *v as f64,
        Num::Float(f) => *f as f64,
        Num::Double(d) => *d,
        Num::Decimal(d) => d.to_f64(),
        Num::Varint(v) => bigint_to_f64(v),
    }
}

fn to_f32(n: &Num) -> f32 {
    match n {
        Num::Long(v) => *v as f32,
        Num::Float(f) => *f,
        Num::Double(d) => *d as f32,
        Num::Decimal(d) => d.to_f32(),
        Num::Varint(v) => v.to_f32().unwrap_or(f32::NAN),
    }
}

fn java_number_format(c: char) -> MarshalError {
    MarshalError::new(format!(
        "java.lang.NumberFormatException: Character {c} is neither a decimal digit number, decimal point, nor \"e\" notation exponential mark."
    ))
}

fn to_decimal(n: &Num, dialect: Dialect) -> Result<Decimal, MarshalError> {
    let float_text = |text: String, finite: bool| -> Result<Decimal, MarshalError> {
        if !finite {
            return Err(match dialect {
                Dialect::Cassandra => java_number_format(
                    text.chars()
                        .find(|c| c.is_ascii_alphabetic())
                        .unwrap_or('N'),
                ),
                Dialect::Scylla => {
                    MarshalError::scylla(format!("big_decimal - incorrect integer: {text}"))
                }
            });
        }
        Decimal::parse(&text)
            .ok_or_else(|| MarshalError::new(format!("unable to make BigDecimal from '{text}'")))
    };
    Ok(match (n, dialect) {
        (Num::Float(f), Dialect::Cassandra) => float_text(float::java_float(*f), f.is_finite())?,
        (Num::Double(d), Dialect::Cassandra) => float_text(float::java_double(*d), d.is_finite())?,
        (Num::Float(f), Dialect::Scylla) => float_text(float::scylla_float(*f), f.is_finite())?,
        (Num::Double(d), Dialect::Scylla) => float_text(float::scylla_double(*d), d.is_finite())?,
        (Num::Long(v), Dialect::Cassandra) => Decimal::from_i64(*v),
        (Num::Varint(v), Dialect::Cassandra) => Decimal::new(v.clone(), 0),
        // ScyllaDB widens integers with one (zero) fraction digit.
        (Num::Long(v), Dialect::Scylla) => Decimal::new(BigInt::from(*v) * 10, 1),
        (Num::Varint(v), Dialect::Scylla) => Decimal::new(v * 10, 1),
        (Num::Decimal(d), _) => d.clone(),
    })
}

fn to_varint(n: &Num, dialect: Dialect) -> Result<BigInt, MarshalError> {
    Ok(match dialect {
        // `BigInteger.valueOf(p.longValue())`.
        Dialect::Cassandra => BigInt::from(match n {
            Num::Long(v) => *v,
            Num::Float(f) => java_long(*f as f64),
            Num::Double(d) => java_long(*d),
            Num::Decimal(d) => d.to_i64_wrapping(),
            Num::Varint(v) => low_i64(v),
        }),
        Dialect::Scylla => match n {
            Num::Long(v) => BigInt::from(*v),
            Num::Float(_) | Num::Double(_) => {
                let d = to_f64(n);
                if !d.is_finite() {
                    return Err(MarshalError::new(
                        "Cannot convert a non-finite number to an integer.",
                    ));
                }
                exact_float_bigint(d)
            }
            Num::Decimal(d) => d
                .to_bigint_trunc()
                .ok_or_else(|| MarshalError::scylla("big_decimal - value too large"))?,
            Num::Varint(v) => v.clone(),
        },
    })
}

/// The exact integer value of a finite, integral-or-truncated double.
fn exact_float_bigint(d: f64) -> BigInt {
    let t = d.trunc();
    let bits = t.to_bits();
    let neg = bits >> 63 == 1;
    let exp = ((bits >> 52) & 0x7ff) as i64 - 1075;
    let mant = (bits & ((1 << 52) - 1))
        | if (bits >> 52) & 0x7ff == 0 {
            0
        } else {
            1 << 52
        };
    let mut v = BigInt::from(mant);
    if exp >= 0 {
        v <<= exp as usize;
    } else {
        v >>= (-exp) as usize;
    }
    if neg { -v } else { v }
}

/// Converts the serialised value `b` of type `from` to `to` (after [`plan`]). `None` is null
/// (an empty argument of a type where empty means null).
pub fn cast(
    b: &[u8],
    from: &CqlType,
    to: &CqlType,
    dialect: Dialect,
) -> Result<Option<Bytes>, MarshalError> {
    use CqlType as T;
    if from == to {
        return Ok(Some(Bytes::copy_from_slice(b)));
    }
    if b.is_empty() {
        return Ok(if from.is_empty_value_meaningless() || is_numeric(from) {
            None
        } else {
            Some(Bytes::new())
        });
    }
    let bad = || {
        MarshalError::new(format!(
            "Invalid value for {}",
            crate::names::cql_name(from)
        ))
    };
    match to {
        T::Text | T::Ascii => {
            let s = crate::format::to_text(from, b, dialect)?;
            Ok(s.map(Bytes::from))
        }
        T::Tinyint | T::Smallint | T::Int | T::Bigint | T::Counter => {
            let n = read_num(from, b).ok_or_else(bad)?;
            let v = to_integer(&n, to, dialect);
            Ok(Some(Bytes::from(match to {
                T::Tinyint => vec![v as i8 as u8],
                T::Smallint => (v as i16).to_be_bytes().to_vec(),
                T::Int => (v as i32).to_be_bytes().to_vec(),
                _ => v.to_be_bytes().to_vec(),
            })))
        }
        T::Float => Ok(Some(Bytes::copy_from_slice(
            &to_f32(&read_num(from, b).ok_or_else(bad)?).to_be_bytes(),
        ))),
        T::Double => Ok(Some(Bytes::copy_from_slice(
            &to_f64(&read_num(from, b).ok_or_else(bad)?).to_be_bytes(),
        ))),
        T::Decimal => Ok(Some(Bytes::from(
            to_decimal(&read_num(from, b).ok_or_else(bad)?, dialect)?.to_bytes(),
        ))),
        T::Varint => Ok(Some(Bytes::from(
            to_varint(&read_num(from, b).ok_or_else(bad)?, dialect)?.to_signed_bytes_be(),
        ))),
        T::Date => {
            let ms = to_millis(from, b).ok_or_else(bad)?;
            Ok(Some(Bytes::copy_from_slice(
                &temporal::millis_to_date(ms, dialect).to_be_bytes(),
            )))
        }
        T::Timestamp => Ok(Some(Bytes::copy_from_slice(
            &to_millis(from, b).ok_or_else(bad)?.to_be_bytes(),
        ))),
        _ => Err(MarshalError::new(format!(
            "{} cannot be cast to {}",
            crate::names::cql_name(from),
            crate::names::cql_name(to)
        ))),
    }
}

/// Unix milliseconds of a `timestamp`, `date` or `timeuuid` (`TemporalType.toTimeInMillis`).
pub fn to_millis(ty: &CqlType, b: &[u8]) -> Option<i64> {
    match ty {
        CqlType::Timestamp => Some(i64::from_be_bytes(b.try_into().ok()?)),
        CqlType::Date => Some(temporal::date_to_millis(u32::from_be_bytes(
            b.try_into().ok()?,
        ))),
        CqlType::Timeuuid => Some(timeuuid::unix_timestamp_of(&b.try_into().ok()?)),
        _ => None,
    }
}

/// `toDate(x)` for a timestamp, date or timeuuid argument.
pub fn to_date(ty: &CqlType, b: &[u8], dialect: Dialect) -> Option<u32> {
    to_millis(ty, b).map(|ms| temporal::millis_to_date(ms, dialect))
}

/// `toUnixTimestamp(x)`.
pub fn to_unix_timestamp(ty: &CqlType, b: &[u8]) -> Option<i64> {
    to_millis(ty, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Value, encode};

    const C: Dialect = Dialect::Cassandra;
    const S: Dialect = Dialect::Scylla;

    fn run(from: &CqlType, v: Value, to: &CqlType, d: Dialect) -> Result<Option<Value>, String> {
        let b = encode(&v, from);
        cast(&b, from, to, d)
            .map(|o| o.map(|b| crate::decode(to, &b).unwrap()))
            .map_err(|e| e.message)
    }

    #[test]
    fn numeric_casts_match_both_references() {
        use CqlType as T;
        // CAST(...) on both images; rows k=1..3 of the probe table.
        let cases: Vec<(CqlType, Value, CqlType, Value, Value)> = vec![
            (
                T::Int,
                Value::Int(300),
                T::Tinyint,
                Value::Tinyint(44),
                Value::Tinyint(44),
            ),
            (
                T::Int,
                Value::Int(i32::MIN),
                T::Tinyint,
                Value::Tinyint(0),
                Value::Tinyint(0),
            ),
            (
                T::Int,
                Value::Int(300),
                T::Float,
                Value::Float(300.0),
                Value::Float(300.0),
            ),
            (
                T::Float,
                Value::Float(1e10),
                T::Int,
                Value::Int(i32::MAX),
                Value::Int(i32::MIN),
            ),
            (
                T::Float,
                Value::Float(f32::NAN),
                T::Int,
                Value::Int(0),
                Value::Int(i32::MIN),
            ),
            (
                T::Float,
                Value::Float(f32::NAN),
                T::Bigint,
                Value::Bigint(0),
                Value::Bigint(i64::MIN),
            ),
            (
                T::Float,
                Value::Float(-3.7),
                T::Int,
                Value::Int(-3),
                Value::Int(-3),
            ),
            (
                T::Float,
                Value::Float(1e10),
                T::Bigint,
                Value::Bigint(10_000_000_000),
                Value::Bigint(10_000_000_000),
            ),
            (
                T::Double,
                Value::Double(-1e19),
                T::Bigint,
                Value::Bigint(i64::MIN),
                Value::Bigint(i64::MIN),
            ),
            (
                T::Double,
                Value::Double(-1e19),
                T::Int,
                Value::Int(i32::MIN),
                Value::Int(i32::MIN),
            ),
            (
                T::Double,
                Value::Double(f64::NEG_INFINITY),
                T::Int,
                Value::Int(i32::MIN),
                Value::Int(i32::MIN),
            ),
            (
                T::Double,
                Value::Double(f64::NEG_INFINITY),
                T::Smallint,
                Value::Smallint(0),
                Value::Smallint(0),
            ),
            (
                T::Double,
                Value::Double(1.5),
                T::Int,
                Value::Int(1),
                Value::Int(1),
            ),
            (
                T::Decimal,
                Value::Decimal(Decimal::parse("123.987").unwrap()),
                T::Int,
                Value::Int(123),
                Value::Int(123),
            ),
            (
                T::Decimal,
                Value::Decimal(Decimal::parse("-0.5").unwrap()),
                T::Bigint,
                Value::Bigint(0),
                Value::Bigint(0),
            ),
            (
                T::Varint,
                Value::Varint("100000000000000000000".parse().unwrap()),
                T::Int,
                Value::Int(1_661_992_960),
                Value::Int(1_661_992_960),
            ),
            (
                T::Varint,
                Value::Varint("100000000000000000000".parse().unwrap()),
                T::Bigint,
                Value::Bigint(7_766_279_631_452_241_920),
                Value::Bigint(7_766_279_631_452_241_920),
            ),
            (
                T::Varint,
                Value::Varint("100000000000000000000".parse().unwrap()),
                T::Double,
                Value::Double(1e20),
                Value::Double(1e20),
            ),
            (
                T::Varint,
                Value::Varint("100000000000000000000".parse().unwrap()),
                T::Float,
                Value::Float(1e20),
                Value::Float(1e20),
            ),
            (
                T::Tinyint,
                Value::Tinyint(-1),
                T::Varint,
                Value::Varint(BigInt::from(-1)),
                Value::Varint(BigInt::from(-1)),
            ),
            (
                T::Bigint,
                Value::Bigint(9_007_199_254_740_993),
                T::Double,
                Value::Double(9_007_199_254_740_992.0),
                Value::Double(9_007_199_254_740_992.0),
            ),
            (
                T::Bigint,
                Value::Bigint(i64::MIN),
                T::Float,
                Value::Float(-9.223372e18),
                Value::Float(-9.223372e18),
            ),
            (
                T::Decimal,
                Value::Decimal(Decimal::parse("123.987").unwrap()),
                T::Float,
                Value::Float(123.987),
                Value::Float(123.987),
            ),
            (
                T::Decimal,
                Value::Decimal(Decimal::parse("123.987").unwrap()),
                T::Double,
                Value::Double(123.987),
                Value::Double(123.987),
            ),
            (
                T::Decimal,
                Value::Decimal(Decimal::parse("123.987").unwrap()),
                T::Varint,
                Value::Varint(BigInt::from(123)),
                Value::Varint(BigInt::from(123)),
            ),
            (
                T::Double,
                Value::Double(1.5),
                T::Varint,
                Value::Varint(BigInt::from(1)),
                Value::Varint(BigInt::from(1)),
            ),
            (
                T::Float,
                Value::Float(-3.7),
                T::Varint,
                Value::Varint(BigInt::from(-3)),
                Value::Varint(BigInt::from(-3)),
            ),
            (
                T::Float,
                Value::Float(1e10),
                T::Varint,
                Value::Varint(BigInt::from(10_000_000_000i64)),
                Value::Varint(BigInt::from(10_000_000_000i64)),
            ),
            (
                T::Double,
                Value::Double(-1e19),
                T::Varint,
                Value::Varint(BigInt::from(i64::MIN)),
                Value::Varint("-10000000000000000000".parse().unwrap()),
            ),
            (
                T::Float,
                Value::Float(f32::NAN),
                T::Double,
                Value::Double(f64::NAN),
                Value::Double(f64::NAN),
            ),
            (
                T::Double,
                Value::Double(f64::NEG_INFINITY),
                T::Float,
                Value::Float(f32::NEG_INFINITY),
                Value::Float(f32::NEG_INFINITY),
            ),
            (
                T::Float,
                Value::Float(f32::NAN),
                T::Varint,
                Value::Varint(BigInt::from(0)),
                Value::Null,
            ),
            // Narrowing from floats (both images).
            (
                T::Float,
                Value::Float(1e10),
                T::Smallint,
                Value::Smallint(-1),
                Value::Smallint(0),
            ),
            (
                T::Float,
                Value::Float(1e10),
                T::Tinyint,
                Value::Tinyint(-1),
                Value::Tinyint(0),
            ),
            (
                T::Double,
                Value::Double(70000.7),
                T::Smallint,
                Value::Smallint(4464),
                Value::Smallint(4464),
            ),
            (
                T::Smallint,
                Value::Smallint(300),
                T::Tinyint,
                Value::Tinyint(44),
                Value::Tinyint(44),
            ),
            (
                T::Tinyint,
                Value::Tinyint(-5),
                T::Double,
                Value::Double(-5.0),
                Value::Double(-5.0),
            ),
        ];
        for (from, v, to, c, s) in cases {
            for (d, want) in [(C, c.clone()), (S, s.clone())] {
                let got = run(&from, v.clone(), &to, d);
                match (&want, &got) {
                    (Value::Null, Err(_)) => {}
                    (Value::Double(w), Ok(Some(Value::Double(g)))) if w.is_nan() => {
                        assert!(g.is_nan())
                    }
                    _ => assert_eq!(got, Ok(Some(want.clone())), "{from:?} -> {to:?} ({d})"),
                }
            }
        }
        assert_eq!(
            run(&T::Float, Value::Float(f32::NAN), &T::Varint, S).unwrap_err(),
            "Cannot convert a non-finite number to an integer."
        );
    }

    #[test]
    fn decimal_casts() {
        use CqlType as T;
        let dec = |from: &CqlType, v: Value, d: Dialect| -> Result<Vec<u8>, String> {
            cast(&encode(&v, from), from, &T::Decimal, d)
                .map(|b| b.unwrap().to_vec())
                .map_err(|e| e.message)
        };
        let h = |s: &str| crate::wire::unhex(s).unwrap();
        // decimalAsBlob(CAST(x AS decimal)) on both images.
        assert_eq!(dec(&T::Int, Value::Int(300), C).unwrap(), h("00000000012c"));
        assert_eq!(dec(&T::Int, Value::Int(300), S).unwrap(), h("000000010bb8"));
        assert_eq!(
            dec(&T::Int, Value::Int(i32::MIN), C).unwrap(),
            h("0000000080000000")
        );
        assert_eq!(
            dec(&T::Int, Value::Int(i32::MIN), S).unwrap(),
            h("00000001fb00000000")
        );
        assert_eq!(
            dec(&T::Double, Value::Double(-1e19), C).unwrap(),
            h("ffffffeef6")
        );
        assert_eq!(
            dec(&T::Double, Value::Double(-1e19), S).unwrap(),
            h("ffffffedff")
        );
        assert_eq!(
            dec(&T::Float, Value::Float(1e10), C).unwrap(),
            h("fffffff70a")
        );
        assert_eq!(
            dec(&T::Float, Value::Float(1e10), S).unwrap(),
            h("fffffff601")
        );
        assert_eq!(
            dec(&T::Float, Value::Float(-3.7), C).unwrap(),
            h("00000001db")
        );
        assert_eq!(
            dec(&T::Double, Value::Double(1.5), S).unwrap(),
            h("000000010f")
        );
        assert_eq!(
            dec(&T::Bigint, Value::Bigint(9_007_199_254_740_993), C).unwrap(),
            h("0000000020000000000001")
        );
        assert_eq!(
            dec(&T::Bigint, Value::Bigint(9_007_199_254_740_993), S).unwrap(),
            h("00000001014000000000000a")
        );
        assert_eq!(
            dec(&T::Varint, Value::Varint(BigInt::from(-300)), C).unwrap(),
            h("00000000fed4")
        );
        assert_eq!(
            dec(&T::Varint, Value::Varint(BigInt::from(-300)), S).unwrap(),
            h("00000001f448")
        );
        assert_eq!(
            dec(&T::Tinyint, Value::Tinyint(-1), S).unwrap(),
            h("00000001f6")
        );
        assert_eq!(
            dec(&T::Float, Value::Float(f32::NAN), C).unwrap_err(),
            "java.lang.NumberFormatException: Character N is neither a decimal digit number, decimal point, nor \"e\" notation exponential mark."
        );
        assert_eq!(
            dec(&T::Double, Value::Double(f64::NEG_INFINITY), C).unwrap_err(),
            "java.lang.NumberFormatException: Character I is neither a decimal digit number, decimal point, nor \"e\" notation exponential mark."
        );
        assert_eq!(
            dec(&T::Float, Value::Float(f32::NAN), S).unwrap_err(),
            "marshaling error: big_decimal - incorrect integer: nan"
        );
        assert_eq!(
            dec(&T::Double, Value::Double(f64::NEG_INFINITY), S).unwrap_err(),
            "marshaling error: big_decimal - incorrect integer: -inf"
        );
    }

    #[test]
    fn text_and_temporal_casts() {
        use CqlType as T;
        let txt = |from: &CqlType, v: Value, d: Dialect| -> Option<String> {
            cast(&encode(&v, from), from, &T::Text, d)
                .unwrap()
                .map(|b| String::from_utf8(b.to_vec()).unwrap())
        };
        assert_eq!(txt(&T::Int, Value::Int(300), C).as_deref(), Some("300"));
        assert_eq!(
            cast(&encode(&Value::Int(300), &T::Int), &T::Int, &T::Float, C)
                .unwrap()
                .map(|b| crate::format::to_text(&T::Float, &b, C).unwrap().unwrap())
                .as_deref(),
            Some("300.0")
        );
        assert_eq!(
            txt(&T::Timestamp, Value::Timestamp(-1), C).as_deref(),
            Some("1969-12-31T23:59:59.999Z")
        );
        assert_eq!(
            txt(&T::Inet, Value::Inet("::1".parse().unwrap()), C).as_deref(),
            Some("0:0:0:0:0:0:0:1")
        );
        assert_eq!(
            txt(&T::Inet, Value::Inet("::1".parse().unwrap()), S).as_deref(),
            Some("::1")
        );
        assert_eq!(
            txt(&T::Boolean, Value::Boolean(false), S).as_deref(),
            Some("false")
        );
        assert_eq!(
            txt(&T::Time, Value::Time(3_723_500_000_000), C).as_deref(),
            Some("01:02:03.500000000")
        );
        assert_eq!(cast(&[], &T::Int, &T::Text, C).unwrap(), None);
        assert_eq!(cast(&[], &T::Decimal, &T::Text, S).unwrap(), None);
        assert_eq!(
            cast(b"x", &T::Ascii, &T::Text, C).unwrap().unwrap(),
            Bytes::from_static(b"x")
        );

        // CAST(ts AS date): Cassandra truncates toward zero, ScyllaDB floors.
        let date = |d: Dialect| {
            u32::from_be_bytes(
                cast(&(-1i64).to_be_bytes(), &T::Timestamp, &T::Date, d)
                    .unwrap()
                    .unwrap()[..]
                    .try_into()
                    .unwrap(),
            )
        };
        assert_eq!(temporal::format_date(date(C), C), "1970-01-01");
        assert_eq!(temporal::format_date(date(S), S), "1969-12-31");
        let tu = timeuuid::parse_uuid("50554d6e-29bb-11e5-b345-feff819cdc9f").unwrap();
        for d in [C, S] {
            let t = cast(&tu, &T::Timeuuid, &T::Timestamp, d).unwrap().unwrap();
            assert_eq!(&t[..], &1_436_832_018_805i64.to_be_bytes());
            let dt = cast(&tu, &T::Timeuuid, &T::Date, d).unwrap().unwrap();
            assert_eq!(
                temporal::format_date(u32::from_be_bytes(dt[..].try_into().unwrap()), d),
                "2015-07-14"
            );
            let dt = ((1u32 << 31) - 1).to_be_bytes();
            assert_eq!(
                &cast(&dt, &T::Date, &T::Timestamp, d).unwrap().unwrap()[..],
                &(-86_400_000i64).to_be_bytes()
            );
        }
        assert_eq!(
            to_unix_timestamp(&T::Timeuuid, &tu),
            Some(1_436_832_018_805)
        );
        assert_eq!(
            to_unix_timestamp(&T::Date, &((1u32 << 31) - 1).to_be_bytes()),
            Some(-86_400_000)
        );
        assert_eq!(
            to_date(&T::Timestamp, &(-1i64).to_be_bytes(), C),
            Some(1 << 31)
        );
        assert_eq!(
            to_date(&T::Timestamp, &(-1i64).to_be_bytes(), S),
            Some((1 << 31) - 1)
        );
        assert_eq!(to_millis(&T::Int, &[0; 4]), None);
        assert!(cast(&[0; 4], &T::Int, &T::Uuid, C).is_err());
        assert!(cast(&[0; 3], &T::Int, &T::Bigint, C).is_err());
    }

    #[test]
    fn planning() {
        use CqlType as T;
        assert_eq!(
            plan("i", &T::Int, &T::Int, "int", C),
            Ok(CastPlan::Identity)
        );
        assert_eq!(
            plan("i", &T::Int, &T::Bigint, "bigint", C),
            Ok(CastPlan::Convert { param: T::Int })
        );
        // CAST(ts AS bigint) resolves through cast_as_bigint(varint) on Cassandra.
        assert_eq!(
            plan("ts", &T::Timestamp, &T::Bigint, "bigint", C),
            Ok(CastPlan::Convert { param: T::Varint })
        );
        let b = cast(&(-1i64).to_be_bytes(), &T::Varint, &T::Bigint, C)
            .unwrap()
            .unwrap();
        assert_eq!(&b[..], &(-1i64).to_be_bytes());
        for (col, from, to, written, msg) in [
            ("i", T::Int, T::Blob, "blob", "i cannot be cast to blob"),
            (
                "tu",
                T::Timeuuid,
                T::Uuid,
                "uuid",
                "tu cannot be cast to uuid",
            ),
            (
                "ip",
                T::Inet,
                T::Text,
                "varchar",
                "ip cannot be cast to varchar",
            ),
        ] {
            assert_eq!(plan(col, &from, &to, written, C).unwrap_err().message, msg);
        }
        let e = plan("u", &T::Uuid, &T::Int, "int", C).unwrap_err().message;
        assert!(e.starts_with("Invalid call to function system.cast_as_int, none of its type signatures match (known type signatures: "), "{e}");
        assert!(e.contains("system.cast_as_int : (varint) -> int"));
        for (from, to, msg) in [
            (
                T::Uuid,
                T::Int,
                "org.apache.cassandra.db.marshal.UUIDType cannot be cast to org.apache.cassandra.db.marshal.Int32Type",
            ),
            (
                T::Date,
                T::Int,
                "org.apache.cassandra.db.marshal.SimpleDateType cannot be cast to org.apache.cassandra.db.marshal.Int32Type",
            ),
            (
                T::Int,
                T::Blob,
                "org.apache.cassandra.db.marshal.Int32Type cannot be cast to org.apache.cassandra.db.marshal.BytesType",
            ),
            (
                T::Timestamp,
                T::Bigint,
                "org.apache.cassandra.db.marshal.TimestampType cannot be cast to org.apache.cassandra.db.marshal.LongType",
            ),
            (
                T::Timeuuid,
                T::Uuid,
                "org.apache.cassandra.db.marshal.TimeUUIDType cannot be cast to org.apache.cassandra.db.marshal.UUIDType",
            ),
            (
                T::Duration,
                T::Text,
                "org.apache.cassandra.db.marshal.DurationType cannot be cast to org.apache.cassandra.db.marshal.UTF8Type",
            ),
        ] {
            assert_eq!(plan("c", &from, &to, "x", S).unwrap_err().message, msg);
        }
        assert_eq!(
            plan("ip", &T::Inet, &T::Text, "varchar", S),
            Ok(CastPlan::Convert { param: T::Inet })
        );
        assert_eq!(
            plan("d", &T::Timestamp, &T::Date, "date", S),
            Ok(CastPlan::Convert {
                param: T::Timestamp
            })
        );
        assert!(cassandra_cast_sources(&T::Text).contains(&T::Ascii));
        assert!(!cassandra_cast_sources(&T::Ascii).contains(&T::Text));
        assert!(cassandra_cast_sources(&T::Uuid).is_empty());
    }
}
