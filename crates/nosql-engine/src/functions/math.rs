//! Cassandra 5's math functions (`MathFcts`, Apache-2.0): `abs`, `exp`, `log`, `log10`, `round`
//! for every numeric type, computed as each `NumberType` does it.
//!
//! - `tinyint` / `smallint` / `int` / `bigint` / `counter`: Java's `Math` on the value, the result
//!   cast back with Java's narrowing (`(byte) Math.exp(b)`: saturate to `int`, then truncate);
//!   `abs` wraps at the minimum; `round` is the identity.
//! - `float` / `double`: `Math.*` in double precision; `round` is `Math.round` (half up), cast
//!   back to the type.
//! - `decimal`: `BigDecimalMath` at the input's precision within [32, 10000] digits
//!   ([`super::bigdec`]); `round` is `setScale(0, HALF_UP)`.
//! - `varint`: as a decimal, truncated back to an integer.
//!
//! Failures are Cassandra's uncaught `ArithmeticException`s (a server error). ScyllaDB has none of
//! these functions ("Unknown function abs called").

use std::sync::Arc;

use bytes::Bytes;
use nosql_types::CqlType;
use num_bigint::BigInt;
use num_traits::{Signed, Zero};

use super::bigdec::{self, ArithError, Dec};
use super::util::arg;
use super::{FnCtx, FnName, Function, Registry};
use crate::error::{Error, Result};

/// `MathFcts.addFunctionsTo`'s types, in its order.
const TYPES: [CqlType; 9] = [
    CqlType::Tinyint,
    CqlType::Smallint,
    CqlType::Int,
    CqlType::Float,
    CqlType::Bigint,
    CqlType::Double,
    CqlType::Varint,
    CqlType::Decimal,
    CqlType::Counter,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Abs,
    Exp,
    Log,
    Log10,
    Round,
}

pub fn register(r: &mut Registry) {
    if r.dialect().is_scylla() {
        return;
    }
    for ty in TYPES {
        for (name, op) in [
            ("abs", Op::Abs),
            ("exp", Op::Exp),
            ("log", Op::Log),
            ("log10", Op::Log10),
            ("round", Op::Round),
        ] {
            r.add(Arc::new(MathFn {
                name: FnName::system(name),
                args: vec![ty.clone()],
                op,
            }));
        }
    }
}

#[derive(Debug)]
struct MathFn {
    name: FnName,
    args: Vec<CqlType>,
    op: Op,
}

impl Function for MathFn {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &self.args[0]
    }
    fn null_on_null_input(&self) -> bool {
        true
    }
    fn execute(&self, _: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        match arg(args, 0) {
            None => Ok(None),
            Some(b) if b.is_empty() => Ok(None),
            Some(b) => compute(self.op, &self.args[0], b).map(Some),
        }
    }
}

fn arith(e: ArithError) -> Error {
    Error::server(format!("java.lang.ArithmeticException: {}", e.0))
}

fn bad() -> Error {
    Error::server("java.lang.IllegalArgumentException: invalid numeric value")
}

/// Java's `(int) d` (saturating; NaN is 0).
fn d2i(d: f64) -> i32 {
    d as i32
}

/// `Math.round(double)`: the closest long, ties toward positive infinity.
fn round_half_up(d: f64) -> i64 {
    if d.is_nan() {
        return 0;
    }
    let f = d.floor();
    let r = if d - f >= 0.5 { f + 1.0 } else { f };
    r as i64
}

/// An integral op on a long value: `exp` / `log` / `log10` through `double`.
fn long_op(op: Op, v: i64) -> f64 {
    let x = v as f64;
    match op {
        Op::Exp => x.exp(),
        Op::Log => x.ln(),
        Op::Log10 => x.log10(),
        Op::Abs | Op::Round => x,
    }
}

fn compute(op: Op, ty: &CqlType, b: &[u8]) -> Result<Bytes> {
    let fixed = |n: usize| -> Result<&[u8]> { if b.len() == n { Ok(b) } else { Err(bad()) } };
    Ok(match ty {
        CqlType::Tinyint => {
            let v = fixed(1)?[0] as i8;
            let r = match op {
                Op::Abs => (v as i32).abs() as i8,
                Op::Round => v,
                _ => d2i(long_op(op, v as i64)) as i8,
            };
            Bytes::copy_from_slice(&r.to_be_bytes())
        }
        CqlType::Smallint => {
            let v = i16::from_be_bytes([fixed(2)?[0], b[1]]);
            let r = match op {
                Op::Abs => (v as i32).abs() as i16,
                Op::Round => v,
                _ => d2i(long_op(op, v as i64)) as i16,
            };
            Bytes::copy_from_slice(&r.to_be_bytes())
        }
        CqlType::Int => {
            let v = i32::from_be_bytes(fixed(4)?.try_into().map_err(|_| bad())?);
            let r = match op {
                Op::Abs => v.wrapping_abs(),
                Op::Round => v,
                _ => d2i(long_op(op, v as i64)),
            };
            Bytes::copy_from_slice(&r.to_be_bytes())
        }
        CqlType::Bigint | CqlType::Counter => {
            let v = i64::from_be_bytes(fixed(8)?.try_into().map_err(|_| bad())?);
            let r = match op {
                Op::Abs => v.wrapping_abs(),
                Op::Round => v,
                _ => long_op(op, v) as i64,
            };
            Bytes::copy_from_slice(&r.to_be_bytes())
        }
        CqlType::Float => {
            let v = f32::from_be_bytes(fixed(4)?.try_into().map_err(|_| bad())?);
            let x = v as f64;
            let r = match op {
                Op::Abs => v.abs(),
                Op::Exp => x.exp() as f32,
                Op::Log => x.ln() as f32,
                Op::Log10 => x.log10() as f32,
                // `Math.round(float)` is an int.
                Op::Round => {
                    if v.is_nan() {
                        0.0
                    } else {
                        ((x + 0.5).floor() as i32) as f32
                    }
                }
            };
            Bytes::copy_from_slice(&r.to_be_bytes())
        }
        CqlType::Double => {
            let v = f64::from_be_bytes(fixed(8)?.try_into().map_err(|_| bad())?);
            let r = match op {
                Op::Abs => v.abs(),
                Op::Exp => v.exp(),
                Op::Log => v.ln(),
                Op::Log10 => v.log10(),
                Op::Round => round_half_up(v) as f64,
            };
            Bytes::copy_from_slice(&r.to_be_bytes())
        }
        CqlType::Decimal => {
            let d = nosql_types::Decimal::from_bytes(b).ok_or_else(bad)?;
            let x = Dec::from_decimal(&d);
            let r = decimal_op(op, &x)?;
            Bytes::from(r.to_decimal().map_err(arith)?.to_bytes())
        }
        CqlType::Varint => {
            let v = nosql_types::num::varint_from_bytes(b);
            let r = varint_op(op, &v)?;
            Bytes::from(nosql_types::num::varint_to_bytes(&r))
        }
        _ => return Err(bad()),
    })
}

/// `DecimalType`'s `abs` / `exp` / `log` / `log10` / `round`.
fn decimal_op(op: Op, x: &Dec) -> Result<Dec> {
    let p = bigdec::precision_for(x);
    match op {
        Op::Abs => Ok(x.abs()),
        Op::Round => Ok(x.set_scale(0, bigdec::Mode::HalfUp)),
        Op::Exp => bigdec::exp(x, p).map_err(arith),
        Op::Log => {
            if x.signum() <= 0 {
                return Err(arith(ArithError("Natural log of number zero or less")));
            }
            bigdec::log(x, p).map_err(arith)
        }
        Op::Log10 => {
            if x.signum() <= 0 {
                return Err(arith(ArithError("Log10 of number zero or less")));
            }
            bigdec::log10(x, p).map_err(arith)
        }
    }
}

/// `IntegerType`'s: through `new BigDecimal(bi)`, then `toBigInteger`.
fn varint_op(op: Op, v: &BigInt) -> Result<BigInt> {
    match op {
        Op::Abs => Ok(v.abs()),
        Op::Round => Ok(v.clone()),
        Op::Log if !v.is_positive() => Err(arith(ArithError("Natural log of number zero or less"))),
        Op::Log10 if !v.is_positive() => Err(arith(ArithError("Log10 of number zero or less"))),
        _ => {
            let x = Dec::new(v.clone(), 0);
            let r = decimal_op(op, &x)?;
            // The integer part of a huge `exp` would be gigabytes of digits.
            if r.precision() as i64 - r.scale > bigdec::MAX_VARINT_EXP_DIGITS {
                return Err(arith(ArithError("Overflow")));
            }
            if r.unscaled.is_zero() {
                return Ok(BigInt::zero());
            }
            Ok(r.to_bigint())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(op: Op, ty: CqlType, v: &[u8]) -> Bytes {
        compute(op, &ty, v).unwrap()
    }

    #[test]
    fn integral_types_follow_java_casts() {
        assert_eq!(run(Op::Abs, CqlType::Tinyint, &[0x80]).as_ref(), &[0x80]);
        assert_eq!(run(Op::Log, CqlType::Tinyint, &[0x80]).as_ref(), &[0]);
        assert_eq!(run(Op::Exp, CqlType::Tinyint, &[100]).as_ref(), &[0xff]);
        assert_eq!(run(Op::Log, CqlType::Tinyint, &[100]).as_ref(), &[4]);
        assert_eq!(
            run(Op::Abs, CqlType::Int, &i32::MIN.to_be_bytes()).as_ref(),
            &i32::MIN.to_be_bytes()
        );
        assert_eq!(
            run(Op::Log10, CqlType::Smallint, &30000i16.to_be_bytes()).as_ref(),
            &4i16.to_be_bytes()
        );
        assert_eq!(
            run(Op::Exp, CqlType::Bigint, &50i64.to_be_bytes()).as_ref(),
            &i64::MAX.to_be_bytes()
        );
    }

    #[test]
    fn rounding() {
        let f = |v: f32| {
            f32::from_be_bytes(
                run(Op::Round, CqlType::Float, &v.to_be_bytes())[..]
                    .try_into()
                    .unwrap(),
            )
        };
        assert_eq!(f(0.49999997), 0.0);
        assert_eq!(f(-2.5), -2.0);
        assert_eq!(f(1.5), 2.0);
        let d = |v: f64| {
            f64::from_be_bytes(
                run(Op::Round, CqlType::Double, &v.to_be_bytes())[..]
                    .try_into()
                    .unwrap(),
            )
        };
        assert_eq!(d(2.5), 3.0);
        assert_eq!(d(-2.5), -2.0);
        assert_eq!(d(1e300), 9.223372036854776e18);
    }

    #[test]
    fn varints() {
        let v = |x: i64| Bytes::from(nosql_types::num::varint_to_bytes(&BigInt::from(x)));
        assert_eq!(run(Op::Exp, CqlType::Varint, &v(-5)), v(0));
        assert_eq!(run(Op::Exp, CqlType::Varint, &v(10)), v(22026));
        assert_eq!(run(Op::Log, CqlType::Varint, &v(100)), v(4));
        assert_eq!(run(Op::Log10, CqlType::Varint, &v(1000)), v(3));
        assert_eq!(
            compute(Op::Log, &CqlType::Varint, &v(0))
                .unwrap_err()
                .message,
            "java.lang.ArithmeticException: Natural log of number zero or less"
        );
    }
}
