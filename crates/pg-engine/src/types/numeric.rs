//! Arbitrary-precision `numeric` with Postgres's scale rules, rounding (half away from zero),
//! NaN and ±Infinity.

use crate::error::{Error, Result, code};
use crate::{bail, err};
use super::decint::{DecInt, Sign};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

pub const MAX_DISPLAY_SCALE: i64 = 1000;
const MIN_DISPLAY_SCALE: i64 = 0;
const MIN_SIG_DIGITS: i64 = 16;
/// NUMERIC_MAX_RESULT_SCALE.
const MAX_RESULT_SCALE: i64 = 2000;
/// The int16 weight field of the on-disk format, in base-10000 digits.
const WEIGHT_MAX: i64 = 32767;
const WEIGHT_MIN: i64 = -32768;
/// NUMERIC_DSCALE_MAX.
pub const DSCALE_MAX: i64 = 0x3FFF;
// numeric.c's literal constants, kept exactly so the truncations to int match.
#[allow(clippy::approx_constant)]
const PG_LOG10_E: f64 = 0.434294481903252;
#[allow(clippy::approx_constant)]
const PG_LN_10: f64 = 2.302585092994046;
#[allow(clippy::approx_constant)]
const PG_LOG10_2: f64 = 0.301029995663981;

#[derive(Clone, Debug)]
pub enum Numeric {
    /// `int * 10^-scale`, scale >= 0 (the display scale).
    Finite {
        int: DecInt,
        scale: u32,
    },
    NaN,
    PosInf,
    NegInf,
}

/// 10^n as an i128 (n <= 38).
fn pow10_i128(n: u32) -> Option<i128> {
    10i128.checked_pow(n)
}

/// Two small values (each fitting an i64) at their common scale, as i128s: the fast path
/// of add / sub / cmp, exactly what `aligned` computes, without allocating.
fn aligned_small(a: &DecInt, sa: u32, b: &DecInt, sb: u32) -> Option<(i128, i128, u32)> {
    let (x, y) = (a.to_i64()? as i128, b.to_i64()? as i128);
    let s = sa.max(sb);
    Some((x.checked_mul(pow10_i128(s - sa)?)?, y.checked_mul(pow10_i128(s - sb)?)?, s))
}

/// Divide rounding half away from zero.
fn div_round(n: &DecInt, d: &DecInt) -> DecInt {
    if let (Some(x), Some(y)) = (n.to_i128(), d.to_i128()) {
        return DecInt::from(div_round_i128(x, y));
    }
    let (q, r) = n.div_rem(d);
    if r.is_zero() {
        return q;
    }
    let twice = r.abs() * 2u32;
    if twice >= d.abs() {
        if (n.sign() == Sign::Minus) != (d.sign() == Sign::Minus) {
            q - 1
        } else {
            q + 1
        }
    } else {
        q
    }
}

/// `div_round` of i128s (d != 0; |n| < 2^127).
fn div_round_i128(n: i128, d: i128) -> i128 {
    let (q, r) = (n / d, n % d);
    if r != 0 && r.unsigned_abs() >= d.unsigned_abs() - r.unsigned_abs() {
        if (n < 0) != (d < 0) { q - 1 } else { q + 1 }
    } else {
        q
    }
}

impl Numeric {
    pub fn zero() -> Self {
        Numeric::Finite {
            int: DecInt::zero(),
            scale: 0,
        }
    }

    pub fn from_i64(v: i64) -> Self {
        Numeric::Finite {
            int: DecInt::from(v),
            scale: 0,
        }
    }

    pub fn from_i128(v: i128) -> Self {
        Numeric::Finite {
            int: DecInt::from(v),
            scale: 0,
        }
    }

    pub fn from_parts(int: DecInt, scale: u32) -> Self {
        Numeric::Finite { int, scale }
    }

    pub fn is_nan(&self) -> bool {
        matches!(self, Numeric::NaN)
    }

    pub fn is_finite(&self) -> bool {
        matches!(self, Numeric::Finite { .. })
    }

    pub fn is_zero(&self) -> bool {
        matches!(self, Numeric::Finite { int, .. } if int.is_zero())
    }

    pub fn scale(&self) -> u32 {
        match self {
            Numeric::Finite { scale, .. } => *scale,
            _ => 0,
        }
    }

    pub fn is_negative(&self) -> bool {
        match self {
            Numeric::Finite { int, .. } => int.is_negative(),
            Numeric::NegInf => true,
            _ => false,
        }
    }

    /// Same value at a different display scale (rounding when reducing).
    pub fn with_scale(&self, new_scale: u32) -> Numeric {
        match self {
            Numeric::Finite { int, scale } => {
                let int = if new_scale >= *scale {
                    int.mul_pow10((new_scale - scale) as usize)
                } else {
                    int.round_pow10((scale - new_scale) as usize)
                };
                Numeric::Finite {
                    int,
                    scale: new_scale,
                }
            }
            n => n.clone(),
        }
    }

    /// Round to `digits` places; negative rounds to the left of the point (`round(1234, -2)`).
    /// (numeric_round's clamping; the result may still need `checked`.)
    pub fn round(&self, digits: i64) -> Numeric {
        match self {
            Numeric::Finite { int, scale } => {
                let digits = digits.clamp(-(WEIGHT_MAX + 1) * 4 - 1, DSCALE_MAX);
                if digits >= *scale as i64 {
                    return self.with_scale(digits as u32);
                }
                let drop = (*scale as i64 - digits) as u32;
                let r = if drop as usize > int.num_digits() {
                    // Fewer digits than the drop: rounds to zero.
                    DecInt::zero()
                } else {
                    int.round_pow10(drop as usize)
                };
                if digits >= 0 {
                    Numeric::Finite {
                        int: r,
                        scale: digits as u32,
                    }
                } else if r.is_zero() {
                    Numeric::zero()
                } else {
                    Numeric::Finite {
                        int: r.mul_pow10((-digits) as usize),
                        scale: 0,
                    }
                }
            }
            n => n.clone(),
        }
    }

    pub fn trunc(&self, digits: i64) -> Numeric {
        match self {
            Numeric::Finite { int, scale } => {
                let digits = digits.clamp(-(WEIGHT_MAX + 1) * 4, DSCALE_MAX);
                if digits >= *scale as i64 {
                    return self.with_scale(digits as u32);
                }
                let drop = (*scale as i64 - digits) as u32;
                let r = if drop as usize > int.num_digits() {
                    DecInt::zero()
                } else {
                    int.div_pow10(drop as usize)
                };
                if digits >= 0 {
                    Numeric::Finite {
                        int: r,
                        scale: digits as u32,
                    }
                } else if r.is_zero() {
                    Numeric::zero()
                } else {
                    Numeric::Finite {
                        int: r.mul_pow10((-digits) as usize),
                        scale: 0,
                    }
                }
            }
            n => n.clone(),
        }
    }

    pub fn floor(&self) -> Numeric {
        match self {
            Numeric::Finite { int, scale } => {
                let mut q = int.div_pow10(*scale as usize);
                if int.is_negative() && !int.divisible_pow10(*scale as usize) {
                    q -= 1;
                }
                Numeric::Finite { int: q, scale: 0 }
            }
            n => n.clone(),
        }
    }

    pub fn ceil(&self) -> Numeric {
        match self {
            Numeric::Finite { int, scale } => {
                let mut q = int.div_pow10(*scale as usize);
                if int.is_positive() && !int.divisible_pow10(*scale as usize) {
                    q += 1;
                }
                Numeric::Finite { int: q, scale: 0 }
            }
            n => n.clone(),
        }
    }

    pub fn abs(&self) -> Numeric {
        match self {
            Numeric::Finite { int, scale } => Numeric::Finite {
                int: int.abs(),
                scale: *scale,
            },
            Numeric::NegInf => Numeric::PosInf,
            n => n.clone(),
        }
    }

    pub fn neg(&self) -> Numeric {
        match self {
            Numeric::Finite { int, scale } => Numeric::Finite {
                int: -int,
                scale: *scale,
            },
            Numeric::PosInf => Numeric::NegInf,
            Numeric::NegInf => Numeric::PosInf,
            Numeric::NaN => Numeric::NaN,
        }
    }

    pub fn sign(&self) -> Numeric {
        match self {
            Numeric::Finite { int, .. } => Numeric::from_i64(match int.sign() {
                Sign::Minus => -1,
                Sign::NoSign => 0,
                Sign::Plus => 1,
            }),
            Numeric::PosInf => Numeric::from_i64(1),
            Numeric::NegInf => Numeric::from_i64(-1),
            Numeric::NaN => Numeric::NaN,
        }
    }

    /// Strip trailing fractional zeros (`trim_scale`).
    pub fn normalize(&self) -> Numeric {
        match self {
            Numeric::Finite { int, scale } => {
                if int.is_zero() {
                    return Numeric::zero();
                }
                let drop = int.trailing_zero_digits().min(*scale as usize);
                Numeric::Finite { int: int.div_pow10(drop), scale: scale - drop as u32 }
            }
            n => n.clone(),
        }
    }

    /// Minimum scale needed to represent the value exactly (`min_scale`).
    pub fn min_scale(&self) -> u32 {
        self.normalize().scale()
    }

    fn aligned(a: &DecInt, sa: u32, b: &DecInt, sb: u32) -> (DecInt, DecInt, u32) {
        match sa.cmp(&sb) {
            Ordering::Equal => (a.clone(), b.clone(), sa),
            Ordering::Less => (a.mul_pow10((sb - sa) as usize), b.clone(), sb),
            Ordering::Greater => (a.clone(), b.mul_pow10((sa - sb) as usize), sa),
        }
    }

    pub fn add(&self, other: &Numeric) -> Numeric {
        use Numeric::*;
        match (self, other) {
            (NaN, _) | (_, NaN) => NaN,
            (PosInf, NegInf) | (NegInf, PosInf) => NaN,
            (PosInf, _) | (_, PosInf) => PosInf,
            (NegInf, _) | (_, NegInf) => NegInf,
            (Finite { int: a, scale: sa }, Finite { int: b, scale: sb }) => {
                if let Some((x, y, s)) = aligned_small(a, *sa, b, *sb)
                    && let Some(r) = x.checked_add(y)
                {
                    return Finite { int: DecInt::from(r), scale: s };
                }
                let (a, b, s) = Self::aligned(a, *sa, b, *sb);
                Finite {
                    int: a + b,
                    scale: s,
                }
            }
        }
    }

    pub fn sub(&self, other: &Numeric) -> Numeric {
        if let (Numeric::Finite { int: a, scale: sa }, Numeric::Finite { int: b, scale: sb }) = (self, other)
            && let Some((x, y, s)) = aligned_small(a, *sa, b, *sb)
            && let Some(r) = x.checked_sub(y)
        {
            return Numeric::Finite { int: DecInt::from(r), scale: s };
        }
        self.add(&other.neg())
    }

    pub fn mul(&self, other: &Numeric) -> Numeric {
        use Numeric::*;
        match (self, other) {
            (NaN, _) | (_, NaN) => NaN,
            (Finite { int: a, scale: sa }, Finite { int: b, scale: sb }) => {
                let scale = sa + sb;
                if scale as i64 <= DSCALE_MAX
                    && let (Some(x), Some(y)) = (a.to_i64(), b.to_i64())
                {
                    return Finite { int: DecInt::from(x as i128 * y as i128), scale };
                }
                let n = Finite { int: a * b, scale };
                if scale as i64 > DSCALE_MAX {
                    n.with_scale(DSCALE_MAX as u32)
                } else {
                    n
                }
            }
            (a, b) => {
                // Infinity times zero is NaN; otherwise the sign decides.
                if a.is_zero() || b.is_zero() {
                    return NaN;
                }
                if a.is_negative() != b.is_negative() {
                    NegInf
                } else {
                    PosInf
                }
            }
        }
    }

    /// The exact product, without mul's NUMERIC_DSCALE_MAX rounding (for the aggregates'
    /// internal sums, which numeric.c keeps as unbounded NumericVars).
    pub fn mul_exact(&self, other: &Numeric) -> Numeric {
        match (self, other) {
            (Numeric::Finite { int: a, scale: sa }, Numeric::Finite { int: b, scale: sb }) => {
                Numeric::Finite {
                    int: a * b,
                    scale: sa + sb,
                }
            }
            _ => self.mul(other),
        }
    }

    /// (weight, first base-10000 digit) of a finite non-zero value, as in numeric.c.
    fn weight_and_first(int: &DecInt, scale: u32) -> (i64, i64) {
        if int.is_zero() {
            return (0, 0);
        }
        let digits = int.num_digits().max(1) as i64;
        let exp10 = digits - 1 - scale as i64;
        let weight = exp10.div_euclid(4);
        let shift = 4 * weight + scale as i64;
        // The digits of the magnitude at positions shift..shift+4 (below 0: zeros).
        let first = (0..4).rev().fold(0i64, |acc, k| {
            let pos = shift + k;
            acc * 10 + if pos >= 0 { int.digit(pos as usize) as i64 } else { 0 }
        });
        (weight, first)
    }

    fn select_div_scale(a: &DecInt, sa: u32, b: &DecInt, sb: u32) -> u32 {
        let (w1, f1) = Self::weight_and_first(a, sa);
        let (w2, f2) = Self::weight_and_first(b, sb);
        let mut qweight = w1 - w2;
        if f1 <= f2 {
            qweight -= 1;
        }
        let mut rscale = MIN_SIG_DIGITS - qweight * 4;
        rscale = rscale.max(sa as i64).max(sb as i64).max(MIN_DISPLAY_SCALE);
        rscale.min(MAX_DISPLAY_SCALE) as u32
    }

    /// Divide to an explicit result scale, rounding.
    pub fn div_scale(&self, other: &Numeric, rscale: u32) -> Result<Numeric> {
        match (self, other) {
            (Numeric::Finite { int: a, scale: sa }, Numeric::Finite { int: b, scale: sb }) => {
                if b.is_zero() {
                    bail!(DIVISION_BY_ZERO, "division by zero");
                }
                // a/10^sa ÷ b/10^sb = (a * 10^(sb + rscale - sa)) / b, in units of 10^-rscale.
                let shift = *sb as i64 + rscale as i64 - *sa as i64;
                // (Both small: in i128, without allocating.)
                if let (Some(x), Some(y)) = (a.to_i64(), b.to_i64())
                    && let Some(p) = pow10_i128(shift.unsigned_abs().min(39) as u32)
                {
                    let (n, d) = if shift >= 0 {
                        ((x as i128).checked_mul(p), Some(y as i128))
                    } else {
                        (Some(x as i128), (y as i128).checked_mul(p))
                    };
                    if let (Some(n), Some(d)) = (n, d) {
                        return Ok(Numeric::Finite { int: DecInt::from(div_round_i128(n, d)), scale: rscale });
                    }
                }
                let (n, d) = if shift >= 0 {
                    (a.mul_pow10(shift as usize), b.clone())
                } else {
                    (a.clone(), b.mul_pow10((-shift) as usize))
                };
                Ok(Numeric::Finite {
                    int: div_round(&n, &d),
                    scale: rscale,
                })
            }
            _ => self.div(other),
        }
    }

    pub fn div(&self, other: &Numeric) -> Result<Numeric> {
        use Numeric::*;
        Ok(match (self, other) {
            (NaN, _) | (_, NaN) => NaN,
            (Finite { int: a, scale: sa }, Finite { int: b, scale: sb }) => {
                if b.is_zero() {
                    bail!(DIVISION_BY_ZERO, "division by zero");
                }
                let rscale = Self::select_div_scale(a, *sa, b, *sb);
                return self.div_scale(other, rscale);
            }
            (Finite { .. }, _) => Numeric::zero(),
            (_, Finite { int, .. }) => {
                if int.is_zero() {
                    bail!(DIVISION_BY_ZERO, "division by zero");
                }
                if self.is_negative() != int.is_negative() {
                    NegInf
                } else {
                    PosInf
                }
            }
            _ => NaN,
        })
    }

    /// Integer division truncating toward zero (`div(a, b)`).
    pub fn div_trunc(&self, other: &Numeric) -> Result<Numeric> {
        match (self, other) {
            (Numeric::Finite { int: a, scale: sa }, Numeric::Finite { int: b, scale: sb }) => {
                if b.is_zero() {
                    bail!(DIVISION_BY_ZERO, "division by zero");
                }
                let (a, b, _) = Self::aligned(a, *sa, b, *sb);
                Ok(Numeric::Finite {
                    int: a / b,
                    scale: 0,
                })
            }
            _ => self.div(other).map(|n| n.trunc(0)),
        }
    }

    pub fn rem(&self, other: &Numeric) -> Result<Numeric> {
        use Numeric::*;
        Ok(match (self, other) {
            (NaN, _) | (_, NaN) => NaN,
            (Finite { int: a, scale: sa }, Finite { int: b, scale: sb }) => {
                if b.is_zero() {
                    bail!(DIVISION_BY_ZERO, "division by zero");
                }
                let (a, b, s) = Self::aligned(a, *sa, b, *sb);
                Finite {
                    int: a % b,
                    scale: s,
                }
            }
            (Finite { .. }, _) => self.clone(),
            (_, Finite { int, .. }) if int.is_zero() => bail!(DIVISION_BY_ZERO, "division by zero"),
            _ => NaN,
        })
    }

    pub fn sqrt(&self) -> Result<Numeric> {
        match self {
            Numeric::NaN => Ok(Numeric::NaN),
            Numeric::PosInf => Ok(Numeric::PosInf),
            Numeric::NegInf => Err(err!(
                INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                "cannot take square root of a negative number"
            )),
            Numeric::Finite { int, scale } => {
                if int.is_negative() {
                    bail!(
                        INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                        "cannot take square root of a negative number"
                    );
                }
                // Result scale as in numeric_sqrt: at least MIN_SIG_DIGITS significant digits.
                let (w, _) = Self::weight_and_first(int, *scale);
                let sweight = (w + 1) * 4 / 2 - 1;
                let rscale = (MIN_SIG_DIGITS - sweight)
                    .max(*scale as i64)
                    .clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE)
                    as u32;
                // sqrt(int * 10^-scale) at rscale+1 digits, then round.
                let target = rscale + 1;
                let exp = 2 * target as i64 - *scale as i64;
                let n = if exp >= 0 {
                    int.mul_pow10(exp as usize)
                } else {
                    int.div_pow10((-exp) as usize)
                };
                let r = n.sqrt();
                Ok(Numeric::Finite {
                    int: r,
                    scale: target,
                }
                .with_scale(rscale))
            }
        }
    }

    /// Square root rounded to `rscale` digits (sqrt_var with an explicit scale).
    pub fn sqrt_rscale(&self, rscale: u32) -> Result<Numeric> {
        match self {
            Numeric::Finite { int, scale } => {
                if int.is_negative() {
                    bail!(
                        INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                        "cannot take square root of a negative number"
                    );
                }
                let target = rscale + 1;
                let exp = 2 * target as i64 - *scale as i64;
                let n = if exp >= 0 {
                    int.mul_pow10(exp as usize)
                } else {
                    int.div_pow10((-exp) as usize)
                };
                Ok(Numeric::Finite {
                    int: n.sqrt(),
                    scale: target,
                }
                .with_scale(rscale))
            }
            other => other.sqrt(),
        }
    }

    /// The result scale Postgres picks for `self / other`.
    pub fn div_result_scale(&self, other: &Numeric) -> u32 {
        match (self, other) {
            (Numeric::Finite { int: a, scale: sa }, Numeric::Finite { int: b, scale: sb })
                if !b.is_zero() =>
            {
                Self::select_div_scale(a, *sa, b, *sb)
            }
            _ => 0,
        }
    }

    pub fn to_f64(&self) -> f64 {
        match self {
            Numeric::NaN => f64::NAN,
            Numeric::PosInf => f64::INFINITY,
            Numeric::NegInf => f64::NEG_INFINITY,
            n => n.to_string().parse().unwrap_or(f64::NAN),
        }
    }

    /// float8 → numeric keeps 15 significant digits (DBL_DIG), like Postgres.
    pub fn from_f64(v: f64) -> Numeric {
        Self::from_float(v, 15)
    }

    pub fn from_f32(v: f32) -> Numeric {
        Self::from_float(v as f64, 6)
    }

    fn from_float(v: f64, digits: usize) -> Numeric {
        if v.is_nan() {
            return Numeric::NaN;
        }
        if v.is_infinite() {
            return if v > 0.0 {
                Numeric::PosInf
            } else {
                Numeric::NegInf
            };
        }
        // %.{digits}g, then parse.
        let s = format!("{:.*e}", digits - 1, v);
        let n: Numeric = s.parse().unwrap_or_else(|_| Numeric::zero());
        // %g drops trailing zeros.
        n.normalize()
    }

    /// Round to an integer (half away from zero) and convert.
    pub fn to_i64_round(&self) -> Option<i64> {
        match self {
            Numeric::Finite { .. } => {
                if let Numeric::Finite { int, .. } = self.with_scale(0) {
                    int.to_i64()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn to_i128_round(&self) -> Option<i128> {
        match self.with_scale(0) {
            Numeric::Finite { int, .. } => int.to_i128(),
            _ => None,
        }
    }

    /// Enforce `numeric(p, s)`.
    pub fn apply_typmod(&self, typmod: i32) -> Result<Numeric> {
        if typmod < 4 {
            return Ok(self.clone());
        }
        let (p, s) = super::numeric_typmod_parts(typmod);
        match self {
            Numeric::Finite { .. } => {
                let r = self.round(s as i64);
                // Integer digits allowed: p - s.
                let maxdigits = p - s;
                if let Numeric::Finite { int, scale } = &r
                    && !int.is_zero()
                {
                    let digits = int.num_digits().max(1) as i64 - *scale as i64;
                    if digits > maxdigits as i64 {
                        let limit = if maxdigits != 0 {
                            format!("10^{maxdigits}")
                        } else {
                            "1".into()
                        };
                        return Err(Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "numeric field overflow").detail(
                            format!("A field with precision {p}, scale {s} must round to an absolute value less than {limit}."),
                        ));
                    }
                }
                Ok(r)
            }
            Numeric::NaN => Ok(Numeric::NaN),
            _ => Err(
                Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "numeric field overflow").detail(
                    format!("A field with precision {p}, scale {s} cannot hold an infinite value."),
                ),
            ),
        }
    }

    /// numeric_in (without typmod): syntax errors and "value overflows numeric format".
    pub fn parse_pg(s: &str) -> Result<Numeric> {
        match parse_numeric(s) {
            Ok(n) => n.checked(),
            Err(true) => Err(err!(
                NUMERIC_VALUE_OUT_OF_RANGE,
                "value overflows numeric format"
            )),
            Err(false) => Err(err!(
                INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type numeric: \"{s}\""
            )),
        }
    }

    /// make_result's range check: the base-10000 weight and the display scale must fit the
    /// on-disk header.
    pub fn checked(self) -> Result<Numeric> {
        if let Numeric::Finite { int, scale } = &self {
            if *scale as i64 > DSCALE_MAX {
                bail!(NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format");
            }
            // (Below 2^63 at a scale up to DSCALE_MAX the weight is within -4097..=4.)
            if !int.is_zero() && int.to_i64().is_none() {
                let w = Var::from_parts(int.clone(), *scale as i64).weight();
                if !(WEIGHT_MIN..=WEIGHT_MAX).contains(&w) {
                    bail!(NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format");
                }
            }
        }
        Ok(self)
    }

    /// Integer power with an integer exponent, exact.
    pub fn pow_int(&self, exp: i64) -> Result<Numeric> {
        match self {
            Numeric::Finite { int, scale } => {
                if exp >= 0 {
                    if exp > 100_000 {
                        bail!(NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format");
                    }
                    let e = exp as u32;
                    let s = scale.saturating_mul(e);
                    let n = Numeric::Finite {
                        int: int.pow(e),
                        scale: s,
                    };
                    // power() result scale: at least MIN_SIG_DIGITS significant, max scale of inputs.
                    if s as i64 > MAX_DISPLAY_SCALE {
                        Ok(n.with_scale(MAX_DISPLAY_SCALE as u32))
                    } else {
                        Ok(n)
                    }
                } else {
                    if int.is_zero() {
                        bail!(
                            DIVISION_BY_ZERO,
                            "zero raised to a negative power is undefined"
                        );
                    }
                    let pos = self.pow_int(-exp)?;
                    Numeric::from_i64(1).div(&pos)
                }
            }
            _ => Ok(Numeric::from_f64(self.to_f64().powf(exp as f64))),
        }
    }
}

impl Numeric {
    fn var(&self) -> Var {
        match self {
            Numeric::Finite { int, scale } => Var::from_parts(int.clone(), *scale as i64),
            _ => Var::zero(),
        }
    }

    /// numeric_exp.
    pub fn exp(&self) -> Result<Numeric> {
        match self {
            Numeric::NaN | Numeric::PosInf => Ok(self.clone()),
            Numeric::NegInf => Ok(Numeric::zero()),
            Numeric::Finite { .. } => {
                let arg = self.var();
                // Approximate decimal weight of the result: x * log10(e).
                let val = (arg.to_f64() * PG_LOG10_E)
                    .clamp(-MAX_RESULT_SCALE as f64, MAX_RESULT_SCALE as f64);
                let rscale = (MIN_SIG_DIGITS - val as i64)
                    .max(arg.scale)
                    .clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE);
                exp_var(&arg, rscale)?.into_numeric().checked()
            }
        }
    }

    /// numeric_ln.
    pub fn ln(&self) -> Result<Numeric> {
        match self {
            Numeric::NegInf => Err(log_negative()),
            Numeric::NaN | Numeric::PosInf => Ok(self.clone()),
            Numeric::Finite { .. } => {
                let arg = self.var();
                let ln_dweight = estimate_ln_dweight(&arg);
                let rscale = (MIN_SIG_DIGITS - ln_dweight)
                    .max(arg.scale)
                    .clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE);
                ln_var(&arg, rscale)?.into_numeric().checked()
            }
        }
    }

    /// numeric_log: the logarithm of `num` to base `self`.
    pub fn log(&self, num: &Numeric) -> Result<Numeric> {
        if !self.is_finite() || !num.is_finite() {
            if self.is_nan() || num.is_nan() {
                return Ok(Numeric::NaN);
            }
            if self.is_negative() || num.is_negative() {
                return Err(log_negative());
            }
            if self.is_zero() || num.is_zero() {
                return Err(log_zero());
            }
            if matches!(self, Numeric::PosInf) {
                // log(Inf, Inf) is Inf/Inf; log(Inf, finite) is zero.
                return Ok(if matches!(num, Numeric::PosInf) {
                    Numeric::NaN
                } else {
                    Numeric::zero()
                });
            }
            return Ok(Numeric::PosInf);
        }
        log_var(&self.var(), &num.var())?.into_numeric().checked()
    }

    /// numeric_power, with POSIX pow(3)'s rules for NaN and infinities.
    pub fn power(&self, exp: &Numeric) -> Result<Numeric> {
        let one = Numeric::from_i64(1);
        if !self.is_finite() || !exp.is_finite() {
            if self.is_nan() {
                return Ok(if exp.is_zero() { one } else { Numeric::NaN });
            }
            if exp.is_nan() {
                return Ok(if *self == one { one } else { Numeric::NaN });
            }
            let sign1 = sign_of(self);
            let sign2 = sign_of(exp);
            if sign1 == 0 && sign2 < 0 {
                bail!(
                    INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                    "zero raised to a negative power is undefined"
                );
            }
            if sign1 < 0 && !is_integral(exp) {
                bail!(
                    INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                    "a negative number raised to a non-integer power yields a complex result"
                );
            }
            if *self == one || sign2 == 0 {
                return Ok(one);
            }
            if sign1 == 0 && sign2 > 0 {
                return Ok(Numeric::zero());
            }
            if !exp.is_finite() {
                let abs_x_gt_one = if !self.is_finite() {
                    true
                } else {
                    if *self == Numeric::from_i64(-1) {
                        return Ok(one);
                    }
                    self.abs() > one
                };
                return Ok(if abs_x_gt_one == (sign2 > 0) {
                    Numeric::PosInf
                } else {
                    Numeric::zero()
                });
            }
            if matches!(self, Numeric::PosInf) {
                return Ok(if sign2 > 0 {
                    Numeric::PosInf
                } else {
                    Numeric::zero()
                });
            }
            // -Infinity.
            if sign2 < 0 {
                return Ok(Numeric::zero());
            }
            return Ok(if is_odd_integer(exp) {
                Numeric::NegInf
            } else {
                Numeric::PosInf
            });
        }
        if self.is_zero() && exp.is_negative() {
            bail!(
                INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                "zero raised to a negative power is undefined"
            );
        }
        power_var(&self.var(), &exp.var())?.into_numeric().checked()
    }

    /// numeric_gcd: computed at the larger input scale.
    pub fn gcd(&self, other: &Numeric) -> Numeric {
        match (self, other) {
            (Numeric::Finite { int: a, scale: sa }, Numeric::Finite { int: b, scale: sb }) => {
                let (a, b, s) = Self::aligned(a, *sa, b, *sb);
                Numeric::Finite {
                    int: big_gcd(a, b),
                    scale: s,
                }
            }
            _ => Numeric::NaN,
        }
    }

    /// numeric_lcm: abs(a) / gcd(a, b) * abs(b), at the larger input scale.
    pub fn lcm(&self, other: &Numeric) -> Result<Numeric> {
        match (self, other) {
            (Numeric::Finite { int: a, scale: sa }, Numeric::Finite { int: b, scale: sb }) => {
                let (a, b, s) = Self::aligned(a, *sa, b, *sb);
                if a.is_zero() || b.is_zero() {
                    return Ok(Numeric::zero().with_scale(s));
                }
                let g = big_gcd(a.clone(), b.clone());
                Numeric::Finite {
                    int: (a.abs() / g) * b.abs(),
                    scale: s,
                }
                .checked()
            }
            _ => Ok(Numeric::NaN),
        }
    }

    /// numeric_inc: self + 1.
    pub fn inc(&self) -> Result<Numeric> {
        self.add(&Numeric::from_i64(1)).checked()
    }
}

fn big_gcd(mut a: DecInt, mut b: DecInt) -> DecInt {
    while !b.is_zero() {
        let t = &a % &b;
        a = b;
        b = t;
    }
    a.abs()
}

fn log_negative() -> Error {
    err!(
        INVALID_ARGUMENT_FOR_LOGARITHM,
        "cannot take logarithm of a negative number"
    )
}

fn log_zero() -> Error {
    err!(
        INVALID_ARGUMENT_FOR_LOGARITHM,
        "cannot take logarithm of zero"
    )
}

fn overflow() -> Error {
    err!(NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format")
}

fn sign_of(n: &Numeric) -> i32 {
    match n {
        Numeric::PosInf => 1,
        Numeric::NegInf => -1,
        Numeric::NaN => 0,
        Numeric::Finite { int, .. } => match int.sign() {
            Sign::Minus => -1,
            Sign::NoSign => 0,
            Sign::Plus => 1,
        },
    }
}

/// numeric_is_integral: no fractional part; infinities count as integral, NaN does not.
fn is_integral(n: &Numeric) -> bool {
    match n {
        Numeric::NaN => false,
        Numeric::PosInf | Numeric::NegInf => true,
        Numeric::Finite { .. } => n.var().is_integral(),
    }
}

fn is_odd_integer(n: &Numeric) -> bool {
    let v = n.var();
    v.is_integral() && v.integer_part().is_odd()
}

/// numeric.c's NumericVar for the transcendental functions: `int * 10^-scale`. The scale is
/// the display scale (dscale), which may go negative while reducing arguments; operations
/// round to an explicit result scale like mul_var/div_var/round_var.
#[derive(Clone, Debug)]
struct Var {
    int: DecInt,
    scale: i64,
}

fn rescale(int: &DecInt, from: i64, to: i64) -> DecInt {
    match to.cmp(&from) {
        Ordering::Equal => int.clone(),
        Ordering::Greater => int.mul_pow10((to - from) as usize),
        Ordering::Less => int.round_pow10((from - to) as usize),
    }
}

impl Var {
    fn from_parts(int: DecInt, scale: i64) -> Var {
        Var { int, scale }
    }

    fn from_i64(v: i64) -> Var {
        Var {
            int: DecInt::from(v),
            scale: 0,
        }
    }

    fn zero() -> Var {
        Var::from_i64(0)
    }

    fn one() -> Var {
        Var::from_i64(1)
    }

    fn into_numeric(self) -> Numeric {
        if self.scale >= 0 {
            Numeric::Finite {
                int: self.int,
                scale: self.scale as u32,
            }
        } else {
            Numeric::Finite {
                int: self.int.mul_pow10((-self.scale) as usize),
                scale: 0,
            }
        }
    }

    fn is_zero(&self) -> bool {
        self.int.is_zero()
    }

    fn is_integral(&self) -> bool {
        self.scale <= 0 || self.int.divisible_pow10(self.scale as usize)
    }

    /// The value truncated to an integer.
    fn integer_part(&self) -> DecInt {
        if self.scale <= 0 {
            self.int.mul_pow10((-self.scale) as usize)
        } else {
            self.int.div_pow10(self.scale as usize)
        }
    }

    /// Decimal exponent of the leading digit.
    fn dexp(&self) -> i64 {
        self.int.num_digits().max(1) as i64 - 1 - self.scale
    }

    /// The base-NBASE weight (zero has weight 0).
    fn weight(&self) -> i64 {
        if self.is_zero() {
            0
        } else {
            self.dexp().div_euclid(4)
        }
    }

    /// The base-10000 digits from the most significant, trailing zero digits stripped.
    fn nbase_digits(&self) -> Vec<u32> {
        if self.is_zero() {
            return Vec::new();
        }
        let s = self.int.abs().to_string();
        let lead = self.dexp() - 4 * self.weight() + 1;
        let mut d = "0".repeat((4 - lead) as usize);
        d.push_str(&s);
        while !d.len().is_multiple_of(4) {
            d.push('0');
        }
        let mut out: Vec<u32> = d
            .as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).unwrap().parse().unwrap())
            .collect();
        while out.last() == Some(&0) {
            out.pop();
        }
        out
    }

    fn cmp(&self, o: &Var) -> Ordering {
        let s = self.scale.max(o.scale);
        rescale(&self.int, self.scale, s).cmp(&rescale(&o.int, o.scale, s))
    }

    fn add(&self, o: &Var) -> Var {
        let s = self.scale.max(o.scale);
        Var {
            int: rescale(&self.int, self.scale, s) + rescale(&o.int, o.scale, s),
            scale: s,
        }
    }

    fn sub(&self, o: &Var) -> Var {
        let s = self.scale.max(o.scale);
        Var {
            int: rescale(&self.int, self.scale, s) - rescale(&o.int, o.scale, s),
            scale: s,
        }
    }

    /// round_var.
    fn round(&self, rscale: i64) -> Var {
        Var {
            int: rescale(&self.int, self.scale, rscale),
            scale: rscale,
        }
    }

    /// mul_var: the product rounded to `rscale`.
    fn mul(&self, o: &Var, rscale: i64) -> Var {
        Var {
            int: rescale(&(&self.int * &o.int), self.scale + o.scale, rscale),
            scale: rscale,
        }
    }

    /// div_var with rounding.
    fn div(&self, o: &Var, rscale: i64) -> Result<Var> {
        if o.is_zero() {
            bail!(DIVISION_BY_ZERO, "division by zero");
        }
        // (a/10^sa) / (b/10^sb) in units of 10^-rscale = a * 10^(sb + rscale - sa) / b.
        let shift = o.scale + rscale - self.scale;
        let (n, d) = if shift >= 0 {
            (self.int.mul_pow10(shift as usize), o.int.clone())
        } else {
            (self.int.clone(), o.int.mul_pow10((-shift) as usize))
        };
        Ok(Var {
            int: div_round(&n, &d),
            scale: rscale,
        })
    }

    /// div_var_int with rounding.
    fn div_int(&self, i: i64, rscale: i64) -> Var {
        self.div(&Var::from_i64(i), rscale)
            .unwrap_or_else(|_| Var::zero())
    }

    /// sqrt_var: the square root rounded to `rscale` (which may be negative).
    fn sqrt(&self, rscale: i64) -> Var {
        // floor(sqrt) one decimal digit beyond rscale, then round: the same as rounding the
        // exact root.
        let s = rscale + 1;
        let e = 2 * s - self.scale;
        let n = if e >= 0 {
            self.int.mul_pow10(e as usize)
        } else {
            self.int.div_pow10((-e) as usize)
        };
        Var {
            int: n.sqrt(),
            scale: s,
        }
        .round(rscale)
    }

    /// numericvar_to_double_no_overflow.
    fn to_f64(&self) -> f64 {
        let mut s = self.int.to_string();
        if self.scale != 0 {
            s.push_str(&format!("e{}", -self.scale));
        }
        s.parse().unwrap_or(0.0)
    }
}

/// estimate_ln_dweight: the approximate decimal weight of ln(var).
fn estimate_ln_dweight(var: &Var) -> i64 {
    if var.int.sign() != Sign::Plus {
        return 0;
    }
    let p9 = Var::from_parts(DecInt::from(9), 1);
    let p11 = Var::from_parts(DecInt::from(11), 1);
    if var.cmp(&p9) != Ordering::Less && var.cmp(&p11) != Ordering::Greater {
        // ln(1 + x) ~= x.
        let x = var.sub(&Var::one());
        if x.is_zero() {
            return 0;
        }
        let d = x.nbase_digits();
        return x.weight() * 4 + (d[0] as f64).log10() as i64;
    }
    let d = var.nbase_digits();
    let mut digits = d[0] as f64;
    let mut dweight = var.weight() * 4;
    if d.len() > 1 {
        digits = digits * 10000.0 + d[1] as f64;
        dweight -= 4;
    }
    let ln = digits.ln() + dweight as f64 * PG_LN_10;
    ln.abs().log10() as i64
}

/// exp_var: e^arg rounded to `rscale`.
fn exp_var(arg: &Var, rscale: i64) -> Result<Var> {
    let mut x = arg.clone();
    let mut val = x.to_f64();
    if val.abs() >= (MAX_RESULT_SCALE * 3) as f64 {
        if val > 0.0 {
            return Err(overflow());
        }
        return Ok(Var::zero().round(rscale));
    }
    let dweight = (val * PG_LOG10_E) as i64;
    // Reduce x to about |x| <= 0.01 by dividing by 2^ndiv2.
    let mut ndiv2 = 0i64;
    if val.abs() > 0.01 {
        ndiv2 = 1;
        val /= 2.0;
        while val.abs() > 0.01 {
            ndiv2 += 1;
            val /= 2.0;
        }
        let local_rscale = x.scale + ndiv2;
        x = x.div_int(1 << ndiv2, local_rscale);
    }
    let sig_digits =
        (1 + dweight + rscale + (ndiv2 as f64 * PG_LOG10_2) as i64).max(0) + 8;
    let local_rscale = sig_digits - 1;
    // Taylor series.
    let mut result = Var::one().add(&x);
    let mut elem = x.mul(&x, local_rscale);
    let mut ni = 2;
    elem = elem.div_int(ni, local_rscale);
    while !elem.is_zero() {
        result = result.add(&elem);
        elem = elem.mul(&x, local_rscale);
        ni += 1;
        elem = elem.div_int(ni, local_rscale);
    }
    // Undo the range reduction.
    while ndiv2 > 0 {
        ndiv2 -= 1;
        let local_rscale = (sig_digits - result.weight() * 2 * 4).max(MIN_DISPLAY_SCALE);
        result = result.mul(&result, local_rscale);
    }
    Ok(result.round(rscale))
}

/// ln_var: the natural logarithm rounded to `rscale`.
fn ln_var(arg: &Var, rscale: i64) -> Result<Var> {
    match arg.int.sign() {
        Sign::NoSign => return Err(log_zero()),
        Sign::Minus => return Err(log_negative()),
        Sign::Plus => {}
    }
    let two = Var::from_i64(2);
    let p9 = Var::from_parts(DecInt::from(9), 1);
    let p11 = Var::from_parts(DecInt::from(11), 1);
    let mut x = arg.clone();
    let mut fact = two.clone();
    // Reduce into 0.9 < x < 1.1 with repeated square roots.
    let mut nsqrt = 0i64;
    while x.cmp(&p9) != Ordering::Greater {
        let local_rscale = rscale - x.weight() * 4 / 2 + 8;
        x = x.sqrt(local_rscale);
        fact = fact.mul(&two, 0);
        nsqrt += 1;
    }
    while x.cmp(&p11) != Ordering::Less {
        let local_rscale = rscale - x.weight() * 4 / 2 + 8;
        x = x.sqrt(local_rscale);
        fact = fact.mul(&two, 0);
        nsqrt += 1;
    }
    // Taylor series for 0.5 * ln((1+z)/(1-z)), z = (x-1)/(x+1).
    let local_rscale = rscale + ((nsqrt + 1) as f64 * PG_LOG10_2) as i64 + 8;
    let mut result = x.sub(&Var::one());
    let elem = x.add(&Var::one());
    result = result.div(&elem, local_rscale)?;
    let mut xx = result.clone();
    let x = result.mul(&result, local_rscale);
    let mut ni = 1;
    loop {
        ni += 2;
        xx = xx.mul(&x, local_rscale);
        let elem = xx.div_int(ni, local_rscale);
        if elem.is_zero() {
            break;
        }
        result = result.add(&elem);
        if elem.weight() < result.weight() - local_rscale * 2 / 4 {
            break;
        }
    }
    Ok(result.mul(&fact, rscale))
}

/// log_var: log of `num` to base `base`.
fn log_var(base: &Var, num: &Var) -> Result<Var> {
    let ln_base_dweight = estimate_ln_dweight(base);
    let ln_num_dweight = estimate_ln_dweight(num);
    let result_dweight = ln_num_dweight - ln_base_dweight;
    let rscale = (MIN_SIG_DIGITS - result_dweight)
        .max(base.scale)
        .max(num.scale)
        .clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE);
    let ln_base_rscale = (rscale + result_dweight - ln_base_dweight + 8).max(MIN_DISPLAY_SCALE);
    let ln_num_rscale = (rscale + result_dweight - ln_num_dweight + 8).max(MIN_DISPLAY_SCALE);
    let ln_base = ln_var(base, ln_base_rscale)?;
    let ln_num = ln_var(num, ln_num_rscale)?;
    ln_num.div(&ln_base, rscale)
}

/// power_var: base ^ exp for finite inputs.
fn power_var(base: &Var, exp: &Var) -> Result<Var> {
    if exp.is_integral()
        && let Some(e) = exp.integer_part().to_i64()
        && (i32::MIN as i64..=i32::MAX as i64).contains(&e)
    {
        return power_var_int(base, e, exp.scale);
    }
    if base.is_zero() {
        return Ok(Var::zero().round(MIN_SIG_DIGITS));
    }
    let mut res_neg = false;
    let mut base = base.clone();
    if base.int.is_negative() {
        if !exp.is_integral() {
            bail!(
                INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                "a negative number raised to a non-integer power yields a complex result"
            );
        }
        res_neg = exp.integer_part().is_odd();
        base.int = -base.int;
    }
    // A low-precision exp * ln(base) for the result weight and a crude overflow test.
    let ln_dweight = estimate_ln_dweight(&base);
    let local_rscale = (8 - ln_dweight).max(MIN_DISPLAY_SCALE);
    let ln_base = ln_var(&base, local_rscale)?;
    let ln_num = ln_base.mul(exp, local_rscale);
    let mut val = ln_num.to_f64();
    if val.abs() > MAX_RESULT_SCALE as f64 * 3.01 {
        if val > 0.0 {
            return Err(overflow());
        }
        return Ok(Var::zero().round(MAX_DISPLAY_SCALE));
    }
    val *= PG_LOG10_E;
    let rscale = (MIN_SIG_DIGITS - val as i64)
        .max(base.scale)
        .max(exp.scale)
        .clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE);
    let sig_digits = (rscale + val as i64).max(0);
    let local_rscale = (sig_digits - ln_dweight + 8).max(MIN_DISPLAY_SCALE);
    let ln_base = ln_var(&base, local_rscale)?;
    let ln_num = ln_base.mul(exp, local_rscale);
    let mut result = exp_var(&ln_num, rscale)?;
    if res_neg && !result.is_zero() {
        result.int = -result.int;
    }
    Ok(result)
}

/// power_var_int: base ^ exp for an int32 exponent.
fn power_var_int(base: &Var, exp: i64, exp_dscale: i64) -> Result<Var> {
    // Approximate decimal weight of the result, from base ~= f * 10^p.
    let f = if !base.is_zero() {
        let d = base.nbase_digits();
        let mut f = d[0] as f64;
        let mut p = base.weight() * 4;
        let mut i = 1;
        while i < d.len() && i * 4 < 16 {
            f = f * 10000.0 + d[i] as f64;
            p -= 4;
            i += 1;
        }
        exp as f64 * (f.log10() + p as f64)
    } else {
        0.0
    };
    if f > ((WEIGHT_MAX + 1) * 4) as f64 {
        return Err(overflow());
    }
    if f + 1.0 < -(MAX_DISPLAY_SCALE as f64) {
        return Ok(Var::zero().round(MAX_DISPLAY_SCALE));
    }
    let rscale = (MIN_SIG_DIGITS - f as i64)
        .max(base.scale)
        .max(exp_dscale)
        .clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE);
    match exp {
        0 => return Ok(Var::one().round(rscale)),
        1 => return Ok(base.round(rscale)),
        -1 => return Var::one().div(base, rscale),
        2 => return Ok(base.mul(base, rscale)),
        _ => {}
    }
    if base.is_zero() {
        if exp < 0 {
            bail!(DIVISION_BY_ZERO, "division by zero");
        }
        return Ok(Var::zero().round(rscale));
    }
    let mut sig_digits = 1 + rscale + f as i64;
    sig_digits += (exp as f64).abs().ln() as i64 + 8;
    let mut neg = exp < 0;
    let mut mask = exp.unsigned_abs();
    let mut base_prod = base.clone();
    let mut result = if mask & 1 == 1 {
        base.clone()
    } else {
        Var::one()
    };
    loop {
        mask >>= 1;
        if mask == 0 {
            break;
        }
        let local_rscale = (sig_digits - 2 * base_prod.weight() * 4)
            .min(2 * base_prod.scale)
            .max(MIN_DISPLAY_SCALE);
        base_prod = base_prod.mul(&base_prod, local_rscale);
        if mask & 1 == 1 {
            let local_rscale = (sig_digits - (base_prod.weight() + result.weight()) * 4)
                .min(base_prod.scale + result.scale)
                .max(MIN_DISPLAY_SCALE);
            result = base_prod.mul(&result, local_rscale);
        }
        // Past the int16 weight the result must overflow (or underflow if exp < 0).
        if base_prod.weight() > WEIGHT_MAX || result.weight() > WEIGHT_MAX {
            if !neg {
                return Err(overflow());
            }
            result = Var::zero();
            neg = false;
            break;
        }
    }
    if neg {
        Var::one().div(&result, rscale)
    } else {
        Ok(result.round(rscale))
    }
}

impl std::str::FromStr for Numeric {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        parse_numeric(s).map_err(|_| ())
    }
}

/// numeric_in's grammar (set_var_from_str / set_var_from_non_decimal_integer_str). Err(true)
/// is "value overflows numeric format", Err(false) a syntax error. Limits beyond the exponent
/// are checked by `checked`.
fn parse_numeric(s: &str) -> std::result::Result<Numeric, bool> {
    let b = s.as_bytes();
    let isspace = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let mut i = 0;
    while i < b.len() && isspace(b[i]) {
        i += 1;
    }
    let numstart = i;
    let mut neg = false;
    if i < b.len() && b[i] == b'+' {
        i += 1;
    } else if i < b.len() && b[i] == b'-' {
        neg = true;
        i += 1;
    }
    let trailing_ok = |mut j: usize| -> bool {
        while j < b.len() {
            if !isspace(b[j]) {
                return false;
            }
            j += 1;
        }
        true
    };
    if i >= b.len() || (!b[i].is_ascii_digit() && b[i] != b'.') {
        let rest = &s[numstart..];
        let starts = |t: &[u8], w: &str| {
            t.len() >= w.len() && t[..w.len()].eq_ignore_ascii_case(w.as_bytes())
        };
        let (v, end) = if starts(rest.as_bytes(), "NaN") {
            (Numeric::NaN, numstart + 3)
        } else if starts(&b[i..], "Infinity") {
            (if neg { Numeric::NegInf } else { Numeric::PosInf }, i + 8)
        } else if starts(&b[i..], "inf") {
            (if neg { Numeric::NegInf } else { Numeric::PosInf }, i + 3)
        } else {
            return Err(false);
        };
        if !trailing_ok(end) {
            return Err(false);
        }
        return Ok(v);
    }
    // Non-decimal integers.
    if b[i] == b'0' && i + 1 < b.len() {
        let radix = match b[i + 1] {
            b'x' | b'X' => 16,
            b'o' | b'O' => 8,
            b'b' | b'B' => 2,
            _ => 10,
        };
        if radix != 10 {
            let mut j = i + 2;
            let first = j;
            let mut digits = String::new();
            while j < b.len() {
                let c = b[j];
                if (c as char).is_digit(radix) {
                    digits.push(c as char);
                    j += 1;
                } else if c == b'_' {
                    j += 1;
                    if j >= b.len() || !(b[j] as char).is_digit(radix) {
                        return Err(false);
                    }
                } else {
                    break;
                }
            }
            if j == first || digits.is_empty() || !trailing_ok(j) {
                return Err(false);
            }
            let v = DecInt::parse_radix(digits.as_bytes(), radix).ok_or(false)?;
            return Ok(Numeric::Finite {
                int: if neg { -v } else { v },
                scale: 0,
            });
        }
    }
    // Decimal.
    let mut have_dp = false;
    if b[i] == b'.' {
        have_dp = true;
        i += 1;
    }
    if i >= b.len() || !b[i].is_ascii_digit() {
        return Err(false);
    }
    let mut digits = String::new();
    let mut dscale: i64 = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            digits.push(c as char);
            if have_dp {
                dscale += 1;
            }
            i += 1;
        } else if c == b'.' {
            if have_dp {
                return Err(false);
            }
            have_dp = true;
            i += 1;
            if i < b.len() && b[i] == b'_' {
                return Err(false);
            }
        } else if c == b'_' {
            i += 1;
            if i >= b.len() || !b[i].is_ascii_digit() {
                return Err(false);
            }
        } else {
            break;
        }
    }
    let mut exponent: i64 = 0;
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        let mut eneg = false;
        if i < b.len() && b[i] == b'+' {
            i += 1;
        } else if i < b.len() && b[i] == b'-' {
            eneg = true;
            i += 1;
        }
        if i >= b.len() || !b[i].is_ascii_digit() {
            return Err(false);
        }
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_digit() {
                exponent = exponent * 10 + (c - b'0') as i64;
                if exponent > (i32::MAX / 2) as i64 {
                    return Err(true);
                }
                i += 1;
            } else if c == b'_' {
                i += 1;
                if i >= b.len() || !b[i].is_ascii_digit() {
                    return Err(false);
                }
            } else {
                break;
            }
        }
        if eneg {
            exponent = -exponent;
        }
    }
    if !trailing_ok(i) {
        return Err(false);
    }
    // value = digits * 10^(exponent - fraction digits); dscale = max(fraction - exponent, 0).
    let shift = exponent - dscale;
    let out_scale = (dscale - exponent).max(0);
    let mut int: DecInt = digits.parse().map_err(|_| false)?;
    // Leading significant digit's decimal exponent, to refuse absurd sizes before allocating.
    let lead = digits.trim_start_matches('0').len() as i64 - 1 + shift;
    if !int.is_zero() && lead > (WEIGHT_MAX + 2) * 4 {
        return Err(true);
    }
    if out_scale > DSCALE_MAX + 4 {
        return Err(true);
    }
    // Express at out_scale.
    let e = shift + out_scale;
    if e >= 0 {
        int = int.mul_pow10(e as usize);
    } else {
        int = int.div_pow10((-e) as usize);
    }
    Ok(Numeric::Finite {
        int: if neg { -int } else { int },
        scale: out_scale as u32,
    })
}

impl fmt::Display for Numeric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Numeric::NaN => f.write_str("NaN"),
            Numeric::PosInf => f.write_str("Infinity"),
            Numeric::NegInf => f.write_str("-Infinity"),
            Numeric::Finite { int, scale } => {
                let neg = int.is_negative();
                let digits = int.magnitude_string();
                let scale = *scale as usize;
                let mut out = String::with_capacity(digits.len() + 3);
                if neg {
                    out.push('-');
                }
                if scale == 0 {
                    out.push_str(&digits);
                } else if digits.len() > scale {
                    out.push_str(&digits[..digits.len() - scale]);
                    out.push('.');
                    out.push_str(&digits[digits.len() - scale..]);
                } else {
                    out.push_str("0.");
                    for _ in 0..scale - digits.len() {
                        out.push('0');
                    }
                    out.push_str(&digits);
                }
                f.write_str(&out)
            }
        }
    }
}

impl PartialEq for Numeric {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Numeric {}

impl PartialOrd for Numeric {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Numeric {
    /// -Infinity < finite < Infinity < NaN (NaN equals itself), as Postgres sorts them.
    fn cmp(&self, other: &Self) -> Ordering {
        use Numeric::*;
        let rank = |n: &Numeric| match n {
            NegInf => 0,
            Finite { .. } => 1,
            PosInf => 2,
            NaN => 3,
        };
        match (self, other) {
            (Finite { int: a, scale: sa }, Finite { int: b, scale: sb }) => {
                if let Some((x, y, _)) = aligned_small(a, *sa, b, *sb) {
                    return x.cmp(&y);
                }
                let (a, b, _) = Self::aligned(a, *sa, b, *sb);
                a.cmp(&b)
            }
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

impl Hash for Numeric {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // (A value fitting an i64 normalised without allocating; either way, a normalised
        // value fitting an i64 hashes as one, so equal values hash alike.)
        if let Numeric::Finite { int, scale } = self
            && let Some(mut v) = int.to_i64()
        {
            let mut scale = *scale;
            while scale > 0 && v != 0 && v % 10 == 0 {
                v /= 10;
                scale -= 1;
            }
            if v == 0 {
                scale = 0;
            }
            0u8.hash(state);
            v.hash(state);
            scale.hash(state);
            return;
        }
        match self.normalize() {
            Numeric::Finite { int, scale } => match int.to_i64() {
                Some(v) => {
                    0u8.hash(state);
                    v.hash(state);
                    scale.hash(state);
                }
                None => {
                    5u8.hash(state);
                    int.hash(state);
                    scale.hash(state);
                }
            },
            Numeric::NaN => 1u8.hash(state),
            Numeric::PosInf => 2u8.hash(state),
            Numeric::NegInf => 3u8.hash(state),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Numeric {
        s.parse().unwrap()
    }

    #[test]
    fn parse_and_print() {
        assert_eq!(n("1.50").to_string(), "1.50");
        assert_eq!(n("-0.001").to_string(), "-0.001");
        assert_eq!(n("1.5e3").to_string(), "1500");
        assert_eq!(n("1.50e1").to_string(), "15.0");
        assert_eq!(n("12e-3").to_string(), "0.012");
        assert_eq!(n(".5").to_string(), "0.5");
        assert_eq!(n("5.").to_string(), "5");
        assert_eq!(n("1_000").to_string(), "1000");
        assert_eq!(n("0x1F").to_string(), "31");
        assert!("abc".parse::<Numeric>().is_err());
        assert!(".".parse::<Numeric>().is_err());
    }

    #[test]
    fn arithmetic_scales() {
        assert_eq!(n("1.50").add(&n("1")).to_string(), "2.50");
        assert_eq!(n("1.5").mul(&n("2.25")).to_string(), "3.375");
        assert_eq!(
            n("1").div(&n("3")).unwrap().to_string(),
            "0.33333333333333333333"
        );
        assert_eq!(
            n("10").div(&n("4")).unwrap().to_string(),
            "2.5000000000000000"
        );
        assert_eq!(
            n("2").div(&n("3")).unwrap().to_string(),
            "0.66666666666666666667"
        );
        assert_eq!(
            n("100").div(&n("7")).unwrap().to_string(),
            "14.2857142857142857"
        );
        assert_eq!(n("7.5").rem(&n("2")).unwrap().to_string(), "1.5");
        assert_eq!(n("-7.5").rem(&n("2")).unwrap().to_string(), "-1.5");
        assert_eq!(n("2").sqrt().unwrap().to_string(), "1.414213562373095");
    }

    #[test]
    fn rounding() {
        assert_eq!(n("2.5").round(0).to_string(), "3");
        assert_eq!(n("-2.5").round(0).to_string(), "-3");
        assert_eq!(n("1234.5678").round(-2).to_string(), "1200");
        assert_eq!(n("1234.5678").round(2).to_string(), "1234.57");
        assert_eq!(n("1234.5678").trunc(2).to_string(), "1234.56");
        assert_eq!(n("-1.5").floor().to_string(), "-2");
        assert_eq!(n("-1.5").ceil().to_string(), "-1");
    }

    #[test]
    fn typmod() {
        let m = ((5 << 16) | 2) + 4;
        assert_eq!(n("123.456").apply_typmod(m).unwrap().to_string(), "123.46");
        assert!(n("1234.5").apply_typmod(m).is_err());
    }

    #[test]
    fn ordering() {
        assert!(n("1.0") == n("1.00"));
        assert!(Numeric::NaN > Numeric::PosInf);
        assert!(Numeric::NegInf < n("-1e100"));
    }

    #[test]
    fn from_float() {
        assert_eq!(Numeric::from_f64(0.1).to_string(), "0.1");
        assert_eq!(
            Numeric::from_f64(1.0 / 3.0).to_string(),
            "0.333333333333333"
        );
        assert_eq!(Numeric::from_f64(1e20).to_string(), "100000000000000000000");
    }

    /// The i64 fast paths of add / sub / mul / cmp / checked give what the DecInt
    /// arithmetic gives (computed here as the general path does), and equal values hash
    /// alike however they're written.
    #[test]
    fn small_fast_paths_match_bigint() {
        use std::hash::BuildHasher;
        let vals: Vec<(i64, u32)> = [0i64, 1, -1, 7, -12, 100, 999_999, -1_000_000, i64::MAX, i64::MIN, i64::MAX / 3]
            .iter()
            .flat_map(|&v| [0u32, 1, 2, 5, 18].map(|s| (v, s)))
            .collect();
        let big = |v: i64, s: u32| Numeric::Finite { int: DecInt::from(v), scale: s };
        let slow = |a: (i64, u32), b: (i64, u32), op: char| -> Numeric {
            let (x, y) = (DecInt::from(a.0), DecInt::from(b.0));
            let (x, y, s) = Numeric::aligned(&x, a.1, &y, b.1);
            match op {
                '+' => Numeric::Finite { int: x + y, scale: s },
                '-' => Numeric::Finite { int: x - y, scale: s },
                _ => unreachable!(),
            }
        };
        let st = std::collections::hash_map::RandomState::new();
        let h = |n: &Numeric| st.hash_one(n);
        for &a in &vals {
            for &b in &vals {
                let (na, nb) = (big(a.0, a.1), big(b.0, b.1));
                for op in ['+', '-'] {
                    let got = if op == '+' { na.add(&nb) } else { na.sub(&nb) };
                    let want = slow(a, b, op);
                    assert_eq!(format!("{got:?}"), format!("{want:?}"), "{a:?} {op} {b:?}");
                }
                let got = na.mul(&nb);
                let want = Numeric::Finite { int: DecInt::from(a.0) * DecInt::from(b.0), scale: a.1 + b.1 };
                assert_eq!(format!("{got:?}"), format!("{want:?}"), "{a:?} * {b:?}");
                let (x, y, _) = Numeric::aligned(&DecInt::from(a.0), a.1, &DecInt::from(b.0), b.1);
                assert_eq!(na.cmp(&nb), x.cmp(&y), "{a:?} cmp {b:?}");
                if na == nb {
                    assert_eq!(h(&na), h(&nb), "{a:?} hash {b:?}");
                }
            }
        }
        // A value past the i64 equal to a small one (trailing zeros) hashes as it does.
        let a = big(15, 1);
        let b = Numeric::Finite { int: DecInt::from(15) * DecInt::from(10).pow(30), scale: 31 };
        assert_eq!(a, b);
        assert_eq!(h(&a), h(&b));
        assert!(big(i64::MAX, 0).checked().is_ok());
        assert!(Numeric::Finite { int: DecInt::from(10).pow(200000), scale: 0 }.checked().is_err());
    }
}
