//! `SET c = c + v` / `c = c - v` on non-counter columns, which only a conditional statement can
//! do (the old value is read in the same transaction).
//!
//! Cassandra (`Operation.Addition` with `canReadExistingState`, `Constants.Adder`) allows `+` on
//! numbers (`NumberType.add`, integers wrap like Java) and strings (concatenation); a null old
//! value leaves the column alone. ScyllaDB allows `+` and `-` on numbers only, fails on integer
//! overflow and propagates nulls (`null + 1` writes null).

use bytes::Bytes;
use nosql_types::{CqlType, Decimal, Dialect};
use num_bigint::BigInt;

use crate::error::{Error, Result};
use crate::functions::arith::{dec_add, dec_sub};
use crate::messages as msg;
use crate::prepare::terms::{Bound, EvalCtx, Term};

fn is_number(ty: &CqlType) -> bool {
    matches!(
        ty,
        CqlType::Tinyint
            | CqlType::Smallint
            | CqlType::Int
            | CqlType::Bigint
            | CqlType::Varint
            | CqlType::Decimal
            | CqlType::Float
            | CqlType::Double
    )
}

/// Whether `column = column ± value` is allowed on a non-counter scalar column. ScyllaDB checks
/// after typing the value, Cassandra before ([`checks_first`]).
pub fn check(
    d: Dialect,
    ty: &CqlType,
    column: &str,
    shown: &str,
    sub: bool,
    conditional: bool,
) -> Result<()> {
    let non_counter = || {
        Error::invalid(format!(
            "Invalid operation ({shown}) for non counter column {column}"
        ))
    };
    match d {
        Dialect::Scylla if !is_number(ty) => Err(non_counter()),
        Dialect::Scylla if !conditional => Err(msg::non_counter_increment(d, column, shown)),
        Dialect::Scylla => Ok(()),
        Dialect::Cassandra if sub || !conditional => Err(non_counter()),
        Dialect::Cassandra if is_number(ty) || matches!(ty, CqlType::Text | CqlType::Ascii) => {
            Ok(())
        }
        Dialect::Cassandra => Err(Error::invalid(format!(
            "Invalid operation ({shown}) for non-numeric and non-text type {column}"
        ))),
    }
}

/// Cassandra validates the operation before typing its value, ScyllaDB after.
pub fn checks_first(d: Dialect) -> bool {
    !d.is_scylla()
}

/// What to write for the column.
pub enum Write {
    Nothing,
    Null,
    Value(Bytes),
}

/// Applies the operation to the old value.
pub fn apply(
    ty: &CqlType,
    current: Option<&[u8]>,
    value: &Term,
    sub: bool,
    cx: &EvalCtx,
) -> Result<Write> {
    let d = cx.dialect;
    let delta = value.eval(cx)?;
    match d {
        Dialect::Scylla => {
            let delta = match delta {
                Bound::Unset => {
                    let index = match value {
                        Term::Bind { index, .. } => *index,
                        _ => 0,
                    };
                    return Err(msg::lwt_unset_value(d, index));
                }
                Bound::Null => return Ok(Write::Null),
                Bound::Val(b) => b,
            };
            let (Some(cur), false) = (current, delta.is_empty()) else {
                return Ok(Write::Null);
            };
            if cur.is_empty() {
                return Ok(Write::Null);
            }
            scylla_numeric(ty, cur, &delta, sub).map(Write::Value)
        }
        Dialect::Cassandra => {
            let Some(cur) = current else {
                return Ok(Write::Nothing);
            };
            if matches!(ty, CqlType::Text | CqlType::Ascii) {
                let append = match delta {
                    Bound::Val(b) => b,
                    Bound::Unset => Bytes::new(),
                    Bound::Null => return Err(npe("java.nio.ByteBuffer.remaining()", "append")),
                };
                let mut v = cur.to_vec();
                v.extend_from_slice(&append);
                return Ok(Write::Value(Bytes::from(v)));
            }
            let delta = match delta {
                Bound::Val(b) if !b.is_empty() => b,
                // `type.compose` of an empty or missing value is null.
                _ => return Err(npe(java_accessor(ty), "right")),
            };
            cassandra_numeric(ty, cur, &delta).map(Write::Value)
        }
    }
}

fn java_accessor(ty: &CqlType) -> &'static str {
    match ty {
        CqlType::Tinyint => "java.lang.Number.byteValue()",
        CqlType::Smallint => "java.lang.Number.shortValue()",
        CqlType::Int => "java.lang.Number.intValue()",
        CqlType::Bigint => "java.lang.Number.longValue()",
        CqlType::Float => "java.lang.Number.floatValue()",
        CqlType::Double => "java.lang.Number.doubleValue()",
        CqlType::Varint => "java.math.BigInteger.add(java.math.BigInteger)",
        _ => "java.math.BigDecimal.add(java.math.BigDecimal, java.math.MathContext)",
    }
}

fn npe(method: &str, var: &str) -> Error {
    Error::server(format!(
        "java.lang.NullPointerException: Cannot invoke \"{method}\" because \"{var}\" is null"
    ))
}

fn int_of(b: &[u8]) -> Option<i64> {
    Some(match b.len() {
        1 => b[0] as i8 as i64,
        2 => i16::from_be_bytes([b[0], b[1]]) as i64,
        4 => i32::from_be_bytes(b.try_into().ok()?) as i64,
        8 => i64::from_be_bytes(b.try_into().ok()?),
        _ => return None,
    })
}

fn invalid_value() -> Error {
    Error::invalid("Invalid value for arithmetic operation")
}

/// Same-type arithmetic with ScyllaDB's overflow errors.
fn scylla_numeric(ty: &CqlType, a: &[u8], b: &[u8], sub: bool) -> Result<Bytes> {
    let overflow = || {
        Error::invalid(if sub {
            "Arithmetic SUB overflow"
        } else {
            "Arithmetic ADD overflow"
        })
    };
    match ty {
        CqlType::Tinyint | CqlType::Smallint | CqlType::Int | CqlType::Bigint => {
            let (x, y) = (
                int_of(a).ok_or_else(invalid_value)?,
                int_of(b).ok_or_else(invalid_value)?,
            );
            let v = if sub {
                x.checked_sub(y)
            } else {
                x.checked_add(y)
            }
            .ok_or_else(overflow)?;
            Ok(match ty {
                CqlType::Tinyint => {
                    Bytes::copy_from_slice(&i8::try_from(v).map_err(|_| overflow())?.to_be_bytes())
                }
                CqlType::Smallint => {
                    Bytes::copy_from_slice(&i16::try_from(v).map_err(|_| overflow())?.to_be_bytes())
                }
                CqlType::Int => {
                    Bytes::copy_from_slice(&i32::try_from(v).map_err(|_| overflow())?.to_be_bytes())
                }
                _ => Bytes::copy_from_slice(&v.to_be_bytes()),
            })
        }
        _ => float_or_big(ty, a, b, sub),
    }
}

/// `NumberType.add`: Java arithmetic (integers wrap).
fn cassandra_numeric(ty: &CqlType, a: &[u8], b: &[u8]) -> Result<Bytes> {
    match ty {
        CqlType::Tinyint | CqlType::Smallint | CqlType::Int | CqlType::Bigint => {
            let (x, y) = (
                int_of(a).ok_or_else(invalid_value)?,
                int_of(b).ok_or_else(invalid_value)?,
            );
            let v = x.wrapping_add(y);
            Ok(match ty {
                CqlType::Tinyint => Bytes::copy_from_slice(&(v as i8).to_be_bytes()),
                CqlType::Smallint => Bytes::copy_from_slice(&(v as i16).to_be_bytes()),
                CqlType::Int => Bytes::copy_from_slice(&(v as i32).to_be_bytes()),
                _ => Bytes::copy_from_slice(&v.to_be_bytes()),
            })
        }
        _ => float_or_big(ty, a, b, false),
    }
}

fn float_or_big(ty: &CqlType, a: &[u8], b: &[u8], sub: bool) -> Result<Bytes> {
    match ty {
        CqlType::Float => {
            let x = f32::from_be_bytes(a.try_into().map_err(|_| invalid_value())?);
            let y = f32::from_be_bytes(b.try_into().map_err(|_| invalid_value())?);
            Ok(Bytes::copy_from_slice(
                &(if sub { x - y } else { x + y }).to_be_bytes(),
            ))
        }
        CqlType::Double => {
            let x = f64::from_be_bytes(a.try_into().map_err(|_| invalid_value())?);
            let y = f64::from_be_bytes(b.try_into().map_err(|_| invalid_value())?);
            Ok(Bytes::copy_from_slice(
                &(if sub { x - y } else { x + y }).to_be_bytes(),
            ))
        }
        CqlType::Varint => {
            let (x, y) = (
                BigInt::from_signed_bytes_be(a),
                BigInt::from_signed_bytes_be(b),
            );
            Ok(Bytes::from(
                if sub { x - y } else { x + y }.to_signed_bytes_be(),
            ))
        }
        CqlType::Decimal => {
            let x = Decimal::from_bytes(a).ok_or_else(invalid_value)?;
            let y = Decimal::from_bytes(b).ok_or_else(invalid_value)?;
            let v = if sub {
                dec_sub(&x, &y)
            } else {
                dec_add(&x, &y)
            };
            Ok(Bytes::from(v.to_bytes()))
        }
        _ => Err(invalid_value()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(v: i32) -> Bytes {
        Bytes::copy_from_slice(&v.to_be_bytes())
    }

    #[test]
    fn dialect_rules() {
        let s = Dialect::Scylla;
        let c = Dialect::Cassandra;
        assert!(check(s, &CqlType::Int, "v", "v = v - 1", true, true).is_ok());
        assert!(check(c, &CqlType::Int, "v", "v = v - 1", true, true).is_err());
        assert!(check(c, &CqlType::Text, "t", "t = t + 'a'", false, true).is_ok());
        assert_eq!(
            check(s, &CqlType::Text, "t", "t = t + 'a'", false, true)
                .unwrap_err()
                .message,
            "Invalid operation (t = t + 'a') for non counter column t"
        );
        assert_eq!(
            check(c, &CqlType::Blob, "b", "b = b + 0x01", false, true)
                .unwrap_err()
                .message,
            "Invalid operation (b = b + 0x01) for non-numeric and non-text type b"
        );
        assert!(
            check(s, &CqlType::Int, "v", "v = v + 1", false, false)
                .unwrap_err()
                .message
                .starts_with("SET with a column expression")
        );
    }

    #[test]
    fn arithmetic() {
        let one = Term::Const(Some(int(1)));
        let max = int(i32::MAX);
        let params = [];
        let s = EvalCtx::new(&params, Dialect::Scylla);
        let c = EvalCtx::new(&params, Dialect::Cassandra);
        assert_eq!(
            apply(&CqlType::Int, Some(&max), &one, false, &s)
                .err()
                .unwrap()
                .message,
            "Arithmetic ADD overflow"
        );
        let Write::Value(v) = apply(&CqlType::Int, Some(&max), &one, false, &c).unwrap() else {
            panic!()
        };
        assert_eq!(v, int(i32::MIN));
        assert!(matches!(
            apply(&CqlType::Int, None, &one, false, &s).unwrap(),
            Write::Null
        ));
        assert!(matches!(
            apply(&CqlType::Int, None, &one, false, &c).unwrap(),
            Write::Nothing
        ));
        let Write::Value(v) = apply(&CqlType::Int, Some(&int(5)), &one, true, &s).unwrap() else {
            panic!()
        };
        assert_eq!(v, int(4));
        let txt = Term::Const(Some(Bytes::from_static(b"cd")));
        let Write::Value(v) = apply(&CqlType::Text, Some(b"ab"), &txt, false, &c).unwrap() else {
            panic!()
        };
        assert_eq!(&v[..], b"abcd");
    }
}
