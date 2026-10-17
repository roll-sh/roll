//! `long double` arithmetic for INCRBYFLOAT / HINCRBYFLOAT / INCREX.
//!
//! Redis does these in `long double` and prints `%.17Lf` with trailing zeros removed. On arm64
//! (Graviton) that is IEEE binary128, so `0.1 + 0.2` prints `0.3` and `1e20 + 0.3` prints
//! `100000000000000000000.29999999999999716`. We use the same binary128 arithmetic and print the
//! exact binary value the way `printf` does.

use rustc_apfloat::ieee::Quad;
use rustc_apfloat::{Float, Round};

/// `MAX_LONG_DOUBLE_CHARS`: `string2ld` refuses anything this long.
const MAX_CHARS: usize = 5 * 1024;

#[derive(Clone, Debug, PartialEq)]
struct Dec {
    neg: bool,
    /// Most significant first, no leading zeros (empty = 0).
    digits: Vec<u8>,
    /// value = digits × 10^exp
    exp: i64,
}

fn parse(s: &[u8]) -> Option<Dec> {
    if s.is_empty() || s[0].is_ascii_whitespace() || s[s.len() - 1].is_ascii_whitespace() {
        return None;
    }
    let mut i = 0;
    let mut neg = false;
    if s[0] == b'-' || s[0] == b'+' {
        neg = s[0] == b'-';
        i = 1;
    }
    let mut digits = Vec::new();
    let mut exp: i64 = 0;
    let mut seen = false;
    while i < s.len() && s[i].is_ascii_digit() {
        digits.push(s[i] - b'0');
        seen = true;
        i += 1;
    }
    if i < s.len() && s[i] == b'.' {
        i += 1;
        while i < s.len() && s[i].is_ascii_digit() {
            digits.push(s[i] - b'0');
            exp -= 1;
            seen = true;
            i += 1;
        }
    }
    if !seen {
        return None;
    }
    if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
        i += 1;
        let mut eneg = false;
        if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
            eneg = s[i] == b'-';
            i += 1;
        }
        let start = i;
        let mut e: i64 = 0;
        while i < s.len() && s[i].is_ascii_digit() {
            e = e.saturating_mul(10).saturating_add((s[i] - b'0') as i64);
            i += 1;
        }
        if i == start {
            return None;
        }
        exp += if eneg { -e } else { e };
    }
    if i != s.len() {
        return None;
    }
    Some(normalize(Dec { neg, digits, exp }))
}

fn normalize(mut d: Dec) -> Dec {
    let lead = d.digits.iter().take_while(|&&c| c == 0).count();
    d.digits.drain(..lead);
    while d.digits.last() == Some(&0) {
        d.digits.pop();
        d.exp += 1;
    }
    if d.digits.is_empty() {
        d.neg = false;
        d.exp = 0;
    }
    d
}

fn increment(digits: &mut Vec<u8>) {
    for c in digits.iter_mut().rev() {
        if *c == 9 {
            *c = 0;
        } else {
            *c += 1;
            return;
        }
    }
    digits.insert(0, 1);
}

/// `%.17Lf` with trailing zeros (and a bare `.`) removed, `-0` printed as `0`.
fn format(d: &Dec) -> String {
    // Round to 17 fractional digits (half to even on the exact value).
    let mut d = d.clone();
    if d.exp < -17 {
        let keep = d.digits.len() as i64 + d.exp + 17; // digits left of the cut
        if keep <= 0 {
            // Entirely below 1e-17: rounds to 0 unless it's >= 0.5e-17.
            let first_pos = -(d.digits.len() as i64 + d.exp); // zeros after the point before digits
            let up = keep == 0 && first_pos == 17 && (d.digits[0] > 5 || (d.digits[0] == 5 && d.digits[1..].iter().any(|&c| c != 0)));
            d = if up { Dec { neg: d.neg, digits: vec![1], exp: -17 } } else { Dec { neg: false, digits: vec![], exp: 0 } };
        } else {
            let keep = keep as usize;
            let rest = d.digits.split_off(keep);
            let first = rest[0];
            let tail = rest[1..].iter().any(|&c| c != 0);
            let up = first > 5 || (first == 5 && (tail || d.digits.last().is_some_and(|c| c % 2 == 1)));
            d.exp = -17;
            if up {
                increment(&mut d.digits);
            }
            d = normalize(d);
        }
    }
    if d.digits.is_empty() {
        return "0".into();
    }
    let mut s = String::new();
    if d.neg {
        s.push('-');
    }
    let digits: String = d.digits.iter().map(|c| (b'0' + c) as char).collect();
    if d.exp >= 0 {
        s.push_str(&digits);
        s.extend(std::iter::repeat_n('0', d.exp as usize));
    } else {
        let frac = (-d.exp) as usize;
        if digits.len() > frac {
            s.push_str(&digits[..digits.len() - frac]);
            s.push('.');
            s.push_str(&digits[digits.len() - frac..]);
        } else {
            s.push_str("0.");
            s.extend(std::iter::repeat_n('0', frac - digits.len()));
            s.push_str(&digits);
        }
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

fn is_inf(s: &[u8]) -> bool {
    let l = s.to_ascii_lowercase();
    let b = l.strip_prefix(b"-").or_else(|| l.strip_prefix(b"+")).unwrap_or(&l);
    b == b"inf" || b == b"infinity"
}

/// `strtold` on hex input (`[+-]0x<hex>[.<hex>][p[+-]<dec>]`): the canonical form apfloat reads,
/// or `None` if it isn't one. Absurd exponents are clamped (the result is 0 or out of range).
fn hex_input(s: &[u8]) -> Option<String> {
    let (neg, body) = match s.first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let body = body.strip_prefix(b"0x").or_else(|| body.strip_prefix(b"0X"))?;
    let split = body.iter().position(|&c| c == b'p' || c == b'P').unwrap_or(body.len());
    let (mant, exp) = body.split_at(split);
    let hex = |c: &u8| c.is_ascii_hexdigit();
    let (int, frac) = match mant.iter().position(|&c| c == b'.') {
        Some(i) => (&mant[..i], &mant[i + 1..]),
        None => (mant, &mant[..0]),
    };
    if int.len() + frac.len() == 0 || !int.iter().all(hex) || !frac.iter().all(hex) {
        return None;
    }
    let e: i64 = if exp.is_empty() {
        0
    } else {
        let d = &exp[1..];
        let (eneg, d) = match d.first() {
            Some(b'-') => (true, &d[1..]),
            Some(b'+') => (false, &d[1..]),
            _ => (false, d),
        };
        if d.is_empty() || !d.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let v = d.iter().fold(0i64, |a, &c| a.saturating_mul(10).saturating_add((c - b'0') as i64)).min(100_000);
        if eneg { -v } else { v }
    };
    let mant = std::str::from_utf8(mant).ok()?;
    Some(format!("{}0x{mant}p{e}", if neg { "-" } else { "" }))
}

/// A `long double` (IEEE binary128), as `string2ld` reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ld(Quad);

impl Ld {
    pub const ZERO: Ld = Ld(Quad::ZERO);

    /// `string2ld`: decimal, hex or infinity, no surrounding spaces; NaN, overflow and underflow
    /// to zero are rejected. Rounded to nearest, ties to even.
    pub fn parse(s: &[u8]) -> Option<Ld> {
        if s.is_empty() || s.len() >= MAX_CHARS || s[0].is_ascii_whitespace() {
            return None;
        }
        if is_inf(s) {
            return Some(Ld(if s[0] == b'-' { -Quad::INFINITY } else { Quad::INFINITY }));
        }
        let (text, nonzero) = match parse(s) {
            Some(d) if d.digits.is_empty() => return Some(Ld(if s[0] == b'-' { -Quad::ZERO } else { Quad::ZERO })),
            Some(d) => {
                let top = d.digits.len() as i64 + d.exp;
                // Beyond LDBL_MAX (1.19e4932), or below half the smallest subnormal (6.5e-4966).
                if !(-4966..=4933).contains(&top) {
                    return None;
                }
                let digits: String = d.digits.iter().map(|c| (b'0' + c) as char).collect();
                (format!("{}{digits}e{}", if d.neg { "-" } else { "" }, d.exp), true)
            }
            None => {
                let h = hex_input(s)?;
                let nonzero = h.split('p').next().is_some_and(|m| m.bytes().skip(2).any(|c| c.is_ascii_hexdigit() && c != b'0'));
                (h, nonzero)
            }
        };
        let v = Quad::from_str_r(&text, Round::NearestTiesToEven).ok()?.value;
        if v.is_infinite() || v.is_nan() || (v.is_zero() && nonzero) {
            return None;
        }
        Some(Ld(v))
    }

    pub fn max() -> Ld {
        Ld(Quad::largest())
    }

    pub fn min() -> Ld {
        Ld(-Quad::largest())
    }

    pub fn is_inf(&self) -> bool {
        self.0.is_infinite()
    }

    pub fn is_neg(&self) -> bool {
        self.0.is_negative()
    }

    /// `a + b`; past `LDBL_MAX` it is infinite.
    pub fn add(&self, b: &Ld) -> Ld {
        Ld((self.0 + b.0).value)
    }

    pub fn sub(&self, b: &Ld) -> Ld {
        Ld((self.0 - b.0).value)
    }

    pub fn compare(&self, b: &Ld) -> std::cmp::Ordering {
        self.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)
    }

    /// `ld2string(…, LD_STR_HUMAN)`: `%.17Lf` without trailing zeros.
    pub fn human(&self) -> String {
        if self.0.is_infinite() {
            return if self.0.is_negative() { "-inf".into() } else { "inf".into() };
        }
        if self.0.is_nan() {
            return "nan".into();
        }
        format(&exact(self.0))
    }
}

/// The exact decimal value of a finite quad (or 0 if it is far below `%.17Lf`'s precision).
fn exact(q: Quad) -> Dec {
    const BASE: u64 = 1_000_000_000;
    let bits = q.to_bits();
    let neg = bits >> 127 == 1;
    let field = ((bits >> 112) & 0x7fff) as i64;
    let frac = bits & ((1u128 << 112) - 1);
    let (m, e) = if field == 0 { (frac, -16494) } else { (frac | 1 << 112, field - 16383 - 112) };
    let width = 128 - m.leading_zeros() as i64;
    // Under 2^-60 (< 1e-18) prints as 0.
    if m == 0 || e + width <= -60 {
        return Dec { neg: false, digits: vec![], exp: 0 };
    }
    // Little-endian base-1e9 limbs of m, then × 2^e or × 5^-e (with the point moved by e).
    let mut limbs = Vec::new();
    let mut v = m;
    while v > 0 {
        limbs.push((v % BASE as u128) as u64);
        v /= BASE as u128;
    }
    let mul = |limbs: &mut Vec<u64>, f: u64| {
        let mut carry = 0;
        for l in limbs.iter_mut() {
            let x = *l * f + carry;
            *l = x % BASE;
            carry = x / BASE;
        }
        while carry > 0 {
            limbs.push(carry % BASE);
            carry /= BASE;
        }
    };
    let (mut left, step, f) = if e >= 0 { (e, 29, 1u64 << 29) } else { (-e, 13, 5u64.pow(13)) };
    while left > 0 {
        let n = left.min(step);
        mul(&mut limbs, if n == step { f } else if e >= 0 { 1 << n } else { 5u64.pow(n as u32) });
        left -= n;
    }
    let mut digits = Vec::with_capacity(limbs.len() * 9);
    for (i, l) in limbs.iter().rev().enumerate() {
        let s = if i == 0 { l.to_string() } else { format!("{l:09}") };
        digits.extend(s.bytes().map(|c| c - b'0'));
    }
    normalize(Dec { neg, digits, exp: e.min(0) })
}

/// Would `string2ld` accept this (finite, or an infinity literal)?
pub fn valid(s: &[u8]) -> bool {
    Ld::parse(s).is_some()
}

#[derive(Debug, PartialEq, Eq)]
pub enum IncrError {
    /// The current value or the increment isn't a number.
    NotFloat,
    /// The result would be NaN or infinite.
    NanOrInf,
}

/// `current + incr`, formatted like Redis. `current` is the stored string (absent = "0").
pub fn incr_by_float(current: &[u8], incr: &[u8]) -> Result<String, IncrError> {
    let a = Ld::parse(current).ok_or(IncrError::NotFloat)?;
    let b = Ld::parse(incr).ok_or(IncrError::NotFloat)?;
    let sum = a.add(&b);
    if sum.0.is_infinite() || sum.0.is_nan() {
        return Err(IncrError::NanOrInf);
    }
    Ok(sum.human())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_redis_on_arm64() {
        let t = |a: &str, b: &str| incr_by_float(a.as_bytes(), b.as_bytes()).unwrap();
        assert_eq!(t("0.1", "0.2"), "0.3");
        assert_eq!(t("10.50", "0.1"), "10.6");
        assert_eq!(t("5.0e3", "2.0e2"), "5200");
        assert_eq!(t("3", "1.1234567890123456789"), "4.12345678901234568");
        assert_eq!(t("1", "1e-20"), "1");
        assert_eq!(t("123456789", "0.333333333333333333333333"), "123456789.33333333333333333");
        assert_eq!(t("1e20", "1"), "100000000000000000001");
        assert_eq!(t("1.5", "-1.5"), "0");
        assert_eq!(t("0", "-0.5"), "-0.5");
        assert_eq!(t("-1", "0.25"), "-0.75");
        assert_eq!(t("0", "5e-18"), "0");
        assert_eq!(t("0", "6e-18"), "0.00000000000000001");
        assert_eq!(incr_by_float(b"abc", b"1"), Err(IncrError::NotFloat));
        assert_eq!(incr_by_float(b"1", b"inf"), Err(IncrError::NanOrInf));
        assert_eq!(incr_by_float(b"1", b" 1"), Err(IncrError::NotFloat));
    }

    #[test]
    fn long_double() {
        let max = Ld::max().human();
        assert_eq!(max.len(), 4933);
        assert!(max.starts_with("118973149535723176508575932662800701"));
        let p = |s: &str| Ld::parse(s.as_bytes()).unwrap();
        assert_eq!(p("1.5").add(&p("2")).human(), "3.5");
        assert_eq!(p("1").sub(&p("3")).human(), "-2");
        assert_eq!(p("-2").compare(&p("1")), std::cmp::Ordering::Less);
        assert_eq!(p("-2").compare(&p("-10")), std::cmp::Ordering::Greater);
        assert_eq!(p("10").compare(&p("9.99")), std::cmp::Ordering::Greater);
        assert!(Ld::max().add(&Ld::max()).is_inf());
        assert!(Ld::parse(b"1e5000").is_none() && Ld::parse(b"1e-5000").is_none() && Ld::parse(b" 1").is_none());
        assert!(p("-inf").is_inf() && p("-inf").compare(&Ld::min()) == std::cmp::Ordering::Less);
        // Binary, not decimal: what Redis prints on arm64.
        assert_eq!(p("1e20").add(&p("0.3")).human(), "100000000000000000000.29999999999999716");
        assert_eq!(p("0x1.8p1").human(), "3");
        assert_eq!(p("0x10").human(), "16");
        assert!(Ld::parse(b"0x").is_none() && Ld::parse(b"0x1p").is_none() && Ld::parse(b"nan").is_none());
        assert!(Ld::parse(b"1e-4950").is_some() && Ld::parse(b"0e-99999").is_some());
        assert_eq!(p("-0").human(), "0");
    }
}
