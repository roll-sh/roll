//! `varint` and `decimal`: arbitrary-precision integers and decimals.
//!
//! A varint is a two's-complement big-endian integer of any length (Java's
//! `BigInteger.toByteArray`); a decimal is `[i32 scale][varint unscaled]` meaning
//! `unscaled × 10^-scale` (`BigDecimal`). Parsing and `to_java_string` follow
//! `java.math.BigDecimal`'s grammar and canonical string form, as Cassandra's `DecimalType` uses
//! them. ScyllaDB prints decimals in plain notation without trailing fractional zeros.
//!
//! Comparison is numeric (`1.0 == 1.00`), allocation-free for varints and for decimals whose
//! unscaled values fit 128 bits.

use std::cmp::Ordering;

use num_bigint::{BigInt, Sign};
use num_traits::{Signed, ToPrimitive, Zero};

/// Longest run of zeros a plain-notation decimal may expand to before we fall back to scientific
/// notation; a peer-supplied scale of ±2^31 must not allocate gigabytes.
const MAX_PLAIN_ZEROS: u64 = 1 << 20;

/// A decoded `decimal`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Decimal {
    pub unscaled: BigInt,
    pub scale: i32,
}

/// Decodes a varint (empty = 0).
pub fn varint_from_bytes(b: &[u8]) -> BigInt {
    BigInt::from_signed_bytes_be(b)
}

/// Minimal two's-complement encoding (`BigInteger.toByteArray`; zero is `[0]`).
pub fn varint_to_bytes(v: &BigInt) -> Vec<u8> {
    v.to_signed_bytes_be()
}

/// Parses a base-10 integer as `new BigInteger(String)` does: optional sign, digits only.
pub fn parse_varint(s: &str) -> Option<BigInt> {
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<BigInt>().ok()
}

impl Decimal {
    pub fn new(unscaled: BigInt, scale: i32) -> Decimal {
        Decimal { unscaled, scale }
    }

    pub fn from_i64(v: i64) -> Decimal {
        Decimal {
            unscaled: BigInt::from(v),
            scale: 0,
        }
    }

    /// `None` if shorter than the 4-byte scale.
    pub fn from_bytes(b: &[u8]) -> Option<Decimal> {
        let scale = crate::wire::i32_at(b, 0)?;
        Some(Decimal {
            unscaled: BigInt::from_signed_bytes_be(&b[4..]),
            scale,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.scale.to_be_bytes().to_vec();
        out.extend_from_slice(&self.unscaled.to_signed_bytes_be());
        out
    }

    /// `new BigDecimal(String)`: `[+-]digits[.digits][e[+-]digits]` (or `.digits`), the scale
    /// being the fraction digits minus the exponent.
    pub fn parse(s: &str) -> Option<Decimal> {
        let (neg, body) = match s.as_bytes().first()? {
            b'-' => (true, &s[1..]),
            b'+' => (false, &s[1..]),
            _ => (false, s),
        };
        let (mant, exp) = match body.find(['e', 'E']) {
            Some(i) => (&body[..i], Some(&body[i + 1..])),
            None => (body, None),
        };
        let (int, frac) = match mant.find('.') {
            Some(i) => (&mant[..i], &mant[i + 1..]),
            None => (mant, ""),
        };
        if int.is_empty() && frac.is_empty() {
            return None;
        }
        if !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
            return None;
        }
        let exp: i64 = match exp {
            Some(e) => {
                let digits = e.strip_prefix(['-', '+']).unwrap_or(e);
                if digits.is_empty()
                    || !digits.bytes().all(|b| b.is_ascii_digit())
                    || digits.len() > 11
                {
                    return None;
                }
                e.parse().ok()?
            }
            None => 0,
        };
        let scale = frac.len() as i64 - exp;
        let scale = i32::try_from(scale).ok()?;
        let mut digits = String::with_capacity(int.len() + frac.len() + 1);
        if neg {
            digits.push('-');
        }
        digits.push_str(int);
        digits.push_str(frac);
        Some(Decimal {
            unscaled: digits.parse().ok()?,
            scale,
        })
    }

    pub fn is_zero(&self) -> bool {
        self.unscaled.is_zero()
    }

    /// `BigDecimal.toString()`: plain when the scale is non-negative and the adjusted exponent is
    /// at least -6, otherwise one digit, a point and `E±n`.
    pub fn to_java_string(&self) -> String {
        let coeff = self.unscaled.magnitude().to_string();
        let neg = self.unscaled.sign() == Sign::Minus;
        let adjusted = -(self.scale as i64) + (coeff.len() as i64 - 1);
        let mut out = String::with_capacity(coeff.len() + 16);
        if neg {
            out.push('-');
        }
        if self.scale == 0 {
            out.push_str(&coeff);
        } else if self.scale > 0 && adjusted >= -6 {
            let scale = self.scale as usize;
            if coeff.len() > scale {
                let (i, f) = coeff.split_at(coeff.len() - scale);
                out.push_str(i);
                out.push('.');
                out.push_str(f);
            } else {
                out.push_str("0.");
                out.extend(std::iter::repeat_n('0', scale - coeff.len()));
                out.push_str(&coeff);
            }
        } else {
            out.push_str(&coeff[..1]);
            if coeff.len() > 1 {
                out.push('.');
                out.push_str(&coeff[1..]);
            }
            out.push('E');
            if adjusted > 0 {
                out.push('+');
            }
            out.push_str(&adjusted.to_string());
        }
        out
    }

    /// Plain notation with the scale's trailing zeros (`BigDecimal.toPlainString`), or the Java
    /// scientific form when that would expand past `MAX_PLAIN_ZEROS` zeros.
    pub fn to_plain_string(&self) -> String {
        let coeff = self.unscaled.magnitude().to_string();
        if self.scale.unsigned_abs() as u64 > MAX_PLAIN_ZEROS {
            return self.to_java_string();
        }
        let mut out = String::with_capacity(coeff.len() + self.scale.unsigned_abs() as usize + 3);
        if self.unscaled.sign() == Sign::Minus {
            out.push('-');
        }
        if self.scale <= 0 {
            out.push_str(&coeff);
            if !self.unscaled.is_zero() {
                out.extend(std::iter::repeat_n('0', self.scale.unsigned_abs() as usize));
            }
        } else {
            let scale = self.scale as usize;
            if coeff.len() > scale {
                let (i, f) = coeff.split_at(coeff.len() - scale);
                out.push_str(i);
                out.push('.');
                out.push_str(f);
            } else {
                out.push_str("0.");
                out.extend(std::iter::repeat_n('0', scale - coeff.len()));
                out.push_str(&coeff);
            }
        }
        out
    }

    /// ScyllaDB's form: plain notation, trailing fractional zeros and a bare point removed
    /// (`1.10` → `1.1`, `1E+3` → `1000`, `-0.00` → `0`).
    pub fn to_scylla_string(&self) -> String {
        let mut s = self.to_plain_string();
        if s.contains('E') {
            return s;
        }
        if s.contains('.') {
            while s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
        }
        if s == "-0" {
            s = "0".into();
        }
        s
    }

    /// Correctly rounded conversion (`BigDecimal.doubleValue`).
    pub fn to_f64(&self) -> f64 {
        format!("{}e{}", self.unscaled, -(self.scale as i64))
            .parse()
            .unwrap_or(f64::NAN)
    }

    pub fn to_f32(&self) -> f32 {
        format!("{}e{}", self.unscaled, -(self.scale as i64))
            .parse()
            .unwrap_or(f32::NAN)
    }

    /// The integer part, truncated toward zero (`BigDecimal.toBigInteger`). `None` when a
    /// negative scale would make it absurdly large.
    pub fn to_bigint_trunc(&self) -> Option<BigInt> {
        if self.scale <= 0 {
            let k = self.scale.unsigned_abs() as u64;
            if k > MAX_PLAIN_ZEROS && !self.unscaled.is_zero() {
                return None;
            }
            Some(&self.unscaled * pow10(k as u32))
        } else {
            let digits = self.unscaled.magnitude().bits() as f64 * std::f64::consts::LOG10_2;
            if (self.scale as f64) > digits + 1.0 {
                return Some(BigInt::zero());
            }
            Some(&self.unscaled / pow10(self.scale as u32))
        }
    }

    /// The low 64 bits of the truncated integer part (`BigDecimal.longValue`), computed without
    /// materialising huge powers of ten.
    pub fn to_i64_wrapping(&self) -> i64 {
        if self.scale <= 0 {
            let k = self.scale.unsigned_abs();
            // unscaled * 10^k mod 2^64.
            let low = bigint_low_u64(&self.unscaled);
            let mut p: u64 = 1;
            let mut base: u64 = 10;
            let mut e = k;
            while e > 0 {
                if e & 1 == 1 {
                    p = p.wrapping_mul(base);
                }
                base = base.wrapping_mul(base);
                e >>= 1;
            }
            low.wrapping_mul(p) as i64
        } else {
            self.to_bigint_trunc()
                .map(|b| bigint_low_u64(&b) as i64)
                .unwrap_or(0)
        }
    }

    /// Numeric comparison (`BigDecimal.compareTo`: scale is ignored).
    pub fn cmp_numeric(&self, other: &Decimal) -> Ordering {
        cmp_decimal_parts(&self.unscaled, self.scale, &other.unscaled, other.scale)
    }
}

pub(crate) fn pow10(k: u32) -> BigInt {
    num_traits::pow(BigInt::from(10u8), k as usize)
}

/// The low 64 bits of a two's-complement integer.
pub(crate) fn bigint_low_u64(v: &BigInt) -> u64 {
    let b = v.to_signed_bytes_le();
    let fill = if v.sign() == Sign::Minus { 0xff } else { 0 };
    let mut a = [fill; 8];
    for (i, x) in b.iter().take(8).enumerate() {
        a[i] = *x;
    }
    u64::from_le_bytes(a)
}

/// Compares two varints numerically without allocating (`IntegerType.compareIntegers`); empty
/// values sort first.
pub fn compare_varint(l: &[u8], r: &[u8]) -> Ordering {
    match (l.is_empty(), r.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let li = msb_index(l);
    let ri = msb_index(r);
    let ll = l.len() - li;
    let rl = r.len() - ri;
    let lm = l[li] as i8;
    let rm = r[ri] as i8;
    if ll != rl {
        if lm < 0 {
            return if rm < 0 { rl.cmp(&ll) } else { Ordering::Less };
        }
        if rm < 0 {
            return Ordering::Greater;
        }
        return ll.cmp(&rl);
    }
    if lm != rm {
        return lm.cmp(&rm);
    }
    l[li + 1..].cmp(&r[ri + 1..])
}

/// Index of the first byte that is not sign padding.
fn msb_index(b: &[u8]) -> usize {
    let sign = if (b[0] as i8) < 0 { 0xff } else { 0 };
    let mut i = 0;
    while i + 1 < b.len() && b[i] == sign && ((b[i + 1] as i8) < 0) == (sign == 0xff) {
        i += 1;
    }
    i
}

/// Two's-complement bytes to i128 when they fit.
fn small(b: &[u8]) -> Option<i128> {
    if b.len() > 16 {
        return None;
    }
    if b.is_empty() {
        return Some(0);
    }
    let fill = if (b[0] as i8) < 0 { 0xff } else { 0 };
    let mut a = [fill; 16];
    a[16 - b.len()..].copy_from_slice(b);
    Some(i128::from_be_bytes(a))
}

/// Compares two serialised decimals (`DecimalType`: empty first, then numeric).
pub fn compare_decimal(l: &[u8], r: &[u8]) -> Ordering {
    match (l.is_empty(), r.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let (Some(ls), Some(rs)) = (crate::wire::i32_at(l, 0), crate::wire::i32_at(r, 0)) else {
        // Malformed (validation rejects these); keep a total order.
        return l.cmp(r);
    };
    let (lu, ru) = (&l[4..], &r[4..]);
    if let (Some(a), Some(b)) = (small(lu), small(ru)) {
        if ls == rs {
            return a.cmp(&b);
        }
        if a.signum() != b.signum() || a == 0 {
            return a.signum().cmp(&b.signum());
        }
        let diff = ls as i64 - rs as i64;
        if diff.unsigned_abs() <= 38 {
            let p = 10i128.pow(diff.unsigned_abs() as u32);
            let scaled = if diff > 0 {
                b.checked_mul(p).map(|b| (a, b))
            } else {
                a.checked_mul(p).map(|a| (a, b))
            };
            if let Some((a, b)) = scaled {
                return a.cmp(&b);
            }
        }
    }
    cmp_decimal_parts(
        &BigInt::from_signed_bytes_be(lu),
        ls,
        &BigInt::from_signed_bytes_be(ru),
        rs,
    )
}

fn cmp_decimal_parts(a: &BigInt, sa: i32, b: &BigInt, sb: i32) -> Ordering {
    let (siga, sigb) = (a.signum(), b.signum());
    if siga != sigb || a.is_zero() {
        return siga.cmp(&sigb);
    }
    if sa == sb {
        return a.cmp(b);
    }
    // Compare orders of magnitude first so we never scale by a huge power of ten.
    let da = a.magnitude().to_string().len() as i64;
    let db = b.magnitude().to_string().len() as i64;
    let adj_a = da - 1 - sa as i64;
    let adj_b = db - 1 - sb as i64;
    if adj_a != adj_b {
        let o = adj_a.cmp(&adj_b);
        return if siga.is_negative() { o.reverse() } else { o };
    }
    // Same magnitude: the scale difference equals the digit-count difference, so it is bounded
    // by the bytes present.
    let diff = sa as i64 - sb as i64;
    if diff > 0 {
        a.cmp(&(b * pow10(diff as u32)))
    } else {
        (a * pow10((-diff) as u32)).cmp(b)
    }
}

/// `BigInteger` → f64, correctly rounded.
pub fn bigint_to_f64(v: &BigInt) -> f64 {
    v.to_f64().unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).unwrap_or_else(|| panic!("{s}"))
    }

    #[test]
    fn decimal_parse_and_java_string() {
        // (input, unscaled, scale, BigDecimal.toString) — Cassandra prints these via CAST/JSON.
        for (s, u, sc, out) in [
            ("1.10", 110, 2, "1.10"),
            ("1e7", 1, -7, "1E+7"),
            ("1E+3", 1, -3, "1E+3"),
            ("0.0001", 1, 4, "0.0001"),
            ("0.000001", 1, 6, "0.000001"),
            ("0.0000001", 1, 7, "1E-7"),
            ("123.456e-10", 123456, 13, "1.23456E-8"),
            ("-0.00", 0, 2, "0.00"),
            ("-1.50", -150, 2, "-1.50"),
            ("100", 100, 0, "100"),
            (".5", 5, 1, "0.5"),
            ("5.", 5, 0, "5"),
            ("+12", 12, 0, "12"),
            ("-1e10", -1, -10, "-1E+10"),
            ("0e2", 0, -2, "0E+2"),
            (
                "12345678901234567890.123",
                12345678901234567890123i128,
                3,
                "12345678901234567890.123",
            ),
        ] {
            let x = d(s);
            assert_eq!(x.unscaled, BigInt::from(u), "{s}");
            assert_eq!(x.scale, sc, "{s}");
            assert_eq!(x.to_java_string(), out, "{s}");
        }
        for bad in [
            "",
            "-",
            ".",
            "e5",
            "1e",
            "1e+",
            "1.2.3",
            "abc",
            " 1",
            "1 ",
            "0x10",
            "1e99999999999",
            "NaN",
            "Infinity",
        ] {
            assert!(Decimal::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn decimal_scylla_string() {
        // ScyllaDB 2026.3: CAST(dec AS text) / toJson.
        for (s, out) in [
            ("1.10", "1.1"),
            ("1.0", "1"),
            ("1.00", "1"),
            ("1e7", "10000000"),
            ("1E+3", "1000"),
            ("1e2", "100"),
            ("123.456e-10", "0.0000000123456"),
            ("-0.00", "0"),
            ("0.0", "0"),
            ("-0", "0"),
            ("0.10", "0.1"),
            ("-1.50", "-1.5"),
            ("1e-20", "0.00000000000000000001"),
            ("1e20", "100000000000000000000"),
            ("12345678901234567890.123", "12345678901234567890.123"),
            ("5E-1", "0.5"),
        ] {
            assert_eq!(d(s).to_scylla_string(), out, "{s}");
        }
        // A hostile scale does not expand.
        let huge = Decimal::new(BigInt::from(1), i32::MIN);
        assert!(huge.to_scylla_string().len() < 64);
        assert!(huge.to_plain_string().len() < 64);
    }

    #[test]
    fn decimal_bytes() {
        // decimalAsBlob on both references.
        assert_eq!(d("1.0").to_bytes(), [0, 0, 0, 1, 0x0a]);
        assert_eq!(d("1e10").to_bytes(), [0xff, 0xff, 0xff, 0xf6, 0x01]);
        assert_eq!(d("-0.5").to_bytes(), [0, 0, 0, 1, 0xfb]);
        assert_eq!(d("0").to_bytes(), [0, 0, 0, 0, 0]);
        assert_eq!(
            Decimal::from_bytes(&[0, 0, 0, 2, 0xff, 0x6a]).unwrap(),
            d("-1.50")
        );
        assert_eq!(
            Decimal::from_bytes(&[0, 0, 0, 2]).unwrap(),
            Decimal::new(BigInt::zero(), 2)
        );
        assert!(Decimal::from_bytes(&[0, 0, 0]).is_none());
    }

    #[test]
    fn decimal_ordering() {
        let ord = |a: &str, b: &str| compare_decimal(&d(a).to_bytes(), &d(b).to_bytes());
        assert_eq!(ord("1.0", "1.00"), Ordering::Equal);
        assert_eq!(ord("1", "1.00"), Ordering::Equal);
        assert_eq!(ord("-1e10", "-0.5"), Ordering::Less);
        assert_eq!(ord("0.1", "1.0"), Ordering::Less);
        assert_eq!(ord("1e10", "1.0"), Ordering::Greater);
        assert_eq!(ord("0", "-0.00"), Ordering::Equal);
        assert_eq!(ord("-0.5", "0"), Ordering::Less);
        assert_eq!(ord("1e-30", "1e-31"), Ordering::Greater);
        assert_eq!(
            ord(
                "123456789012345678901234567890123456789.5",
                "123456789012345678901234567890123456789.4"
            ),
            Ordering::Greater
        );
        assert_eq!(
            ord("1e100", "99999999999999999999999999999999999999999"),
            Ordering::Greater
        );
        assert_eq!(
            ord("-1e100", "-99999999999999999999999999999999999999999"),
            Ordering::Less
        );
        assert_eq!(compare_decimal(&[], &d("0").to_bytes()), Ordering::Less);
        // Extreme scales compare without expanding.
        let a = Decimal::new(BigInt::from(1), i32::MIN).to_bytes();
        let b = Decimal::new(BigInt::from(1), i32::MAX).to_bytes();
        assert_eq!(compare_decimal(&a, &b), Ordering::Greater);
        assert_eq!(compare_decimal(&b, &a), Ordering::Less);
    }

    #[test]
    fn varint_ordering_and_codec() {
        let v = |s: &str| varint_to_bytes(&parse_varint(s).unwrap());
        assert_eq!(v("0"), [0]);
        assert_eq!(v("255"), [0, 0xff]);
        assert_eq!(v("-1"), [0xff]);
        assert_eq!(v("-256"), [0xff, 0]);
        assert_eq!(
            v("100000000000000000000"),
            [0x05, 0x6b, 0xc7, 0x5e, 0x2d, 0x63, 0x10, 0, 0]
        );
        let c = compare_varint;
        // Padded encodings compare equal to minimal ones (0x0000ff == 0x00ff, 0xffff == 0xff).
        assert_eq!(c(&[0, 0, 0xff], &[0, 0xff]), Ordering::Equal);
        assert_eq!(c(&[0xff, 0xff], &[0xff]), Ordering::Equal);
        let sorted = ["-256", "-1", "0", "255", "100000000000000000000"];
        for w in sorted.windows(2) {
            assert_eq!(c(&v(w[0]), &v(w[1])), Ordering::Less, "{w:?}");
            assert_eq!(c(&v(w[1]), &v(w[0])), Ordering::Greater, "{w:?}");
        }
        assert_eq!(c(&[], &[0]), Ordering::Less);
        assert_eq!(c(&[0x80], &[0x7f]), Ordering::Less);
        assert_eq!(
            c(&[0xff, 0x00, 0x01], &[0xff, 0x00, 0x00]),
            Ordering::Greater
        );
        for bad in ["", "-", "1.5", " 1", "1e3", "0x10"] {
            assert!(parse_varint(bad).is_none(), "{bad}");
        }
        assert_eq!(parse_varint("+12"), Some(BigInt::from(12)));
        assert_eq!(parse_varint("-0012"), Some(BigInt::from(-12)));
    }

    #[test]
    fn decimal_conversions() {
        assert_eq!(d("123.987").to_i64_wrapping(), 123);
        assert_eq!(d("-0.5").to_i64_wrapping(), 0);
        assert_eq!(
            d("1e19").to_i64_wrapping(),
            10_000_000_000_000_000_000u64 as i64
        );
        assert_eq!(Decimal::new(BigInt::from(1), -100).to_i64_wrapping(), 0);
        assert_eq!(d("-7.9").to_bigint_trunc(), Some(BigInt::from(-7)));
        assert_eq!(
            Decimal::new(BigInt::from(5), i32::MAX).to_bigint_trunc(),
            Some(BigInt::zero())
        );
        assert_eq!(
            Decimal::new(BigInt::from(5), i32::MIN).to_bigint_trunc(),
            None
        );
        assert_eq!(d("0.1").to_f64(), 0.1);
        assert_eq!(d("123.987").to_f32(), 123.987f32);
        assert_eq!(bigint_low_u64(&BigInt::from(-1)), u64::MAX);
    }
}

#[cfg(test)]
mod more_tests {
    use super::*;

    #[test]
    fn helpers() {
        assert!(Decimal::new(BigInt::zero(), 5).is_zero());
        assert!(!Decimal::from_i64(1).is_zero());
        assert_eq!(Decimal::from_i64(-7).to_java_string(), "-7");
        assert_eq!(
            Decimal::parse("1.0")
                .unwrap()
                .cmp_numeric(&Decimal::parse("1.00").unwrap()),
            Ordering::Equal
        );
        assert_eq!(
            Decimal::parse("2")
                .unwrap()
                .cmp_numeric(&Decimal::parse("10e-1").unwrap()),
            Ordering::Greater
        );
        assert_eq!(Decimal::parse("1E+3").unwrap().to_plain_string(), "1000");
        assert_eq!(
            Decimal::parse("0.0100").unwrap().to_plain_string(),
            "0.0100"
        );
        assert_eq!(Decimal::parse("-0e5").unwrap().to_plain_string(), "0");
        assert_eq!(bigint_to_f64(&BigInt::from(1u64 << 53)), 9007199254740992.0);
        assert_eq!(varint_from_bytes(&[]), BigInt::zero());
        assert_eq!(varint_from_bytes(&[0xff, 0x00]), BigInt::from(-256));
        assert_eq!(pow10(3), BigInt::from(1000));
    }
}
