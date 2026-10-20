//! Mathematical functions.

use super::{FnCtx, Registry, Ret, float_arg, int_arg};
use crate::err;
use crate::error::Result;
use crate::types::numeric::Numeric;
use crate::types::{DataType as T, Value};
use num_bigint::BigInt;
use num_traits::One;

fn num(v: &Value) -> Numeric {
    match v {
        Value::Numeric(n) => (**n).clone(),
        v => v
            .as_i64()
            .map(Numeric::from_i64)
            .unwrap_or_else(|| Numeric::from_f64(v.as_f64().unwrap_or(0.0))),
    }
}

fn f8(v: f64) -> Result<Value> {
    Ok(Value::Float8(v))
}

pub fn float_power(x: f64, y: f64) -> Result<Value> {
    if x == 0.0 && y < 0.0 {
        return Err(err!(
            INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "zero raised to a negative power is undefined"
        ));
    }
    if x < 0.0 && y.fract() != 0.0 && y.is_finite() {
        return Err(err!(
            INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "a negative number raised to a non-integer power yields a complex result"
        ));
    }
    let r = x.powf(y);
    if r.is_infinite() && x.is_finite() && y.is_finite() {
        return Err(err!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: overflow"
        ));
    }
    if r == 0.0 && x != 0.0 && y.is_finite() && x.is_finite() {
        return Err(err!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: underflow"
        ));
    }
    f8(r)
}

pub fn numeric_power(x: &Numeric, y: &Numeric) -> Result<Value> {
    Ok(Value::numeric(x.power(y)?))
}

pub fn sqrt_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    if x < 0.0 {
        return Err(err!(
            INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "cannot take square root of a negative number"
        ));
    }
    f8(x.sqrt())
}

pub fn cbrt_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    f8(float_arg(&a[0]).cbrt())
}

fn abs_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).abs()))
}
fn abs_f(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(match c.ret {
        T::Float4 => Value::Float4(float_arg(&a[0]).abs() as f32),
        _ => Value::Float8(float_arg(&a[0]).abs()),
    })
}
fn abs_i(a: &[Value], c: &FnCtx) -> Result<Value> {
    let v = int_arg(&a[0]) as i128;
    let r = v.abs();
    match c.ret {
        T::Int2 => i16::try_from(r)
            .map(Value::Int2)
            .map_err(|_| err!(NUMERIC_VALUE_OUT_OF_RANGE, "smallint out of range")),
        T::Int4 => i32::try_from(r)
            .map(Value::Int4)
            .map_err(|_| err!(NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range")),
        _ => i64::try_from(r)
            .map(Value::Int8)
            .map_err(|_| err!(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")),
    }
}
fn round_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    let d = a.get(1).map(int_arg).unwrap_or(0);
    Ok(Value::numeric(num(&a[0]).round(d).checked()?))
}
fn round_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    f8(float_arg(&a[0]).round_ties_even())
}
fn trunc_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    let d = a.get(1).map(int_arg).unwrap_or(0);
    Ok(Value::numeric(num(&a[0]).trunc(d).checked()?))
}
fn trunc_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    f8(float_arg(&a[0]).trunc())
}
fn ceil_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).ceil()))
}
fn ceil_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    f8(float_arg(&a[0]).ceil())
}
fn floor_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).floor()))
}
fn floor_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    f8(float_arg(&a[0]).floor())
}
fn sign_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).sign()))
}
fn sign_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    f8(if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    })
}
fn sqrt_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).sqrt()?))
}
/// dexp.
fn exp_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    if x.is_nan() {
        return f8(x);
    }
    if x.is_infinite() {
        return f8(if x > 0.0 { x } else { 0.0 });
    }
    let r = x.exp();
    if r.is_infinite() {
        return Err(float_overflow());
    }
    if r == 0.0 {
        return Err(float_underflow());
    }
    f8(r)
}
fn float_overflow() -> crate::error::Error {
    err!(NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}
fn float_underflow() -> crate::error::Error {
    err!(NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: underflow")
}
fn input_out_of_range() -> crate::error::Error {
    err!(NUMERIC_VALUE_OUT_OF_RANGE, "input is out of range")
}
fn exp_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).exp()?))
}
fn ln_check(x: f64) -> Result<()> {
    if x == 0.0 {
        return Err(err!(
            INVALID_ARGUMENT_FOR_LOGARITHM,
            "cannot take logarithm of zero"
        ));
    }
    if x < 0.0 {
        return Err(err!(
            INVALID_ARGUMENT_FOR_LOGARITHM,
            "cannot take logarithm of a negative number"
        ));
    }
    Ok(())
}
fn ln_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    ln_check(x)?;
    f8(x.ln())
}
fn log10_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    ln_check(x)?;
    f8(x.log10())
}
fn ln_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).ln()?))
}
/// log(numeric) and log10(numeric) are SQL functions calling log(10, $1).
fn log10_sql(a: &[Value], name: &str) -> Result<Value> {
    Numeric::from_i64(10)
        .log(&num(&a[0]))
        .map(Value::numeric)
        .map_err(|e| e.context(format!("SQL function \"{name}\" statement 1")))
}
fn log10_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    log10_sql(a, "log10")
}
fn log_num1(a: &[Value], _: &FnCtx) -> Result<Value> {
    log10_sql(a, "log")
}
fn gcd_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).gcd(&num(&a[1]))))
}
fn lcm_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).lcm(&num(&a[1]))?))
}
fn log_num2(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).log(&num(&a[1]))?))
}
fn power_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    float_power(float_arg(&a[0]), float_arg(&a[1]))
}
fn power_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    numeric_power(&num(&a[0]), &num(&a[1]))
}
fn mod_i(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (x, y) = (int_arg(&a[0]) as i128, int_arg(&a[1]) as i128);
    if y == 0 {
        return Err(err!(DIVISION_BY_ZERO, "division by zero"));
    }
    Ok(match c.ret {
        T::Int2 => Value::Int2((x % y) as i16),
        T::Int4 => Value::Int4((x % y) as i32),
        _ => Value::Int8((x % y) as i64),
    })
}
fn mod_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).rem(&num(&a[1]))?))
}
fn div_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).div_trunc(&num(&a[1]))?))
}
fn pi(_: &[Value], _: &FnCtx) -> Result<Value> {
    f8(std::f64::consts::PI)
}
fn random(_: &[Value], c: &FnCtx) -> Result<Value> {
    f8(c.exec.random())
}
fn random_range_i(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (lo, hi) = (int_arg(&a[0]), int_arg(&a[1]));
    if lo > hi {
        return Err(err!(
            INVALID_PARAMETER_VALUE,
            "lower bound must be less than or equal to upper bound"
        ));
    }
    // pg_prng_int64_range over the bounds.
    let v = c.exec.random_i64_range(lo, hi);
    Ok(match c.ret {
        T::Int4 => Value::Int4(v as i32),
        _ => Value::Int8(v),
    })
}

/// drandom_normal: mean + stddev * a standard normal draw.
fn random_normal(a: &[Value], c: &FnCtx) -> Result<Value> {
    let mean = a.first().map_or(0.0, float_arg);
    let stddev = a.get(1).map_or(1.0, float_arg);
    let z = c.exec.random_normal();
    f8(stddev * z + mean)
}

/// numeric_random (random_var): uniform in [min, max] at the larger of their scales, its
/// base-10000 digits drawn as Postgres draws them.
fn random_range_num(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (lo, hi) = (num(&a[0]), num(&a[1]));
    for (v, which) in [(&lo, "lower"), (&hi, "upper")] {
        match v {
            Numeric::NaN => {
                return Err(err!(INVALID_PARAMETER_VALUE, "{which} bound cannot be NaN"));
            }
            Numeric::PosInf | Numeric::NegInf => {
                return Err(err!(INVALID_PARAMETER_VALUE, "{which} bound cannot be infinity"));
            }
            _ => {}
        }
    }
    let rscale = lo.scale().max(hi.scale());
    let rlen = hi.sub(&lo);
    if rlen.is_negative() {
        return Err(err!(
            INVALID_PARAMETER_VALUE,
            "lower bound must be less than or equal to upper bound"
        ));
    }
    const NBASE: u64 = 10000;
    // rlen's NumericVar digits and weight (stripped).
    let (rdigits, rweight) = nbase_digits(&rlen);
    if rdigits.is_empty() {
        return Ok(Value::numeric(lo.with_scale(rscale)));
    }
    let dec = 4i32;
    let res_ndigits = (rweight + 1 + (rscale as i32 + dec - 1) / dec) as usize;
    let n = ((rscale as i32 + dec - 1) / dec) * dec - rscale as i32;
    let pow10 = 10u64.pow(n as u32);
    let mut rlen64 = rdigits[0];
    let mut rlen64_ndigits = 1usize;
    while rlen64_ndigits < res_ndigits && rlen64_ndigits < 4 {
        rlen64 *= NBASE;
        if rlen64_ndigits < rdigits.len() {
            rlen64 += rdigits[rlen64_ndigits];
        }
        rlen64_ndigits += 1;
    }
    loop {
        let mut digits = vec![0u64; res_ndigits];
        let mut rand = if rlen64_ndigits == res_ndigits && pow10 != 1 {
            c.exec.random_u64_range(0, rlen64 / pow10) * pow10
        } else {
            c.exec.random_u64_range(0, rlen64)
        };
        for i in (0..rlen64_ndigits).rev() {
            digits[i] = rand % NBASE;
            rand /= NBASE;
        }
        let whole = if pow10 != 1 { res_ndigits - 1 } else { res_ndigits };
        let mut i = rlen64_ndigits;
        while i + 3 < whole {
            let mut r = c.exec.random_u64_range(0, NBASE * NBASE * NBASE * NBASE - 1);
            for _ in 0..3 {
                digits[i] = r % NBASE;
                r /= NBASE;
                i += 1;
            }
            digits[i] = r;
            i += 1;
        }
        while i < whole {
            digits[i] = c.exec.random_u64_range(0, NBASE - 1);
            i += 1;
        }
        if i < res_ndigits {
            digits[i] = c.exec.random_u64_range(0, NBASE / pow10 - 1) * pow10;
        }
        // The value: digits at weight rweight, as int * 10^-(4 * fractional digits).
        let mut int = BigInt::from(0u32);
        for d in &digits {
            int = int * NBASE + BigInt::from(*d);
        }
        let frac = 4 * (res_ndigits as i32 - 1 - rweight);
        let v = Numeric::from_parts(int, frac.max(0) as u32);
        if v.sub(&rlen).is_negative() || v.sub(&rlen).is_zero() {
            return Ok(Value::numeric(v.add(&lo).with_scale(rscale)));
        }
    }
}

/// A non-negative finite numeric's base-10000 digits (leading and trailing zero digits
/// stripped) and the weight of the first.
fn nbase_digits(v: &Numeric) -> (Vec<u64>, i32) {
    let Numeric::Finite { int, scale } = v else {
        return (Vec::new(), 0);
    };
    let s4 = scale.div_ceil(4) * 4;
    let scaled = int * BigInt::from(10u32).pow(s4 - scale);
    let text = scaled.to_string();
    let text = text.trim_start_matches('-');
    if text.chars().all(|ch| ch == '0') {
        return (Vec::new(), 0);
    }
    let pad = (4 - text.len() % 4) % 4;
    let padded = format!("{}{text}", "0".repeat(pad));
    let mut digits: Vec<u64> = padded
        .as_bytes()
        .chunks(4)
        .map(|ch| std::str::from_utf8(ch).unwrap().parse().unwrap())
        .collect();
    let mut weight = digits.len() as i32 - (s4 / 4) as i32 - 1;
    while digits.first() == Some(&0) {
        digits.remove(0);
        weight -= 1;
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    (digits, weight)
}
const RADIANS_PER_DEGREE: f64 = 0.017_453_292_519_943_295;

/// ddegrees: float8_div(x, RADIANS_PER_DEGREE).
fn degrees(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    let r = x / RADIANS_PER_DEGREE;
    if r.is_infinite() && !x.is_infinite() {
        return Err(float_overflow());
    }
    if r == 0.0 && x != 0.0 {
        return Err(float_underflow());
    }
    f8(r)
}
/// dradians: float8_mul(x, RADIANS_PER_DEGREE).
fn radians(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    let r = x * RADIANS_PER_DEGREE;
    if r.is_infinite() && !x.is_infinite() {
        return Err(float_overflow());
    }
    if r == 0.0 && x != 0.0 {
        return Err(float_underflow());
    }
    f8(r)
}

// float.c's degree-based trigonometry, exact at the "nice" angles.
struct DegreeConsts {
    sin_30: f64,
    one_minus_cos_60: f64,
    asin_0_5: f64,
    acos_0_5: f64,
    atan_1_0: f64,
    tan_45: f64,
    cot_45: f64,
}

fn degree_consts() -> &'static DegreeConsts {
    static C: std::sync::OnceLock<DegreeConsts> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        let mut c = DegreeConsts {
            sin_30: (30.0 * RADIANS_PER_DEGREE).sin(),
            one_minus_cos_60: 1.0 - (60.0 * RADIANS_PER_DEGREE).cos(),
            asin_0_5: 0.5f64.asin(),
            acos_0_5: 0.5f64.acos(),
            atan_1_0: 1.0f64.atan(),
            tan_45: 0.0,
            cot_45: 0.0,
        };
        c.tan_45 = sind_q1(&c, 45.0) / cosd_q1(&c, 45.0);
        c.cot_45 = cosd_q1(&c, 45.0) / sind_q1(&c, 45.0);
        c
    })
}

fn asind_q1(c: &DegreeConsts, x: f64) -> f64 {
    if x <= 0.5 {
        (x.asin() / c.asin_0_5) * 30.0
    } else {
        90.0 - (x.acos() / c.acos_0_5) * 60.0
    }
}
fn acosd_q1(c: &DegreeConsts, x: f64) -> f64 {
    if x <= 0.5 {
        90.0 - (x.asin() / c.asin_0_5) * 30.0
    } else {
        (x.acos() / c.acos_0_5) * 60.0
    }
}
fn sind_0_to_30(c: &DegreeConsts, x: f64) -> f64 {
    ((x * RADIANS_PER_DEGREE).sin() / c.sin_30) / 2.0
}
fn cosd_0_to_60(c: &DegreeConsts, x: f64) -> f64 {
    let one_minus_cos_x = 1.0 - (x * RADIANS_PER_DEGREE).cos();
    1.0 - (one_minus_cos_x / c.one_minus_cos_60) / 2.0
}
fn sind_q1(c: &DegreeConsts, x: f64) -> f64 {
    if x <= 30.0 {
        sind_0_to_30(c, x)
    } else {
        cosd_0_to_60(c, 90.0 - x)
    }
}
fn cosd_q1(c: &DegreeConsts, x: f64) -> f64 {
    if x <= 60.0 {
        cosd_0_to_60(c, x)
    } else {
        sind_0_to_30(c, 90.0 - x)
    }
}

/// The NaN / infinity checks shared by the degree functions.
fn degree_arg(a: &[Value]) -> Result<Option<f64>> {
    let x = float_arg(&a[0]);
    if x.is_nan() {
        return Ok(None);
    }
    if x.is_infinite() {
        return Err(input_out_of_range());
    }
    Ok(Some(x))
}
fn finite_result(r: f64) -> Result<Value> {
    if r.is_infinite() {
        return Err(float_overflow());
    }
    f8(r)
}
fn sind(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(mut x) = degree_arg(a)? else {
        return f8(f64::NAN);
    };
    let c = degree_consts();
    let mut sign = 1.0;
    x %= 360.0;
    if x < 0.0 {
        x = -x;
        sign = -sign;
    }
    if x > 180.0 {
        x = 360.0 - x;
        sign = -sign;
    }
    if x > 90.0 {
        x = 180.0 - x;
    }
    finite_result(sign * sind_q1(c, x))
}
fn cosd(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(mut x) = degree_arg(a)? else {
        return f8(f64::NAN);
    };
    let c = degree_consts();
    let mut sign = 1.0;
    x %= 360.0;
    if x < 0.0 {
        x = -x;
    }
    if x > 180.0 {
        x = 360.0 - x;
    }
    if x > 90.0 {
        x = 180.0 - x;
        sign = -sign;
    }
    finite_result(sign * cosd_q1(c, x))
}
fn tand(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(mut x) = degree_arg(a)? else {
        return f8(f64::NAN);
    };
    let c = degree_consts();
    let mut sign = 1.0;
    x %= 360.0;
    if x < 0.0 {
        x = -x;
        sign = -sign;
    }
    if x > 180.0 {
        x = 360.0 - x;
        sign = -sign;
    }
    if x > 90.0 {
        x = 180.0 - x;
        sign = -sign;
    }
    let tan_x = sind_q1(c, x) / cosd_q1(c, x);
    let r = sign * (tan_x / c.tan_45);
    // No minus zero; no overflow check (tand(90) is Infinity).
    f8(if r == 0.0 { 0.0 } else { r })
}
fn cotd(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(mut x) = degree_arg(a)? else {
        return f8(f64::NAN);
    };
    let c = degree_consts();
    let mut sign = 1.0;
    x %= 360.0;
    if x < 0.0 {
        x = -x;
        sign = -sign;
    }
    if x > 180.0 {
        x = 360.0 - x;
        sign = -sign;
    }
    if x > 90.0 {
        x = 180.0 - x;
        sign = -sign;
    }
    let cot_x = cosd_q1(c, x) / sind_q1(c, x);
    let r = sign * (cot_x / c.cot_45);
    f8(if r == 0.0 { 0.0 } else { r })
}
fn asind(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    if x.is_nan() {
        return f8(f64::NAN);
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(input_out_of_range());
    }
    let c = degree_consts();
    finite_result(if x >= 0.0 {
        asind_q1(c, x)
    } else {
        -asind_q1(c, -x)
    })
}
fn acosd(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    if x.is_nan() {
        return f8(f64::NAN);
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(input_out_of_range());
    }
    let c = degree_consts();
    finite_result(if x >= 0.0 {
        acosd_q1(c, x)
    } else {
        90.0 + asind_q1(c, -x)
    })
}
fn atand(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    if x.is_nan() {
        return f8(f64::NAN);
    }
    let c = degree_consts();
    finite_result((x.atan() / c.atan_1_0) * 45.0)
}

unsafe extern "C" {
    safe fn erf(x: f64) -> f64;
    safe fn erfc(x: f64) -> f64;
    safe fn tgamma(x: f64) -> f64;
    fn lgamma_r(x: f64, sign: *mut i32) -> f64;
}

fn erf_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    f8(erf(float_arg(&a[0])))
}
fn erfc_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    f8(erfc(float_arg(&a[0])))
}
/// dgamma.
fn gamma_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    if x.is_nan() {
        return f8(x);
    }
    if x.is_infinite() {
        // Per POSIX, -Inf is a domain error.
        if x < 0.0 {
            return Err(float_overflow());
        }
        return f8(x);
    }
    let r = tgamma(x);
    if r.is_infinite() || r.is_nan() {
        return Err(float_overflow());
    }
    if r == 0.0 {
        return Err(float_underflow());
    }
    f8(r)
}
/// dlgamma.
fn lgamma_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = float_arg(&a[0]);
    if x.is_nan() {
        return f8(x);
    }
    let mut sign = 0i32;
    // SAFETY: lgamma_r only writes the sign through the valid pointer.
    let r = unsafe { lgamma_r(x, &mut sign) };
    if r.is_infinite() && !x.is_infinite() {
        return Err(float_overflow());
    }
    f8(r)
}
fn float4send(a: &[Value], _: &FnCtx) -> Result<Value> {
    let v = float_arg(&a[0]) as f32;
    Ok(Value::Bytea(v.to_bits().to_be_bytes().to_vec().into()))
}
fn float8send(a: &[Value], _: &FnCtx) -> Result<Value> {
    let v = float_arg(&a[0]);
    Ok(Value::Bytea(v.to_bits().to_be_bytes().to_vec().into()))
}

macro_rules! trig {
    ($($name:ident => $f:expr),* $(,)?) => {
        $(fn $name(a: &[Value], _: &FnCtx) -> Result<Value> {
            let x = float_arg(&a[0]);
            let r: f64 = $f(x);
            if r.is_nan() && !x.is_nan() {
                return Err(err!(NUMERIC_VALUE_OUT_OF_RANGE, "input is out of range"));
            }
            f8(r)
        })*
    };
}

trig! {
    sin => f64::sin, cos => f64::cos, tan => f64::tan, asin => f64::asin, acos => f64::acos, atan => f64::atan,
    cot => |x: f64| 1.0 / x.tan(), sinh => f64::sinh, cosh => f64::cosh, tanh => f64::tanh,
    asinh => f64::asinh, acosh => f64::acosh, atanh => f64::atanh,
}

fn atan2(a: &[Value], _: &FnCtx) -> Result<Value> {
    f8(float_arg(&a[0]).atan2(float_arg(&a[1])))
}
/// datan2d.
fn atan2d(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (y, x) = (float_arg(&a[0]), float_arg(&a[1]));
    if y.is_nan() || x.is_nan() {
        return f8(f64::NAN);
    }
    let c = degree_consts();
    finite_result((y.atan2(x) / c.atan_1_0) * 45.0)
}

fn gcd_i(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (mut x, mut y) = (int_arg(&a[0]) as i128, int_arg(&a[1]) as i128);
    while y != 0 {
        let t = x % y;
        x = y;
        y = t;
    }
    let r = x.abs();
    Ok(match c.ret {
        T::Int4 => Value::Int4(
            i32::try_from(r)
                .map_err(|_| err!(NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"))?,
        ),
        _ => Value::Int8(
            i64::try_from(r)
                .map_err(|_| err!(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))?,
        ),
    })
}
fn lcm_i(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (x, y) = (int_arg(&a[0]) as i128, int_arg(&a[1]) as i128);
    if x == 0 || y == 0 {
        return Ok(match c.ret {
            T::Int4 => Value::Int4(0),
            _ => Value::Int8(0),
        });
    }
    let (mut g, mut h) = (x, y);
    while h != 0 {
        let t = g % h;
        g = h;
        h = t;
    }
    let r = (x / g * y).abs();
    Ok(match c.ret {
        T::Int4 => Value::Int4(
            i32::try_from(r)
                .map_err(|_| err!(NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"))?,
        ),
        _ => Value::Int8(
            i64::try_from(r)
                .map_err(|_| err!(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))?,
        ),
    })
}
fn factorial(a: &[Value], _: &FnCtx) -> Result<Value> {
    let n = int_arg(&a[0]);
    if n < 0 {
        return Err(err!(
            INVALID_PARAMETER_VALUE,
            "factorial of a negative number is undefined"
        ));
    }
    if n > 32177 {
        return Err(err!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "value overflows numeric format"
        ));
    }
    let mut r = BigInt::one();
    for i in 2..=n {
        r *= i;
    }
    Ok(Value::numeric(Numeric::from_parts(r, 0)))
}
fn scale_fn(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = num(&a[0]);
    if !x.is_finite() {
        return Ok(Value::Null);
    }
    Ok(Value::Int4(x.scale() as i32))
}
fn min_scale(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = num(&a[0]);
    if !x.is_finite() {
        return Ok(Value::Null);
    }
    Ok(Value::Int4(x.min_scale() as i32))
}
fn trim_scale(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).normalize()))
}
fn width_bucket_count(n: i64) -> Result<i64> {
    if n <= 0 {
        return Err(err!(
            INVALID_ARGUMENT_FOR_WIDTH_BUCKET,
            "count must be greater than zero"
        ));
    }
    Ok(n)
}
fn bucket_plus_one(n: i64) -> Result<i64> {
    if n >= i32::MAX as i64 {
        return Err(err!(NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"));
    }
    Ok(n + 1)
}
/// width_bucket_float8.
fn width_bucket_f(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, lo, hi) = (float_arg(&a[0]), float_arg(&a[1]), float_arg(&a[2]));
    let n = width_bucket_count(int_arg(&a[3]))?;
    if x.is_nan() || lo.is_nan() || hi.is_nan() {
        return Err(err!(
            INVALID_ARGUMENT_FOR_WIDTH_BUCKET,
            "operand, lower bound, and upper bound cannot be NaN"
        ));
    }
    if lo.is_infinite() || hi.is_infinite() {
        return Err(err!(
            INVALID_ARGUMENT_FOR_WIDTH_BUCKET,
            "lower and upper bounds must be finite"
        ));
    }
    // count * quotient, truncated to int32 as C does; the quotient is in [0, 1].
    let bucket = |q: f64| -> i64 {
        let r = (n as f64 * q) as i32 as i64;
        r.min(n - 1) + 1
    };
    let b = if lo < hi {
        if x < lo {
            0
        } else if x >= hi {
            bucket_plus_one(n)?
        } else if !(hi - lo).is_infinite() {
            bucket((x - lo) / (hi - lo))
        } else {
            bucket((x / 2.0 - lo / 2.0) / (hi / 2.0 - lo / 2.0))
        }
    } else if lo > hi {
        if x > lo {
            0
        } else if x <= hi {
            bucket_plus_one(n)?
        } else if !(lo - hi).is_infinite() {
            bucket((lo - x) / (lo - hi))
        } else {
            bucket((lo / 2.0 - x / 2.0) / (lo / 2.0 - hi / 2.0))
        }
    } else {
        return Err(err!(
            INVALID_ARGUMENT_FOR_WIDTH_BUCKET,
            "lower bound cannot equal upper bound"
        ));
    };
    Ok(Value::Int4(b as i32))
}
/// width_bucket_numeric: ((operand - bound1) * count) / (bound2 - bound1) + 1, truncating.
fn width_bucket_n(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, lo, hi) = (num(&a[0]), num(&a[1]), num(&a[2]));
    let n = width_bucket_count(int_arg(&a[3]))?;
    if x.is_nan() || lo.is_nan() || hi.is_nan() {
        return Err(err!(
            INVALID_ARGUMENT_FOR_WIDTH_BUCKET,
            "operand, lower bound, and upper bound cannot be NaN"
        ));
    }
    if !lo.is_finite() || !hi.is_finite() {
        return Err(err!(
            INVALID_ARGUMENT_FOR_WIDTH_BUCKET,
            "lower and upper bounds must be finite"
        ));
    }
    let count = Numeric::from_i64(n);
    let compute = || -> Result<Numeric> {
        let q = x.sub(&lo).mul(&count).div_trunc(&hi.sub(&lo))?;
        Ok(q.add(&Numeric::from_i64(1)))
    };
    let r = match lo.cmp(&hi) {
        std::cmp::Ordering::Equal => {
            return Err(err!(
                INVALID_ARGUMENT_FOR_WIDTH_BUCKET,
                "lower bound cannot equal upper bound"
            ));
        }
        std::cmp::Ordering::Less => {
            if x < lo {
                Numeric::zero()
            } else if x >= hi {
                count.add(&Numeric::from_i64(1))
            } else {
                compute()?
            }
        }
        std::cmp::Ordering::Greater => {
            if x > lo {
                Numeric::zero()
            } else if x <= hi {
                count.add(&Numeric::from_i64(1))
            } else {
                compute()?
            }
        }
    };
    r.to_i64_round()
        .and_then(|v| i32::try_from(v).ok())
        .map(Value::Int4)
        .ok_or_else(|| err!(NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"))
}
fn numeric_inc(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::numeric(num(&a[0]).inc()?))
}
fn setseed(a: &[Value], c: &FnCtx) -> Result<Value> {
    let s = float_arg(&a[0]);
    if !(-1.0..=1.0).contains(&s) {
        return Err(err!(
            INVALID_PARAMETER_VALUE,
            "setseed parameter {s} is out of allowed range [-1,1]"
        ));
    }
    c.exec.set_seed(s);
    Ok(Value::Null)
}

pub fn register(r: &mut Registry) {
    use T::*;
    let num = T::NUMERIC;
    // Aggregate support functions (count's, min/max's, avg's and the variances').
    r.func("int8inc", vec![Int8], Int8, int8inc);
    r.func("int8dec", vec![Int8], Int8, int8dec);
    r.func("int8inc_any", vec![Int8, Any], Int8, int8inc);
    r.func("int8dec_any", vec![Int8, Any], Int8, int8dec);
    r.func("booland_statefunc", vec![Bool, Bool], Bool, |a, _| {
        Ok(Value::Bool(a[0].as_bool() == Some(true) && a[1].as_bool() == Some(true)))
    });
    r.func("boolor_statefunc", vec![Bool, Bool], Bool, |a, _| {
        Ok(Value::Bool(a[0].as_bool() == Some(true) || a[1].as_bool() == Some(true)))
    });
    r.func("int4_sum", vec![Int8, Int4], Int8, int_sum)
        .non_strict();
    r.func("int2_sum", vec![Int8, Int2], Int8, int_sum)
        .non_strict();
    for (larger, smaller, t) in [
        ("int2larger", "int2smaller", Int2),
        ("int4larger", "int4smaller", Int4),
        ("int8larger", "int8smaller", Int8),
        ("float4larger", "float4smaller", Float4),
        ("float8larger", "float8smaller", Float8),
        ("numeric_larger", "numeric_smaller", num.clone()),
        ("text_larger", "text_smaller", Text),
        ("bpchar_larger", "bpchar_smaller", Bpchar(-1)),
        ("date_larger", "date_smaller", Date),
        ("time_larger", "time_smaller", Time(-1)),
        ("timestamp_larger", "timestamp_smaller", Timestamp(-1)),
        ("timestamptz_larger", "timestamptz_smaller", Timestamptz(-1)),
        ("interval_larger", "interval_smaller", Interval(-1)),
        ("cashlarger", "cashsmaller", Money),
    ] {
        r.func(larger, vec![t.clone(), t.clone()], t.clone(), larger_of);
        r.func(smaller, vec![t.clone(), t.clone()], t, smaller_of);
    }
    // avg(int2/int4)'s {count, sum} int8[] state.
    let i8s = T::array_of(Int8);
    fn avg_accum(a: &[Value], sign: i64) -> Result<Value> {
        let [n, sum] = int8_avg_state(&a[0])?;
        let v = a[1].as_i64().unwrap_or(0);
        Ok(int8_avg_value(
            n.wrapping_add(sign),
            sum.wrapping_add(v.wrapping_mul(sign)),
        ))
    }
    for (name, t) in [("int2_avg_accum", Int2), ("int4_avg_accum", Int4)] {
        r.func(name, vec![i8s.clone(), t], i8s.clone(), |a, _| avg_accum(a, 1));
    }
    for (name, t) in [("int2_avg_accum_inv", Int2), ("int4_avg_accum_inv", Int4)] {
        r.func(name, vec![i8s.clone(), t], i8s.clone(), |a, _| avg_accum(a, -1));
    }
    r.func("int4_avg_combine", vec![i8s.clone(), i8s.clone()], i8s.clone(), |a, _| {
        let [n1, s1] = int8_avg_state(&a[0])?;
        let [n2, s2] = int8_avg_state(&a[1])?;
        Ok(int8_avg_value(n1.wrapping_add(n2), s1.wrapping_add(s2)))
    });
    r.func("int8_avg", vec![i8s], num.clone(), |a, _| {
        let [n, sum] = int8_avg_state(&a[0])?;
        if n == 0 {
            return Ok(Value::Null);
        }
        Ok(Value::numeric(
            crate::types::numeric::Numeric::from_i64(sum)
                .div(&crate::types::numeric::Numeric::from_i64(n))?,
        ))
    });
    let f8s = T::array_of(Float8);
    r.func(
        "float8_accum",
        vec![f8s.clone(), Float8],
        f8s.clone(),
        float8_accum,
    );
    r.func(
        "float4_accum",
        vec![f8s.clone(), Float4],
        f8s.clone(),
        float8_accum,
    );
    r.func(
        "float8_regr_accum",
        vec![f8s.clone(), Float8, Float8],
        f8s.clone(),
        float8_regr_accum,
    );
    r.func(
        "float8_combine",
        vec![f8s.clone(), f8s.clone()],
        f8s.clone(),
        float8_combine,
    );
    r.func(
        "float8_regr_combine",
        vec![f8s.clone(), f8s.clone()],
        f8s.clone(),
        float8_regr_combine,
    );
    r.func("float8_avg", vec![f8s.clone()], Float8, |a, _| {
        float8_moment(a, 'a')
    });
    r.func("float8_var_pop", vec![f8s.clone()], Float8, |a, _| {
        float8_moment(a, 'p')
    });
    r.func("float8_var_samp", vec![f8s.clone()], Float8, |a, _| {
        float8_moment(a, 's')
    });
    r.func("float8_stddev_pop", vec![f8s.clone()], Float8, |a, _| {
        float8_moment(a, 'P')
    });
    r.func("float8_stddev_samp", vec![f8s], Float8, |a, _| {
        float8_moment(a, 'S')
    });
    for t in [Int2, Int4, Int8] {
        r.func("abs", vec![t.clone()], t.clone(), abs_i);
        r.func("mod", vec![t.clone(), t.clone()], t.clone(), mod_i);
    }
    r.func("abs", vec![Float4], Float4, abs_f);
    r.func("abs", vec![Float8], Float8, abs_f);
    r.func("abs", vec![num.clone()], num.clone(), abs_num);
    r.func("round", vec![Float8], Float8, round_f);
    r.func("round", vec![num.clone()], num.clone(), round_num);
    r.func("round", vec![num.clone(), Int4], num.clone(), round_num);
    r.func("trunc", vec![Float8], Float8, trunc_f);
    r.func("trunc", vec![num.clone()], num.clone(), trunc_num);
    r.func("trunc", vec![num.clone(), Int4], num.clone(), trunc_num);
    for name in ["ceil", "ceiling"] {
        r.func(name, vec![Float8], Float8, ceil_f);
        r.func(name, vec![num.clone()], num.clone(), ceil_num);
    }
    r.func("floor", vec![Float8], Float8, floor_f);
    r.func("floor", vec![num.clone()], num.clone(), floor_num);
    r.func("sign", vec![Float8], Float8, sign_f);
    r.func("sign", vec![num.clone()], num.clone(), sign_num);
    r.func("sqrt", vec![Float8], Float8, sqrt_f);
    r.func("sqrt", vec![num.clone()], num.clone(), sqrt_num);
    r.func("cbrt", vec![Float8], Float8, cbrt_f);
    r.func("exp", vec![Float8], Float8, exp_f);
    r.func("exp", vec![num.clone()], num.clone(), exp_num);
    r.func("ln", vec![Float8], Float8, ln_f);
    r.func("ln", vec![num.clone()], num.clone(), ln_num);
    r.func("log", vec![Float8], Float8, log10_f);
    r.func("log", vec![num.clone()], num.clone(), log_num1);
    r.func("log10", vec![Float8], Float8, log10_f);
    r.func("log10", vec![num.clone()], num.clone(), log10_num);
    r.func("log", vec![num.clone(), num.clone()], num.clone(), log_num2);
    for name in ["power", "pow"] {
        r.func(name, vec![Float8, Float8], Float8, power_f);
        r.func(name, vec![num.clone(), num.clone()], num.clone(), power_num);
    }
    r.func("mod", vec![num.clone(), num.clone()], num.clone(), mod_num);
    r.func("div", vec![num.clone(), num.clone()], num.clone(), div_num);
    r.func("pi", vec![], Float8, pi);
    r.func("random", vec![], Float8, random).volatile();
    r.func("random", vec![Int4, Int4], Int4, random_range_i)
        .volatile()
        .names(&["min", "max"]);
    r.func("random", vec![Int8, Int8], Int8, random_range_i)
        .volatile()
        .names(&["min", "max"]);
    r.from_pg(6341, Some(random_range_num), None)
        .names(&["min", "max"]);
    r.from_pg(6212, Some(random_normal), None)
        .names(&["mean", "stddev"])
        .defaults(vec![Value::Float8(0.0), Value::Float8(1.0)]);
    r.func("setseed", vec![Float8], Void, setseed).volatile();
    r.func("degrees", vec![Float8], Float8, degrees);
    r.func("radians", vec![Float8], Float8, radians);
    let trigs: [(&'static str, super::FnImpl); 24] = [
        ("sin", sin),
        ("cos", cos),
        ("tan", tan),
        ("asin", asin),
        ("acos", acos),
        ("atan", atan),
        ("sind", sind),
        ("cosd", cosd),
        ("tand", tand),
        ("cot", cot),
        ("sinh", sinh),
        ("cosh", cosh),
        ("tanh", tanh),
        ("asinh", asinh),
        ("acosh", acosh),
        ("atanh", atanh),
        ("asind", asind),
        ("acosd", acosd),
        ("atand", atand),
        ("cotd", cotd),
        ("erf", erf_f),
        ("erfc", erfc_f),
        ("gamma", gamma_f),
        ("lgamma", lgamma_f),
    ];
    for (name, f) in trigs {
        r.func(name, vec![Float8], Float8, f);
    }
    r.func("atan2", vec![Float8, Float8], Float8, atan2);
    r.func("atan2d", vec![Float8, Float8], Float8, atan2d);
    r.func("float4send", vec![Float4], Bytea, float4send);
    r.func("float8send", vec![Float8], Bytea, float8send);
    r.func("gcd", vec![Int4, Int4], Int4, gcd_i);
    r.func("gcd", vec![Int8, Int8], Int8, gcd_i);
    r.func("lcm", vec![Int4, Int4], Int4, lcm_i);
    r.func("lcm", vec![Int8, Int8], Int8, lcm_i);
    r.func("gcd", vec![num.clone(), num.clone()], num.clone(), gcd_num);
    r.func("lcm", vec![num.clone(), num.clone()], num.clone(), lcm_num);
    r.func("factorial", vec![Int8], num.clone(), factorial);
    r.func("scale", vec![num.clone()], Int4, scale_fn);
    r.func("min_scale", vec![num.clone()], Int4, min_scale);
    r.func("trim_scale", vec![num.clone()], num.clone(), trim_scale);
    r.func("numeric_inc", vec![num.clone()], num.clone(), numeric_inc);
    r.func(
        "width_bucket",
        vec![Float8, Float8, Float8, Int4],
        Int4,
        width_bucket_f,
    );
    r.func(
        "width_bucket",
        vec![num.clone(), num.clone(), num.clone(), Int4],
        Int4,
        width_bucket_n,
    );
    let _ = Ret::Poly;
}

fn int8inc(a: &[Value], _: &FnCtx) -> Result<Value> {
    let v = a[0].as_i64().unwrap_or(0);
    Ok(Value::Int8(v.checked_add(1).ok_or_else(|| {
        err!(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")
    })?))
}

fn int8dec(a: &[Value], _: &FnCtx) -> Result<Value> {
    let v = a[0].as_i64().unwrap_or(0);
    Ok(Value::Int8(v.checked_sub(1).ok_or_else(|| {
        err!(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")
    })?))
}

/// int4_sum(state, value): NULL state starts the sum; a NULL value leaves it.
fn int_sum(a: &[Value], _: &FnCtx) -> Result<Value> {
    match (&a[0], a[1].as_i64()) {
        (Value::Null, None) => Ok(Value::Null),
        (Value::Null, Some(v)) => Ok(Value::Int8(v)),
        (s, None) => Ok(s.clone()),
        (s, Some(v)) => Ok(Value::Int8(
            s.as_i64()
                .unwrap_or(0)
                .checked_add(v)
                .ok_or_else(|| err!(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))?,
        )),
    }
}

/// text_larger / text_smaller & co. under a collation: the first if it sorts after (before)
/// the second, else the second.
fn collated_pick(
    a: &[Value],
    c: &FnCtx,
    coll: &crate::types::collation::Collation,
    larger: bool,
) -> Value {
    let bp = matches!(c.arg_type(0), T::Bpchar(_));
    let trim = |v: &Value| -> String {
        let s = super::text_arg(v);
        if bp { s.trim_end_matches(' ') } else { s }.to_string()
    };
    let o = coll.cmp(&trim(&a[0]), &trim(&a[1]));
    let first = if larger {
        o == std::cmp::Ordering::Greater
    } else {
        o == std::cmp::Ordering::Less
    };
    if first { a[0].clone() } else { a[1].clone() }
}

fn larger_of(a: &[Value], c: &FnCtx) -> Result<Value> {
    if let Some(coll) = c.coll
        && matches!(a[0], Value::Text(_))
    {
        return Ok(collated_pick(a, c, coll, true));
    }
    Ok(if a[0].cmp_total(&a[1]) == std::cmp::Ordering::Less {
        a[1].clone()
    } else {
        a[0].clone()
    })
}

fn smaller_of(a: &[Value], c: &FnCtx) -> Result<Value> {
    if let Some(coll) = c.coll
        && matches!(a[0], Value::Text(_))
    {
        return Ok(collated_pick(a, c, coll, false));
    }
    Ok(if a[1].cmp_total(&a[0]) == std::cmp::Ordering::Less {
        a[1].clone()
    } else {
        a[0].clone()
    })
}

/// The {count, sum} state of int2_avg_accum / int4_avg_accum.
fn int8_avg_state(v: &Value) -> Result<[i64; 2]> {
    let items = v
        .as_array()
        .map(|a| a.items.iter().map(|x| x.as_i64()).collect::<Vec<_>>())
        .unwrap_or_default();
    match items.as_slice() {
        [Some(n), Some(s)] => Ok([*n, *s]),
        _ => Err(err!(INTERNAL_ERROR, "expected 2-element int8 array")),
    }
}

fn int8_avg_value(n: i64, sum: i64) -> Value {
    Value::Array(std::sync::Arc::new(
        crate::types::array::ArrayValue::from_vec(T::Int8, vec![Value::Int8(n), Value::Int8(sum)]),
    ))
}

/// The {N, Sx, Sxx} state of float8_accum.
fn float8_state(v: &Value, fname: &str) -> Result<[f64; 3]> {
    let items = v
        .as_array()
        .map(|a| a.items.iter().map(|x| x.as_f64()).collect::<Vec<_>>())
        .unwrap_or_default();
    match items.as_slice() {
        [Some(n), Some(sx), Some(sxx)] => Ok([*n, *sx, *sxx]),
        _ => Err(err!(
            INTERNAL_ERROR,
            "{fname}: expected 3-element float8 array"
        )),
    }
}

/// check_float8_array: the `n` values of a float8[] transition state.
fn float8_array(v: &Value, fname: &str, n: usize) -> Result<Vec<f64>> {
    let items = v
        .as_array()
        .map(|a| a.items.iter().map(|x| x.as_f64()).collect::<Vec<_>>())
        .unwrap_or_default();
    if items.len() != n || items.iter().any(|x| x.is_none()) {
        return Err(err!(
            INTERNAL_ERROR,
            "{fname}: expected {n}-element float8 array"
        ));
    }
    Ok(items.into_iter().map(|x| x.unwrap_or(0.0)).collect())
}

fn float8_array_value(vals: &[f64]) -> Value {
    Value::Array(std::sync::Arc::new(
        crate::types::array::ArrayValue::from_vec(
            T::Float8,
            vals.iter().copied().map(Value::Float8).collect(),
        ),
    ))
}

/// float8_pl.
fn float8_pl(a: f64, b: f64) -> Result<f64> {
    let r = a + b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(float_overflow());
    }
    Ok(r)
}

/// float8_regr_accum: Youngs-Cramer over (N, Sx, Sxx, Sy, Syy, Sxy).
pub fn float8_regr_accum_state(t: &mut [f64; 6], y: f64, x: f64) -> Result<()> {
    let [n0, sx0, _, sy0, _, _] = *t;
    let [mut n, mut sx, mut sxx, mut sy, mut syy, mut sxy] = *t;
    n += 1.0;
    sx += x;
    sy += y;
    if n0 > 0.0 {
        let tmp_x = x * n - sx;
        let tmp_y = y * n - sy;
        let scale = 1.0 / (n * n0);
        sxx += tmp_x * tmp_x * scale;
        syy += tmp_y * tmp_y * scale;
        sxy += tmp_x * tmp_y * scale;
        if sx.is_infinite()
            || sxx.is_infinite()
            || sy.is_infinite()
            || syy.is_infinite()
            || sxy.is_infinite()
        {
            if ((sx.is_infinite() || sxx.is_infinite()) && !sx0.is_infinite() && !x.is_infinite())
                || ((sy.is_infinite() || syy.is_infinite())
                    && !sy0.is_infinite()
                    && !y.is_infinite())
                || (sxy.is_infinite()
                    && !sx0.is_infinite()
                    && !x.is_infinite()
                    && !sy0.is_infinite()
                    && !y.is_infinite())
            {
                return Err(float_overflow());
            }
            if sxx.is_infinite() {
                sxx = f64::NAN;
            }
            if syy.is_infinite() {
                syy = f64::NAN;
            }
            if sxy.is_infinite() {
                sxy = f64::NAN;
            }
        }
    } else {
        // A first input of Inf or NaN makes the dependent sums NaN.
        if x.is_nan() || x.is_infinite() {
            sxx = f64::NAN;
            sxy = f64::NAN;
        }
        if y.is_nan() || y.is_infinite() {
            syy = f64::NAN;
            sxy = f64::NAN;
        }
    }
    *t = [n, sx, sxx, sy, syy, sxy];
    Ok(())
}

fn float8_regr_accum(a: &[Value], _: &FnCtx) -> Result<Value> {
    let v = float8_array(&a[0], "float8_regr_accum", 6)?;
    let mut t = [v[0], v[1], v[2], v[3], v[4], v[5]];
    float8_regr_accum_state(&mut t, float_arg(&a[1]), float_arg(&a[2]))?;
    Ok(float8_array_value(&t))
}

/// float8_combine: merge two (N, Sx, Sxx) states.
fn float8_combine(a: &[Value], _: &FnCtx) -> Result<Value> {
    let t1 = float8_array(&a[0], "float8_combine", 3)?;
    let t2 = float8_array(&a[1], "float8_combine", 3)?;
    let (n1, sx1, sxx1) = (t1[0], t1[1], t1[2]);
    let (n2, sx2, sxx2) = (t2[0], t2[1], t2[2]);
    let out = if n1 == 0.0 {
        [n2, sx2, sxx2]
    } else if n2 == 0.0 {
        [n1, sx1, sxx1]
    } else {
        let n = n1 + n2;
        let sx = float8_pl(sx1, sx2)?;
        let tmp = sx1 / n1 - sx2 / n2;
        let sxx = sxx1 + sxx2 + n1 * n2 * tmp * tmp / n;
        if sxx.is_infinite() && !sxx1.is_infinite() && !sxx2.is_infinite() {
            return Err(float_overflow());
        }
        [n, sx, sxx]
    };
    Ok(float8_array_value(&out))
}

/// float8_regr_combine: merge two (N, Sx, Sxx, Sy, Syy, Sxy) states.
fn float8_regr_combine(a: &[Value], _: &FnCtx) -> Result<Value> {
    let t1 = float8_array(&a[0], "float8_regr_combine", 6)?;
    let t2 = float8_array(&a[1], "float8_regr_combine", 6)?;
    let (n1, n2) = (t1[0], t2[0]);
    let out: Vec<f64> = if n1 == 0.0 {
        t2
    } else if n2 == 0.0 {
        t1
    } else {
        let n = n1 + n2;
        let sx = float8_pl(t1[1], t2[1])?;
        let tmp1 = t1[1] / n1 - t2[1] / n2;
        let sxx = t1[2] + t2[2] + n1 * n2 * tmp1 * tmp1 / n;
        if sxx.is_infinite() && !t1[2].is_infinite() && !t2[2].is_infinite() {
            return Err(float_overflow());
        }
        let sy = float8_pl(t1[3], t2[3])?;
        let tmp2 = t1[3] / n1 - t2[3] / n2;
        let syy = t1[4] + t2[4] + n1 * n2 * tmp2 * tmp2 / n;
        if syy.is_infinite() && !t1[4].is_infinite() && !t2[4].is_infinite() {
            return Err(float_overflow());
        }
        let sxy = t1[5] + t2[5] + n1 * n2 * tmp1 * tmp2 / n;
        if sxy.is_infinite() && !t1[5].is_infinite() && !t2[5].is_infinite() {
            return Err(float_overflow());
        }
        vec![n, sx, sxx, sy, syy, sxy]
    };
    Ok(float8_array_value(&out))
}

/// Youngs-Cramer: N, the sum, and the sum of squared differences from the mean.
fn float8_accum(a: &[Value], _: &FnCtx) -> Result<Value> {
    let [mut n, mut sx, mut sxx] = float8_state(&a[0], "float8_accum")?;
    let x = a[1].as_f64().unwrap_or(0.0);
    n += 1.0;
    sx += x;
    if n > 1.0 {
        let tmp = x * n - sx;
        sxx += tmp * tmp / (n * (n - 1.0));
        if sx.is_infinite() || sxx.is_infinite() {
            if !x.is_infinite() && !(sx - x).is_infinite() {
                return Err(err!(
                    NUMERIC_VALUE_OUT_OF_RANGE,
                    "value out of range: overflow"
                ));
            }
            sxx = f64::NAN;
        }
    } else if x.is_nan() || x.is_infinite() {
        sxx = f64::NAN;
    } else {
        sxx = 0.0;
    }
    let items = [n, sx, sxx].into_iter().map(Value::Float8).collect();
    Ok(Value::Array(std::sync::Arc::new(
        crate::types::array::ArrayValue::from_vec(T::Float8, items),
    )))
}

/// float8_avg / var / stddev over a float8_accum state.
fn float8_moment(a: &[Value], which: char) -> Result<Value> {
    let [n, sx, sxx] = float8_state(&a[0], "float8_avg")?;
    let v = match which {
        'a' if n == 0.0 => return Ok(Value::Null),
        'a' => sx / n,
        'p' | 'P' if n == 0.0 => return Ok(Value::Null),
        'p' | 'P' => sxx / n,
        _ if n <= 1.0 => return Ok(Value::Null),
        _ => sxx / (n - 1.0),
    };
    Ok(Value::Float8(if which.is_uppercase() {
        v.sqrt()
    } else {
        v
    }))
}
