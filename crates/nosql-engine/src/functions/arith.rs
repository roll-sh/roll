//! Arithmetic operators (`OperationFcts`, Apache-2.0): `+ - * / %` between numeric types, the
//! result type being the wider of the operands (floating point wins), timestamp/date `± duration`,
//! string concatenation, and unary minus. Integer arithmetic wraps like Java's; division by zero
//! fails with `the operation 'int / int' failed: / by zero`. Also the decimal helpers the
//! aggregates use (`BigDecimal` semantics).

use std::sync::Arc;

use bytes::Bytes;
use nosql_types::{CqlType, Decimal};
use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};

use super::aggregates::{Num, read_num, write_int};
use super::{FnCtx, FnName, Function, Registry};
use crate::error::{Error, Result};

pub const ADD: &str = "_add";
pub const SUB: &str = "_substract";
pub const MUL: &str = "_multiply";
pub const DIV: &str = "_divide";
pub const MOD: &str = "_modulo";
pub const NEGATE: &str = "_negate";

/// The operator symbol of an operation function name.
pub fn operator_of(name: &str) -> Option<char> {
    Some(match name {
        ADD => '+',
        SUB => '-',
        MUL => '*',
        DIV => '/',
        MOD => '%',
        _ => return None,
    })
}

pub fn function_of(op: nosql_cql::ast::ArithOp) -> &'static str {
    use nosql_cql::ast::ArithOp as A;
    match op {
        A::Add => ADD,
        A::Sub => SUB,
        A::Mul => MUL,
        A::Div => DIV,
        A::Mod => MOD,
    }
}

const NUMERIC: [CqlType; 9] = [
    CqlType::Tinyint,
    CqlType::Smallint,
    CqlType::Int,
    CqlType::Bigint,
    CqlType::Float,
    CqlType::Double,
    CqlType::Decimal,
    CqlType::Varint,
    CqlType::Counter,
];

fn size(t: &CqlType) -> usize {
    match t {
        CqlType::Tinyint => 1,
        CqlType::Smallint => 2,
        CqlType::Int | CqlType::Float => 4,
        CqlType::Bigint | CqlType::Double | CqlType::Counter => 8,
        _ => usize::MAX,
    }
}

fn is_float(t: &CqlType) -> bool {
    matches!(t, CqlType::Float | CqlType::Double | CqlType::Decimal)
}

/// `OperationFcts.returnType`.
pub fn result_type(l: &CqlType, r: &CqlType) -> CqlType {
    let s = size(l).max(size(r));
    if is_float(l) || is_float(r) {
        match s {
            4 => CqlType::Float,
            8 => CqlType::Double,
            _ => CqlType::Decimal,
        }
    } else {
        match s {
            1 => CqlType::Tinyint,
            2 => CqlType::Smallint,
            4 => CqlType::Int,
            8 => CqlType::Bigint,
            _ => CqlType::Varint,
        }
    }
}

pub fn register(r: &mut Registry) {
    for l in &NUMERIC {
        for rt in &NUMERIC {
            let ret = result_type(l, rt);
            for name in [ADD, SUB, MUL, DIV, MOD] {
                r.add(Arc::new(Op {
                    name: FnName::system(name),
                    args: vec![l.clone(), rt.clone()],
                    ret: ret.clone(),
                }));
            }
        }
        r.add(Arc::new(Op {
            name: FnName::system(NEGATE),
            args: vec![l.clone()],
            ret: l.clone(),
        }));
    }
    for t in [CqlType::Timestamp, CqlType::Date] {
        for name in [ADD, SUB] {
            r.add(Arc::new(Op {
                name: FnName::system(name),
                args: vec![t.clone(), CqlType::Duration],
                ret: t.clone(),
            }));
        }
    }
    for (ret, l, rt) in [
        (CqlType::Text, CqlType::Text, CqlType::Text),
        (CqlType::Ascii, CqlType::Ascii, CqlType::Ascii),
        (CqlType::Text, CqlType::Ascii, CqlType::Text),
        (CqlType::Text, CqlType::Text, CqlType::Ascii),
    ] {
        r.add(Arc::new(Op {
            name: FnName::system(ADD),
            args: vec![l, rt],
            ret,
        }));
    }
}

#[derive(Debug)]
pub struct Op {
    name: FnName,
    args: Vec<CqlType>,
    ret: CqlType,
}

impl Function for Op {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &self.ret
    }
    fn null_on_null_input(&self) -> bool {
        true
    }
    fn execute(&self, _ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        if args.iter().any(|a| a.is_none()) {
            return Ok(None);
        }
        let a: Vec<&Bytes> = args.iter().flatten().collect();
        if self.name.name == NEGATE {
            return Ok(negate(&self.ret, a[0]));
        }
        let op = operator_of(&self.name.name).unwrap_or('+');
        let fail = |msg: &str| {
            Error::function_failure(
                format!(
                    "the operation '{} {op} {}' failed: {msg}",
                    nosql_types::cql_name(&self.args[0]),
                    nosql_types::cql_name(&self.args[1])
                ),
                "system",
                &self.name.name,
                self.args.iter().map(nosql_types::cql_name).collect(),
            )
        };
        match (&self.args[0], &self.args[1]) {
            (CqlType::Timestamp | CqlType::Date, CqlType::Duration) => {
                return temporal(&self.ret, op, a[0], a[1])
                    .ok_or_else(|| fail("invalid temporal operation"))
                    .map(Some);
            }
            (CqlType::Text | CqlType::Ascii, _) => {
                let mut v = a[0].to_vec();
                v.extend_from_slice(a[1]);
                return Ok(Some(Bytes::from(v)));
            }
            _ => {}
        }
        let (Some(l), Some(r)) = (read_num(&self.args[0], a[0]), read_num(&self.args[1], a[1]))
        else {
            return Ok(None);
        };
        numeric(&self.ret, op, l, r).map(Some).map_err(|m| fail(&m))
    }
}

fn negate(ty: &CqlType, b: &[u8]) -> Option<Bytes> {
    Some(match read_num(ty, b)? {
        Num::I(v) => write_int(ty, v.wrapping_neg()),
        Num::F(v) if *ty == CqlType::Float => Bytes::copy_from_slice(&(-(v as f32)).to_be_bytes()),
        Num::F(v) => Bytes::copy_from_slice(&(-v).to_be_bytes()),
        Num::Big(v) => Bytes::from((-v).to_signed_bytes_be()),
        Num::Dec(d) => Bytes::from(Decimal::new(-d.unscaled, d.scale).to_bytes()),
    })
}

fn to_i64(n: &Num) -> i64 {
    match n {
        Num::I(v) => *v,
        Num::F(v) => *v as i64,
        Num::Big(v) => v.to_i64().unwrap_or_else(|| {
            let bytes = v.to_signed_bytes_le();
            let mut b = [if v.is_negative() { 0xff } else { 0 }; 8];
            for (i, x) in bytes.iter().take(8).enumerate() {
                b[i] = *x;
            }
            i64::from_le_bytes(b)
        }),
        Num::Dec(d) => d.to_i64_wrapping(),
    }
}

fn to_f64(n: &Num) -> f64 {
    match n {
        Num::I(v) => *v as f64,
        Num::F(v) => *v,
        Num::Big(v) => nosql_types::num::bigint_to_f64(v),
        Num::Dec(d) => d.to_f64(),
    }
}

fn to_big(n: &Num) -> BigInt {
    match n {
        Num::I(v) => BigInt::from(*v),
        Num::F(v) => BigInt::from(*v as i64),
        Num::Big(v) => v.clone(),
        Num::Dec(d) => d.to_bigint_trunc().unwrap_or_default(),
    }
}

/// `DecimalType.toBigDecimal`: floating point values through their shortest decimal text.
pub fn to_dec(n: &Num) -> Decimal {
    match n {
        Num::I(v) => Decimal::from_i64(*v),
        Num::F(v) => Decimal::parse(&nosql_types::float::java_double(*v))
            .unwrap_or_else(|| Decimal::from_i64(0)),
        Num::Big(v) => Decimal::new(v.clone(), 0),
        Num::Dec(d) => d.clone(),
    }
}

fn numeric(ret: &CqlType, op: char, l: Num, r: Num) -> std::result::Result<Bytes, String> {
    match ret {
        CqlType::Tinyint
        | CqlType::Smallint
        | CqlType::Int
        | CqlType::Bigint
        | CqlType::Counter => {
            let (a, b) = (to_i64(&l), to_i64(&r));
            let v = match ret {
                CqlType::Tinyint => int_op(a as i8 as i64, b as i8 as i64, op, |x| x as i8 as i64)?,
                CqlType::Smallint => {
                    int_op(a as i16 as i64, b as i16 as i64, op, |x| x as i16 as i64)?
                }
                CqlType::Int => int_op(a as i32 as i64, b as i32 as i64, op, |x| x as i32 as i64)?,
                _ => {
                    if (op == '/' || op == '%') && b == 0 {
                        return Err("/ by zero".into());
                    }
                    match op {
                        '+' => a.wrapping_add(b),
                        '-' => a.wrapping_sub(b),
                        '*' => a.wrapping_mul(b),
                        '/' => a.wrapping_div(b),
                        _ => a.wrapping_rem(b),
                    }
                }
            };
            Ok(write_int(ret, v))
        }
        CqlType::Float => {
            let (a, b) = (to_f64(&l) as f32, to_f64(&r) as f32);
            let v = match op {
                '+' => a + b,
                '-' => a - b,
                '*' => a * b,
                '/' => a / b,
                _ => a % b,
            };
            Ok(Bytes::copy_from_slice(&v.to_be_bytes()))
        }
        CqlType::Double => {
            let (a, b) = (to_f64(&l), to_f64(&r));
            let v = match op {
                '+' => a + b,
                '-' => a - b,
                '*' => a * b,
                '/' => a / b,
                _ => a % b,
            };
            Ok(Bytes::copy_from_slice(&v.to_be_bytes()))
        }
        CqlType::Varint => {
            let (a, b) = (to_big(&l), to_big(&r));
            if (op == '/' || op == '%') && b.is_zero() {
                return Err("BigInteger divide by zero".into());
            }
            let v = match op {
                '+' => a + b,
                '-' => a - b,
                '*' => a * b,
                '/' => a / b,
                _ => a % b,
            };
            Ok(Bytes::from(v.to_signed_bytes_be()))
        }
        _ => {
            let (a, b) = (to_dec(&l), to_dec(&r));
            let v = match op {
                '+' => dec_add(&a, &b),
                '-' => dec_sub(&a, &b),
                '*' => Decimal::new(&a.unscaled * &b.unscaled, a.scale.saturating_add(b.scale)),
                '/' => {
                    if b.unscaled.is_zero() {
                        return Err(if a.unscaled.is_zero() {
                            "Division undefined".into()
                        } else {
                            "Division by zero".into()
                        });
                    }
                    dec_div_cassandra(&a, &b)
                }
                _ => {
                    if b.unscaled.is_zero() {
                        return Err("Division by zero".into());
                    }
                    dec_rem(&a, &b)
                }
            };
            Ok(Bytes::from(v.to_bytes()))
        }
    }
}

fn int_op(a: i64, b: i64, op: char, narrow: fn(i64) -> i64) -> std::result::Result<i64, String> {
    if (op == '/' || op == '%') && b == 0 {
        return Err("/ by zero".into());
    }
    Ok(narrow(match op {
        '+' => a + b,
        '-' => a - b,
        '*' => a * b,
        '/' => a / b,
        _ => a % b,
    }))
}

fn temporal(ret: &CqlType, op: char, t: &[u8], dur: &[u8]) -> Option<Bytes> {
    let d = match nosql_types::decode(&CqlType::Duration, dur).ok()? {
        nosql_types::Value::Duration(d) => d,
        _ => return None,
    };
    let sign: i64 = if op == '-' { -1 } else { 1 };
    match ret {
        CqlType::Timestamp => {
            let ms = i64::from_be_bytes(t.try_into().ok()?);
            Some(Bytes::copy_from_slice(
                &add_duration_ms(ms, &d, sign)?.to_be_bytes(),
            ))
        }
        CqlType::Date => {
            let days = u32::from_be_bytes(t.try_into().ok()?);
            let ms = nosql_types::temporal::date_to_millis(days);
            let out = add_duration_ms(ms, &d, sign)?;
            Some(Bytes::copy_from_slice(
                &nosql_types::temporal::millis_to_date(out, nosql_types::Dialect::Cassandra)
                    .to_be_bytes(),
            ))
        }
        _ => None,
    }
}

/// `Duration.addTo`: months via the calendar, then days and nanoseconds.
fn add_duration_ms(ms: i64, d: &nosql_types::Duration, sign: i64) -> Option<i64> {
    let days_total = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, day) = nosql_types::temporal::civil_from_days(days_total);
    let months = m as i64 - 1 + sign * d.months as i64;
    let ny = y + months.div_euclid(12);
    let nm = (months.rem_euclid(12) + 1) as u32;
    let nd = day.min(nosql_types::temporal::month_length(ny, nm));
    let base = nosql_types::temporal::days_from_civil(ny, nm, nd) + sign * d.days as i64;
    base.checked_mul(86_400_000)?
        .checked_add(rem)?
        .checked_add(sign * d.nanos / 1_000_000)
}

fn align(a: &Decimal, b: &Decimal) -> (BigInt, BigInt, i32) {
    let scale = a.scale.max(b.scale);
    let up = |d: &Decimal| -> BigInt {
        let diff = (scale - d.scale) as u32;
        &d.unscaled * BigInt::from(10).pow(diff)
    };
    (up(a), up(b), scale)
}

pub fn dec_add(a: &Decimal, b: &Decimal) -> Decimal {
    let (x, y, s) = align(a, b);
    Decimal::new(x + y, s)
}

pub fn dec_sub(a: &Decimal, b: &Decimal) -> Decimal {
    let (x, y, s) = align(a, b);
    Decimal::new(x - y, s)
}

fn dec_rem(a: &Decimal, b: &Decimal) -> Decimal {
    let (x, y, s) = align(a, b);
    Decimal::new(x % y, s)
}

/// `a / b` at `a`'s scale, rounding HALF_EVEN (`BigDecimal.divide(divisor, RoundingMode)`).
pub fn dec_div_scale(a: &Decimal, b: &Decimal) -> Decimal {
    div_round(a, b, a.scale, true)
}

/// `a / b` at `scale`, rounding HALF_EVEN or HALF_UP.
fn div_round(a: &Decimal, b: &Decimal, scale: i32, half_even: bool) -> Decimal {
    if b.unscaled.is_zero() {
        return Decimal::new(BigInt::from(0), scale);
    }
    // a.unscaled * 10^(scale - a.scale + b.scale) / b.unscaled
    let shift = scale as i64 - a.scale as i64 + b.scale as i64;
    let (num, den) = if shift >= 0 {
        (
            &a.unscaled * BigInt::from(10).pow(shift.min(2000) as u32),
            b.unscaled.clone(),
        )
    } else {
        (
            a.unscaled.clone(),
            &b.unscaled * BigInt::from(10).pow((-shift).min(2000) as u32),
        )
    };
    let q = &num / &den;
    let r = &num % &den;
    let twice: std::cmp::Ordering = (r.abs() * BigInt::from(2)).cmp(&den.abs());
    let negative = num.is_negative() != den.is_negative();
    let bump = match twice {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Equal => !half_even || (&q % 2u8) != BigInt::from(0),
        std::cmp::Ordering::Less => false,
    };
    let q = if bump && !r.is_zero() {
        if negative { q - 1 } else { q + 1 }
    } else {
        q
    };
    Decimal::new(q, scale)
}

fn precision(d: &Decimal) -> i64 {
    if d.unscaled.is_zero() {
        1
    } else {
        d.unscaled.abs().to_string().len() as i64
    }
}

/// `DecimalType.divide`.
fn dec_div_cassandra(a: &Decimal, b: &Decimal) -> Decimal {
    let first = (precision(a) - a.scale as i64) - (precision(b) - b.scale as i64);
    let mut scale = 32 - first;
    scale = scale
        .max(a.scale as i64)
        .max(b.scale as i64)
        .clamp(32, 1000);
    let q = div_round(a, b, scale as i32, false);
    strip_trailing_zeros(q)
}

fn strip_trailing_zeros(mut d: Decimal) -> Decimal {
    if d.unscaled.is_zero() {
        return Decimal::new(BigInt::from(0), 0);
    }
    let ten = BigInt::from(10);
    while (&d.unscaled % &ten).is_zero() {
        d.unscaled /= &ten;
        d.scale -= 1;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::super::Typed;
    use super::*;

    fn exec(
        r: &Registry,
        name: &str,
        lt: CqlType,
        rt: CqlType,
        a: Bytes,
        b: Bytes,
    ) -> Result<Option<Bytes>> {
        let l = Typed {
            ty: lt,
            text: "a".into(),
        };
        let rr = Typed {
            ty: rt,
            text: "b".into(),
        };
        let f = r.resolve(None, name, &[&l, &rr], None, None)?.unwrap();
        f.execute(
            &FnCtx {
                dialect: nosql_types::Dialect::Cassandra,
                protocol_version: 4,
            },
            &[Some(a), Some(b)],
        )
    }

    fn i(v: i32) -> Bytes {
        Bytes::copy_from_slice(&v.to_be_bytes())
    }

    #[test]
    fn integer_semantics() {
        let r = Registry::new(nosql_types::Dialect::Cassandra);
        assert_eq!(
            exec(&r, ADD, CqlType::Int, CqlType::Int, i(i32::MAX), i(1))
                .unwrap()
                .unwrap(),
            i(i32::MIN)
        );
        assert_eq!(
            exec(&r, DIV, CqlType::Int, CqlType::Int, i(7), i(2))
                .unwrap()
                .unwrap(),
            i(3)
        );
        assert_eq!(
            exec(&r, MOD, CqlType::Int, CqlType::Int, i(-7), i(2))
                .unwrap()
                .unwrap(),
            i(-1)
        );
        let e = exec(&r, DIV, CqlType::Int, CqlType::Int, i(1), i(0)).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::FunctionFailure);
        assert_eq!(e.message, "the operation 'int / int' failed: / by zero");
        let big = exec(
            &r,
            MUL,
            CqlType::Int,
            CqlType::Bigint,
            i(3),
            Bytes::copy_from_slice(&5i64.to_be_bytes()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(big[..], 15i64.to_be_bytes());
        assert_eq!(result_type(&CqlType::Int, &CqlType::Float), CqlType::Float);
        assert_eq!(
            result_type(&CqlType::Bigint, &CqlType::Float),
            CqlType::Double
        );
        assert_eq!(
            result_type(&CqlType::Varint, &CqlType::Float),
            CqlType::Decimal
        );
        assert_eq!(
            result_type(&CqlType::Tinyint, &CqlType::Smallint),
            CqlType::Smallint
        );
    }

    #[test]
    fn decimal_division() {
        let a = Decimal::parse("1").unwrap();
        let b = Decimal::parse("3").unwrap();
        assert_eq!(
            dec_div_cassandra(&a, &b).to_java_string(),
            "0.33333333333333333333333333333333"
        );
        let a = Decimal::parse("10").unwrap();
        let b = Decimal::parse("4").unwrap();
        assert_eq!(dec_div_cassandra(&a, &b).to_java_string(), "2.5");
        assert_eq!(
            dec_div_scale(&Decimal::parse("1.0").unwrap(), &Decimal::from_i64(2)).to_java_string(),
            "0.5"
        );
    }

    #[test]
    fn temporal_and_text() {
        let r = Registry::new(nosql_types::Dialect::Cassandra);
        let one_day = Bytes::from(
            nosql_types::encode(
                &nosql_types::Value::Duration(nosql_types::Duration::new(0, 1, 0)),
                &CqlType::Duration,
            )
            .to_vec(),
        );
        let ts = Bytes::copy_from_slice(&86_400_000i64.to_be_bytes());
        let out = exec(&r, ADD, CqlType::Timestamp, CqlType::Duration, ts, one_day)
            .unwrap()
            .unwrap();
        assert_eq!(out[..], (2 * 86_400_000i64).to_be_bytes());
        let s = exec(
            &r,
            ADD,
            CqlType::Text,
            CqlType::Text,
            Bytes::from_static(b"a"),
            Bytes::from_static(b"b"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(s, "ab");
    }
}
