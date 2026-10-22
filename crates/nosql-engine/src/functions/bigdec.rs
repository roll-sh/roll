//! Arbitrary-precision decimal arithmetic for Cassandra 5's `exp` / `log` / `log10` on `decimal`
//! and `varint` (`DecimalType`, `IntegerType`).
//!
//! Cassandra computes them with `ch.obermuhlner.math.big.BigDecimalMath` 2.3.0 (MIT) on
//! `java.math.BigDecimal`. The visible result (its digits *and* its scale: `log10(1)` is `0E+33`,
//! `log10(1E+10)` is `1E+1`) depends on every intermediate rounding, so this is a port of the
//! library's algorithms (`exp`, `expIntegralFractional`, `expTaylor`, the `ExpCalculator` series,
//! `pow`, `log`, `logUsingExponent`, `logUsingTwoThree`, `logUsingNewton`, `log10`) over a
//! [`Dec`] that follows `BigDecimal`'s documented semantics for the operations they use: `round`,
//! `add` / `multiply` / `divide` with a `MathContext` (preferred scales, trailing zeros stripped
//! from exact quotients), `setScale`, `movePointLeft`. Rounding is `HALF_EVEN`, as `DecimalType`
//! asks.
//!
//! The library caches `ln 10`, `ln 2` and `ln 3` at the highest precision ever asked and rounds
//! the cache; a long-running node always has it, so the constants here are computed with guard
//! digits and rounded the same way.

use std::cmp::Ordering;

use num_bigint::{BigInt, Sign};
use num_traits::{One, Signed, ToPrimitive, Zero};

/// `ArithmeticException` messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArithError(pub &'static str);

pub type R<T> = std::result::Result<T, ArithError>;

const OVERFLOW: ArithError = ArithError("Overflow");

/// `java.math.BigDecimal`: `unscaled × 10^-scale`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dec {
    pub unscaled: BigInt,
    pub scale: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    HalfEven,
    HalfUp,
    Down,
}

fn pow10(n: u64) -> BigInt {
    num_traits::pow(BigInt::from(10u8), n as usize)
}

/// Decimal digits of `|v|` (0 has one).
fn digits(v: &BigInt) -> u64 {
    if v.is_zero() {
        return 1;
    }
    let bits = v.bits();
    // log10(2) ≈ 0.30103: estimate, then correct by comparison.
    let mut d = (bits as f64 * std::f64::consts::LOG10_2) as u64;
    let a = v.abs();
    if d == 0 {
        d = 1;
    }
    while a >= pow10(d) {
        d += 1;
    }
    while d > 1 && a < pow10(d - 1) {
        d -= 1;
    }
    d
}

/// `num / den` rounded to an integer.
fn div_round(num: &BigInt, den: &BigInt, mode: Mode) -> (BigInt, bool) {
    let (q, r) = (num / den, num % den);
    if r.is_zero() {
        return (q, true);
    }
    let negative = (num.sign() == Sign::Minus) != (den.sign() == Sign::Minus);
    let away = match mode {
        Mode::Down => false,
        Mode::HalfUp | Mode::HalfEven => {
            let twice = (r.abs() * 2u8).cmp(&den.abs());
            match twice {
                Ordering::Greater => true,
                Ordering::Less => false,
                Ordering::Equal => mode == Mode::HalfUp || (&q % 2u8) != BigInt::zero(),
            }
        }
    };
    let q = if away {
        if negative { q - 1 } else { q + 1 }
    } else {
        q
    };
    (q, false)
}

impl Dec {
    pub fn new(unscaled: BigInt, scale: i64) -> Dec {
        Dec { unscaled, scale }
    }

    pub fn from_i64(v: i64) -> Dec {
        Dec::new(BigInt::from(v), 0)
    }

    pub fn zero() -> Dec {
        Dec::from_i64(0)
    }

    pub fn one() -> Dec {
        Dec::from_i64(1)
    }

    /// `BigDecimal.valueOf(double)`: the decimal of `Double.toString`.
    pub fn from_f64(v: f64) -> Dec {
        let d = nosql_types::Decimal::parse(&nosql_types::float::java_double(v))
            .unwrap_or_else(|| nosql_types::Decimal::new(BigInt::zero(), 0));
        Dec::new(d.unscaled, d.scale as i64)
    }

    pub fn signum(&self) -> i32 {
        match self.unscaled.sign() {
            Sign::Minus => -1,
            Sign::NoSign => 0,
            Sign::Plus => 1,
        }
    }

    pub fn precision(&self) -> u64 {
        digits(&self.unscaled)
    }

    pub fn negate(&self) -> Dec {
        Dec::new(-&self.unscaled, self.scale)
    }

    pub fn abs(&self) -> Dec {
        Dec::new(self.unscaled.abs(), self.scale)
    }

    /// Unscaled values at a common scale.
    fn aligned(&self, o: &Dec) -> (BigInt, BigInt, i64) {
        let s = self.scale.max(o.scale);
        let a = &self.unscaled * pow10((s - self.scale) as u64);
        let b = &o.unscaled * pow10((s - o.scale) as u64);
        (a, b, s)
    }

    pub fn compare(&self, o: &Dec) -> Ordering {
        let (a, b, _) = self.aligned(o);
        a.cmp(&b)
    }

    pub fn is_one(&self) -> bool {
        self.compare(&Dec::one()) == Ordering::Equal
    }

    /// Exact `add`.
    pub fn add(&self, o: &Dec) -> Dec {
        let (a, b, s) = self.aligned(o);
        Dec::new(a + b, s)
    }

    pub fn sub(&self, o: &Dec) -> Dec {
        self.add(&o.negate())
    }

    /// Exact `multiply`.
    pub fn mul(&self, o: &Dec) -> Dec {
        Dec::new(&self.unscaled * &o.unscaled, self.scale + o.scale)
    }

    /// `round(MathContext)` (`doRound`).
    pub fn round(&self, prec: u64, mode: Mode) -> Dec {
        let mut v = self.clone();
        loop {
            let p = v.precision();
            if p <= prec {
                return v;
            }
            let drop = p - prec;
            let (q, _) = div_round(&v.unscaled, &pow10(drop), mode);
            v = Dec::new(q, v.scale - drop as i64);
        }
    }

    /// `add(augend, MathContext)`.
    pub fn add_mc(&self, o: &Dec, prec: u64, mode: Mode) -> Dec {
        let preferred = self.scale.max(o.scale);
        let (lz, rz) = (self.signum() == 0, o.signum() == 0);
        if lz || rz {
            if lz && rz {
                return Dec::new(BigInt::zero(), preferred);
            }
            let r = if lz {
                o.round(prec, mode)
            } else {
                self.round(prec, mode)
            };
            return match r.scale.cmp(&preferred) {
                Ordering::Equal => r,
                Ordering::Greater => r.strip_to(preferred),
                Ordering::Less => {
                    let room = prec as i64 - r.precision() as i64;
                    let want = preferred - r.scale;
                    r.set_scale_up(r.scale + room.min(want).max(0))
                }
            };
        }
        self.add(o).round(prec, mode)
    }

    /// `multiply(multiplicand, MathContext)`.
    pub fn mul_mc(&self, o: &Dec, prec: u64, mode: Mode) -> Dec {
        self.mul(o).round(prec, mode)
    }

    /// Strips trailing zeros while the scale is above `preferred`.
    fn strip_to(&self, preferred: i64) -> Dec {
        let mut v = self.clone();
        let ten = BigInt::from(10u8);
        while v.scale > preferred && !v.unscaled.is_zero() && (&v.unscaled % &ten).is_zero() {
            v.unscaled /= &ten;
            v.scale -= 1;
        }
        v
    }

    fn set_scale_up(&self, scale: i64) -> Dec {
        if scale <= self.scale {
            return self.clone();
        }
        Dec::new(&self.unscaled * pow10((scale - self.scale) as u64), scale)
    }

    /// `setScale(n, mode)`.
    pub fn set_scale(&self, scale: i64, mode: Mode) -> Dec {
        if scale >= self.scale {
            return self.set_scale_up(scale);
        }
        let (q, _) = div_round(&self.unscaled, &pow10((self.scale - scale) as u64), mode);
        Dec::new(q, scale)
    }

    /// `divide(divisor, MathContext)`: the quotient to `prec` digits; an exact one keeps the
    /// scale closest to `this.scale - divisor.scale`.
    pub fn div_mc(&self, o: &Dec, prec: u64, mode: Mode) -> R<Dec> {
        if o.signum() == 0 {
            return Err(ArithError("Division by zero"));
        }
        let preferred = self.scale - o.scale;
        if self.signum() == 0 {
            return Ok(Dec::new(BigInt::zero(), preferred));
        }
        let xp = self.precision() as i64;
        let mut yp = o.precision() as i64;
        // Normalised mantissas (0.d1d2…): when the dividend's is larger the quotient has one more
        // integer digit.
        let (xa, ya) = (self.unscaled.abs(), o.unscaled.abs());
        let cmp = if xp >= yp {
            xa.cmp(&(&ya * pow10((xp - yp) as u64)))
        } else {
            (&xa * pow10((yp - xp) as u64)).cmp(&ya)
        };
        if cmp == Ordering::Greater {
            yp -= 1;
        }
        let scl = preferred + yp - xp + prec as i64;
        let e = scl - preferred;
        let (num, den) = if e >= 0 {
            (&self.unscaled * pow10(e as u64), o.unscaled.clone())
        } else {
            (self.unscaled.clone(), &o.unscaled * pow10((-e) as u64))
        };
        let (q, exact) = div_round(&num, &den, mode);
        let mut q = Dec::new(q, scl);
        if exact {
            q = q.strip_to(preferred);
        }
        Ok(q.round(prec, mode))
    }

    /// `movePointLeft(n)`: scale `max(scale + n, 0)`.
    pub fn move_point_left(&self, n: i64) -> Dec {
        let v = Dec::new(self.unscaled.clone(), self.scale + n);
        if v.scale < 0 { v.set_scale_up(0) } else { v }
    }

    /// `toBigInteger()` (truncates).
    pub fn to_bigint(&self) -> BigInt {
        if self.scale <= 0 {
            &self.unscaled * pow10((-self.scale) as u64)
        } else {
            &self.unscaled / pow10(self.scale as u64)
        }
    }

    /// `doubleValue()`.
    pub fn to_f64(&self) -> f64 {
        format!("{}e{}", self.unscaled, -self.scale)
            .parse()
            .unwrap_or(f64::NAN)
    }

    /// The `decimal` it serialises as (scales outside `int` overflow, as Java checks them).
    pub fn to_decimal(&self) -> R<nosql_types::Decimal> {
        let scale = i32::try_from(self.scale).map_err(|_| {
            if self.scale > 0 {
                ArithError("Underflow")
            } else {
                OVERFLOW
            }
        })?;
        Ok(nosql_types::Decimal::new(self.unscaled.clone(), scale))
    }

    pub fn from_decimal(d: &nosql_types::Decimal) -> Dec {
        Dec::new(d.unscaled.clone(), d.scale as i64)
    }
}

/// The precision of a `DecimalType` operation: the input's, within [32, 10000].
pub fn precision_for(x: &Dec) -> u64 {
    x.precision().clamp(32, 10_000)
}

/// Largest exponent a computed `exp` may have (`e^x` with `x` near `Integer.MAX_VALUE` has about
/// 9.3e8 decimal digits; materialising that as a varint would need gigabytes).
pub const MAX_VARINT_EXP_DIGITS: i64 = 1_000_000;

// ---- BigDecimalMath ---------------------------------------------------------------------------

/// `BigDecimalMath.exp(x, mc)`.
pub fn exp(x: &Dec, prec: u64) -> R<Dec> {
    if x.signum() == 0 {
        return Ok(Dec::one());
    }
    exp_integral_fractional(x, prec)
}

fn exp_integral_fractional(x: &Dec, prec: u64) -> R<Dec> {
    let m = Mode::HalfEven;
    let integral = x.set_scale(0, Mode::Down);
    if integral.signum() == 0 {
        return exp_taylor(x, prec);
    }
    let fractional = x.sub(&integral);
    let p10 = prec + 10;
    let z = Dec::one().add(&fractional.div_mc(&integral, p10, m)?);
    let t = exp_taylor(&z, p10)?;
    // `intValueExact`.
    let n = integral
        .unscaled
        .to_i32()
        .filter(|_| integral.scale == 0)
        .ok_or(OVERFLOW)?;
    let r = pow(&t, n as i64, p10)?;
    Ok(r.round(prec, m))
}

fn exp_taylor(x: &Dec, prec: u64) -> R<Dec> {
    let m = Mode::HalfEven;
    let p6 = prec + 6;
    let x = x.div_mc(&Dec::from_i64(256), p6, m)?;
    let r = exp_series(&x, p6)?;
    let r = pow(&r, 256, p6)?;
    Ok(r.round(prec, m))
}

/// `ExpCalculator` through `SeriesCalculator.calculate`: Σ xⁿ/n! until a step is below
/// 10^-(precision+1).
fn exp_series(x: &Dec, prec: u64) -> R<Dec> {
    let m = Mode::HalfEven;
    let acceptable = Dec::one().move_point_left(prec as i64 + 1);
    let mut power = Dec::one();
    let mut factorial = BigInt::one();
    let mut sum = Dec::zero();
    let mut n: u64 = 0;
    loop {
        if n >= 2 {
            factorial *= n;
        }
        let xp = power.clone();
        power = power.mul_mc(x, prec, m);
        // numerator ONE × x^n, divided by n!.
        let step = Dec::one()
            .mul(&xp)
            .div_mc(&Dec::new(factorial.clone(), 0), prec, m)?;
        n += 1;
        sum = sum.add(&step);
        if step.abs().compare(&acceptable) != Ordering::Greater {
            break;
        }
        if n > 1_000_000 {
            return Err(OVERFLOW);
        }
    }
    Ok(sum.round(prec, m))
}

/// `BigDecimalMath.pow(x, long y, mc)`.
fn pow(x: &Dec, y: i64, prec: u64) -> R<Dec> {
    let m = Mode::HalfEven;
    let p10 = prec + 10;
    if y < 0 {
        let v = pow(x, -y, p10)?;
        let r = Dec::one().div_mc(&v, p10, m)?;
        return Ok(r.round(prec, m));
    }
    let mut y = y;
    let mut x = x.clone();
    let mut result = Dec::one();
    while y > 0 {
        if y & 1 == 1 {
            result = result.mul_mc(&x, p10, m);
            y -= 1;
        }
        if y > 0 {
            x = x.mul_mc(&x, p10, m);
        }
        y >>= 1;
    }
    Ok(result.round(prec, m))
}

/// `BigDecimalMath.log(x, mc)` (`x > 0`).
pub fn log(x: &Dec, prec: u64) -> R<Dec> {
    if x.signum() <= 0 {
        return Err(ArithError("Illegal log(x) for x <= 0"));
    }
    if x.is_one() {
        return Ok(Dec::zero());
    }
    let ten = Dec::from_i64(10);
    let r = match x.compare(&ten) {
        Ordering::Equal => log_ten(prec)?,
        Ordering::Greater => log_using_exponent(x, prec)?,
        Ordering::Less => log_using_two_three(x, prec)?,
    };
    Ok(r.round(prec, Mode::HalfEven))
}

/// `BigDecimalMath.log10(x, mc)`.
pub fn log10(x: &Dec, prec: u64) -> R<Dec> {
    let p2 = prec + 2;
    let r = log(x, p2)?.div_mc(&log_ten(p2)?, p2, Mode::HalfEven)?;
    Ok(r.round(prec, Mode::HalfEven))
}

/// The cached constants: computed with guard digits, rounded to the precision asked.
fn constant(v: i64, prec: u64) -> R<Dec> {
    Ok(log_using_newton(&Dec::from_i64(v), prec + 10)?.round(prec, Mode::HalfEven))
}

fn log_ten(prec: u64) -> R<Dec> {
    constant(10, prec)
}

fn log_two(prec: u64) -> R<Dec> {
    constant(2, prec)
}

fn log_three(prec: u64) -> R<Dec> {
    constant(3, prec)
}

/// `Double.MAX_VALUE` bounds `isDoubleValue`.
fn is_double_value(x: &Dec) -> bool {
    x.to_f64().is_finite()
}

fn log_using_newton(x: &Dec, prec: u64) -> R<Dec> {
    let m = Mode::HalfEven;
    let max_precision = prec + 20;
    let acceptable = Dec::one().move_point_left(prec as i64 + 1);
    let dx = x.to_f64();
    let (mut result, mut adaptive) = if dx > 0.0 && is_double_value(x) {
        (Dec::from_f64(dx.ln()), 15u64)
    } else {
        (x.div_mc(&Dec::from_i64(2), prec, m)?, 1u64)
    };
    let two = Dec::from_i64(2);
    let mut iterations = 0;
    loop {
        adaptive = (adaptive * 3).min(max_precision);
        let exp_y = exp(&result, adaptive)?;
        let step = two
            .mul(&x.sub(&exp_y))
            .div_mc(&x.add(&exp_y), adaptive, m)?;
        result = result.add(&step);
        iterations += 1;
        if !(adaptive < max_precision || step.abs().compare(&acceptable) == Ordering::Greater) {
            break;
        }
        if iterations > 10_000 {
            return Err(OVERFLOW);
        }
    }
    Ok(result)
}

fn exponent(x: &Dec) -> i64 {
    x.precision() as i64 - x.scale - 1
}

fn mantissa(x: &Dec) -> Dec {
    let e = exponent(x);
    if e == 0 {
        x.clone()
    } else {
        x.move_point_left(e)
    }
}

fn log_using_exponent(x: &Dec, prec: u64) -> R<Dec> {
    let m = Mode::HalfEven;
    let p_double = prec * 2;
    let p4 = prec + 4;
    let e = exponent(x);
    let mant = mantissa(x);
    let mut r = log_using_two_three(&mant, p4)?;
    if e != 0 {
        r = r.add(&Dec::from_i64(e).mul_mc(&log_ten(p_double)?, p4, m));
    }
    Ok(r)
}

fn log_using_two_three(x: &Dec, prec: u64) -> R<Dec> {
    let m = Mode::HalfEven;
    let p_double = prec * 2;
    let p4 = prec + 4;
    let (mut f2, mut p2, mut f3, mut p3) = (0i64, 1i64, 0i64, 1i64);
    let mut value = x.to_f64();
    if value < 0.01 {
    } else if value < 0.1 {
        while value < 0.6 {
            value *= 2.0;
            f2 -= 1;
            p2 <<= 1;
        }
    } else if value < 0.115 {
        (f3, p3) = (-2, 9);
    } else if value < 0.14 {
        (f2, p2) = (-3, 8);
    } else if value < 0.2 {
        (f2, p2, f3, p3) = (-1, 2, -1, 3);
    } else if value < 0.3 {
        (f2, p2) = (-2, 4);
    } else if value < 0.42 {
        (f3, p3) = (-1, 3);
    } else if value < 0.7 {
        (f2, p2) = (-1, 2);
    } else if value < 1.4 {
    } else if value < 2.5 {
        (f2, p2) = (1, 2);
    } else if value < 3.5 {
        (f3, p3) = (1, 3);
    } else if value < 5.0 {
        (f2, p2) = (2, 4);
    } else if value < 7.0 {
        (f2, p2, f3, p3) = (1, 2, 1, 3);
    } else if value < 8.5 {
        (f2, p2) = (3, 8);
    } else if value < 10.0 {
        (f3, p3) = (2, 9);
    } else {
        while value > 1.4 {
            value /= 2.0;
            f2 += 1;
            p2 <<= 1;
            if f2 > 4096 {
                return Err(OVERFLOW);
            }
        }
    }
    if f2 == 0 && f3 == 0 {
        return log_using_newton(x, prec);
    }
    let mut corrected = x.clone();
    let mut result = Dec::zero();
    if f2 > 0 {
        corrected = corrected.div_mc(&Dec::from_i64(p2), p4, m)?;
        result = result.add(&log_two(p_double)?.mul_mc(&Dec::from_i64(f2), p4, m));
    } else if f2 < 0 {
        corrected = corrected.mul_mc(&Dec::from_i64(p2), p4, m);
        result = result.sub(&log_two(p_double)?.mul_mc(&Dec::from_i64(-f2), p4, m));
    }
    if f3 > 0 {
        corrected = corrected.div_mc(&Dec::from_i64(p3), p4, m)?;
        result = result.add(&log_three(p_double)?.mul_mc(&Dec::from_i64(f3), p4, m));
    } else if f3 < 0 {
        corrected = corrected.mul_mc(&Dec::from_i64(p3), p4, m);
        result = result.sub(&log_three(p_double)?.mul_mc(&Dec::from_i64(-f3), p4, m));
    }
    Ok(result.add_mc(&log_using_newton(&corrected, p4)?, p4, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Dec {
        Dec::from_decimal(&nosql_types::Decimal::parse(s).unwrap())
    }

    fn s(v: &Dec) -> String {
        v.to_decimal().unwrap().to_java_string()
    }

    #[test]
    fn big_decimal_semantics() {
        let m = Mode::HalfEven;
        assert_eq!(s(&d("1").div_mc(&d("3"), 5, m).unwrap()), "0.33333");
        assert_eq!(s(&d("6").div_mc(&d("3"), 5, m).unwrap()), "2");
        assert_eq!(s(&d("3.000").div_mc(&d("3"), 5, m).unwrap()), "1.000");
        assert_eq!(s(&d("0").div_mc(&d("3.5"), 5, m).unwrap()), "0E+1");
        assert_eq!(s(&d("2.5").round(1, m)), "2");
        assert_eq!(s(&d("3.5").round(1, m)), "4");
        assert_eq!(s(&d("999.96").round(4, m)), "1000");
        assert_eq!(s(&d("-2.5").set_scale(0, Mode::HalfUp)), "-3");
        assert_eq!(s(&d("1").move_point_left(3)), "0.001");
    }

    /// Values observed on `cassandra:5.0.9` (`CAST(f(de) AS text)`).
    #[test]
    fn matches_the_reference() {
        let cases = [
            ("exp", "-2.5", "0.082084998623898795169528674467160"),
            ("exp", "2.5", "12.182493960703473438070175951168"),
            ("exp", "1", "2.7182818284590452353602874713527"),
            ("exp", "10", "22026.465794806716516957900645284"),
            ("exp", "100", "2.6881171418161354484126255515800E+43"),
            ("exp", "0.001", "1.0010005001667083416680557539931"),
            ("exp", "1000.5", "3.2480981504718027596309675316942E+434"),
            ("exp", "-1000.5", "3.0787246988048834641772868393638E-435"),
            (
                "exp",
                "12.345678901234567890123456789012345678901234567890",
                "229964.19485298854521264777192887338471140368572850",
            ),
            ("log", "100", "4.6051701859880913680359829093687"),
            ("log", "1", "0"),
            ("log", "10", "2.3025850929940456840179914546844"),
            ("log", "1000.5", "6.9082551540237880999680379796365"),
            ("log", "0.001", "-6.9077552789821370520539743640531"),
            ("log", "1E+10", "23.025850929940456840179914546844"),
            (
                "log",
                "12.345678901234567890123456789012345678901234567890",
                "2.5133061153096982450729931651729880898393297539523",
            ),
            ("log10", "100", "2"),
            ("log10", "1", "0E+33"),
            ("log10", "10", "1"),
            ("log10", "1000.5", "3.0002170929722302082819128837511"),
            ("log10", "0.001", "-3.0000000000000000000000000000000"),
            ("log10", "1E+10", "1E+1"),
            (
                "log10",
                "12.345678901234567890123456789012345678901234567890",
                "1.0915149772126998957108302782343211743723014330733",
            ),
        ];
        for (f, x, want) in cases {
            let x = d(x);
            let p = precision_for(&x);
            let got = match f {
                "exp" => exp(&x, p),
                "log" => log(&x, p),
                _ => log10(&x, p),
            }
            .unwrap();
            assert_eq!(s(&got), want, "{f}({})", s(&x));
        }
        assert_eq!(exp(&d("1E+10"), 32), Err(OVERFLOW));
    }
}
