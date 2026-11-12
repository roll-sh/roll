//! DynamoDB numbers (`N`), as Alternator validates, stores and prints them.
//!
//! A number is a decimal of up to 38 significant digits whose magnitude is either zero or in
//! `[1e-130, 1e126)`. The grammar is `java.math.BigDecimal`'s (`+1`, `.5`, `5.`, `1E+2` are
//! numbers; ` 1`, `0x10`, `NaN` are not), as ScyllaDB's `big_decimal` parses it.
//!
//! Alternator keeps two forms of a number:
//!
//! - **Top-level attributes and keys** are stored as a CQL `decimal` (unscaled value and scale,
//!   exactly as parsed: `1.50` is `150 × 10^-2`) and printed back in ScyllaDB's decimal form
//!   (plain notation without trailing fractional zeros: `1.50` → `1.5`, `1E+2` → `100`).
//!   Because a partition key is identified by its bytes, `1.5` and `1.50` are different hash
//!   keys, while as sort keys they compare equal.
//! - **Numbers nested in `M`, `L` or `NS`** live inside a JSON document and keep the text the
//!   client sent.
//!
//! Comparison is always numeric.

use std::cmp::Ordering;

use nosql_types::Decimal;
use num_bigint::BigInt;
use num_traits::{Signed, Zero};

use crate::error::{Error, Result};

/// Maximum significant digits.
pub const MAX_PRECISION: usize = 38;
/// Largest allowed adjusted exponent (the magnitude is below `1e126`).
pub const MAX_EXPONENT: i64 = 125;
/// Smallest allowed adjusted exponent (the magnitude is at least `1e-130`).
pub const MIN_EXPONENT: i64 = -130;

/// A validated number, holding the text it was given as.
#[derive(Clone, Debug)]
pub struct Number {
    text: Box<str>,
    dec: Decimal,
}

impl PartialEq for Number {
    fn eq(&self, other: &Number) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Number {}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Number) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Number {
    fn cmp(&self, other: &Number) -> Ordering {
        self.dec.cmp_numeric(&other.dec)
    }
}

impl Number {
    /// Parses and validates a client-supplied number.
    pub fn parse(text: &str) -> Result<Number> {
        let dec = parse_decimal(text)?;
        check_range(&dec, text)?;
        Ok(Number {
            text: text.into(),
            dec,
        })
    }

    /// A number from a decimal computed by the server (arithmetic, decoding); printed in
    /// ScyllaDB's form.
    pub fn from_decimal(dec: Decimal) -> Number {
        Number {
            text: canonical_text(&dec).into(),
            dec,
        }
    }

    pub fn from_i64(v: i64) -> Number {
        Number::from_decimal(Decimal::from_i64(v))
    }

    /// The text as given (or as computed).
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn decimal(&self) -> &Decimal {
        &self.dec
    }

    /// The text this number reads back as once stored as a top-level attribute or key.
    pub fn canonical(&self) -> String {
        canonical_text(&self.dec)
    }

    /// The CQL `decimal` serialisation (scale, then the unscaled varint).
    pub fn to_decimal_bytes(&self) -> Vec<u8> {
        self.dec.to_bytes()
    }

    pub fn from_decimal_bytes(b: &[u8]) -> Option<Number> {
        Decimal::from_bytes(b).map(Number::from_decimal)
    }

    pub fn is_zero(&self) -> bool {
        self.dec.is_zero()
    }

    /// The value as an `i64`, if it is an integer that fits.
    pub fn to_i64(&self) -> Option<i64> {
        if self.dec.scale > 0 {
            let r = self.dec.to_bigint_trunc()?;
            let back = Decimal::new(r.clone() * pow10(self.dec.scale as u32), self.dec.scale);
            if back.cmp_numeric(&self.dec) != Ordering::Equal {
                return None;
            }
            return i64::try_from(r).ok();
        }
        if self.dec.scale < -20 && !self.dec.is_zero() {
            return None;
        }
        i64::try_from(self.dec.to_bigint_trunc()?).ok()
    }

    pub fn add(&self, other: &Number) -> Result<Number> {
        let (a, b) = aligned(&self.dec, &other.dec);
        let dec = Decimal::new(a.0 + b.0, a.1);
        let n = Number::from_decimal(dec);
        check_range(&n.dec, &n.text)?;
        Ok(n)
    }

    pub fn sub(&self, other: &Number) -> Result<Number> {
        let (a, b) = aligned(&self.dec, &other.dec);
        let dec = Decimal::new(a.0 - b.0, a.1);
        let n = Number::from_decimal(dec);
        check_range(&n.dec, &n.text)?;
        Ok(n)
    }
}

/// ScyllaDB's printed form of a decimal (zero is `0` whatever its scale).
fn canonical_text(dec: &Decimal) -> String {
    if dec.is_zero() {
        return "0".into();
    }
    dec.to_scylla_string()
}

fn pow10(k: u32) -> BigInt {
    num_traits::pow(BigInt::from(10u8), k as usize)
}

/// Both operands at the larger scale. Validated numbers have bounded exponents, so the
/// rescaling factor is at most 10^300 or so.
fn aligned(a: &Decimal, b: &Decimal) -> ((BigInt, i32), (BigInt, i32)) {
    let scale = a.scale.max(b.scale);
    let up = |d: &Decimal| {
        let k = (scale as i64 - d.scale as i64).clamp(0, 2000) as u32;
        d.unscaled.clone() * pow10(k)
    };
    ((up(a), scale), (up(b), scale))
}

fn not_numeric(text: &str) -> Error {
    Error::validation(format!(
        "The parameter cannot be converted to a numeric value: {text}"
    ))
}

fn parse_decimal(text: &str) -> Result<Decimal> {
    if let Some(d) = Decimal::parse(text) {
        return Ok(d);
    }
    // `Decimal::parse` limits the exponent's digits; leading zeros don't count here, only
    // that the resulting scale fits an `i32`.
    let fail = || not_numeric(text);
    let (mant, exp) = text.split_once(['e', 'E']).ok_or_else(fail)?;
    let m = Decimal::parse(mant)
        .filter(|_| !mant.contains(['e', 'E']))
        .ok_or_else(fail)?;
    let (neg, digits) = match exp.as_bytes().first() {
        Some(b'-') => (true, &exp[1..]),
        Some(b'+') => (false, &exp[1..]),
        _ => (false, exp),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(fail());
    }
    let significant = digits.trim_start_matches('0');
    if significant.len() > 10 {
        return Err(fail());
    }
    let e: i64 = if significant.is_empty() {
        0
    } else {
        significant.parse().map_err(|_| fail())?
    };
    let e = if neg { -e } else { e };
    let scale = i32::try_from(m.scale as i64 - e).map_err(|_| fail())?;
    Ok(Decimal::new(m.unscaled, scale))
}

/// Significant digits and the adjusted exponent of a nonzero decimal (trailing zeros of the
/// unscaled value don't count).
fn shape(dec: &Decimal) -> (usize, i64) {
    let digits = dec.unscaled.abs().to_string();
    let trimmed = digits.trim_end_matches('0');
    let precision = trimmed.len().max(1);
    let adjusted = digits.len() as i64 - 1 - dec.scale as i64;
    (precision, adjusted)
}

fn check_range(dec: &Decimal, text: &str) -> Result<()> {
    if dec.unscaled.is_zero() {
        return Ok(());
    }
    let overflow = || {
        Error::validation(format!(
            "Number overflow: {text}. Attempting to store a number with magnitude larger than supported range."
        ))
    };
    let underflow = || {
        Error::validation(format!(
            "Number underflow: {text}. Attempting to store a number with magnitude lower than supported range."
        ))
    };
    // ScyllaDB rejects any exponent written with more than four characters (sign included),
    // whatever its value: `1e00001` overflows, `1e-0001` underflows.
    if let Some(exp) = text.split_once(['e', 'E']).map(|(_, e)| e)
        && exp.len() > 4
    {
        return Err(if exp.starts_with('-') {
            underflow()
        } else {
            overflow()
        });
    }
    let (precision, adjusted) = shape(dec);
    if adjusted > MAX_EXPONENT {
        return Err(overflow());
    }
    if adjusted < MIN_EXPONENT {
        return Err(underflow());
    }
    if precision > MAX_PRECISION {
        return Err(Error::validation(format!(
            "Number too precise: {text}. Attempting to store a number with more significant digits than supported."
        )));
    }
    Ok(())
}

impl std::fmt::Display for Number {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl Number {
    /// Whether the value is negative.
    pub fn is_negative(&self) -> bool {
        self.dec.unscaled.is_negative()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(s: &str) -> String {
        Number::parse(s).unwrap_err().message
    }

    #[test]
    fn grammar() {
        for ok in ["1", "-0", "+1", ".5", "5.", "1E+2", "1e5", "00012", "1.50"] {
            assert!(Number::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "abc", "", " 1", "0x10", "NaN", "Infinity", "1e", "--1", "1_0",
        ] {
            assert_eq!(
                msg(bad),
                format!("The parameter cannot be converted to a numeric value: {bad}")
            );
        }
    }

    #[test]
    fn limits() {
        assert!(Number::parse("9.9999999999999999999999999999999999999e125").is_ok());
        assert_eq!(
            msg("1e126"),
            "Number overflow: 1e126. Attempting to store a number with magnitude larger than supported range."
        );
        assert!(Number::parse("1e-130").is_ok());
        assert_eq!(
            msg("1e-131"),
            "Number underflow: 1e-131. Attempting to store a number with magnitude lower than supported range."
        );
        assert!(Number::parse("12345678901234567890123456789012345678").is_ok());
        assert!(Number::parse("1234567890123456789012345678901234567800000").is_ok());
        assert_eq!(
            msg("123456789012345678901234567890123456789"),
            "Number too precise: 123456789012345678901234567890123456789. Attempting to store a number with more significant digits than supported."
        );
        assert!(Number::parse("0e-1000").is_ok());
    }

    #[test]
    fn canonical_and_arithmetic() {
        let c = |s: &str| Number::parse(s).unwrap().canonical();
        assert_eq!(c("1.50"), "1.5");
        assert_eq!(c("-12.3e5"), "-1230000");
        assert_eq!(c("1E+2"), "100");
        assert_eq!(c("-0"), "0");
        assert_eq!(c(".5"), "0.5");
        assert_eq!(c("5."), "5");
        let a = Number::parse("1.5").unwrap();
        let b = Number::parse("2.25").unwrap();
        assert_eq!(a.add(&b).unwrap().text(), "3.75");
        assert_eq!(a.sub(&b).unwrap().text(), "-0.75");
        assert_eq!(Number::parse("1.0").unwrap(), Number::parse("1").unwrap());
        assert!(Number::parse("2").unwrap() > Number::parse("10e-1").unwrap());
        assert_eq!(Number::parse("12").unwrap().to_i64(), Some(12));
        assert_eq!(Number::parse("1.2e1").unwrap().to_i64(), Some(12));
        assert_eq!(Number::parse("1.5").unwrap().to_i64(), None);
    }
}
