//! Decimal arithmetic for INCRBYFLOAT / HINCRBYFLOAT.
//!
//! Redis does these in `long double` and prints `%.17Lf` with trailing zeros removed. On arm64
//! (Graviton) that is IEEE quad precision (~34 significant digits), so `0.1 + 0.2` prints `0.3`
//! and `1e20 + 1` prints `100000000000000000001`. Plain `f64` can't reproduce that; exact
//! decimal arithmetic rounded to 34 significant digits does.

const SIG_DIGITS: usize = 34;

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

/// Round to `SIG_DIGITS` significant digits, half to even.
fn round_sig(mut d: Dec) -> Dec {
    if d.digits.len() <= SIG_DIGITS {
        return d;
    }
    let cut = d.digits.len() - SIG_DIGITS;
    let rest = d.digits.split_off(SIG_DIGITS);
    d.exp += cut as i64;
    let first = rest[0];
    let tail_nonzero = rest[1..].iter().any(|&c| c != 0);
    let round_up = first > 5 || (first == 5 && (tail_nonzero || d.digits.last().is_some_and(|c| c % 2 == 1)));
    if round_up {
        increment(&mut d.digits);
    }
    normalize(d)
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

/// Digits aligned to exponent `exp` (which must be <= d.exp).
fn aligned(d: &Dec, exp: i64) -> Vec<u8> {
    let mut v = d.digits.clone();
    v.extend(std::iter::repeat_n(0, (d.exp - exp) as usize));
    v
}

fn cmp_mag(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn add_mag(a: &[u8], b: &[u8]) -> Vec<u8> {
    let n = a.len().max(b.len());
    let mut out = vec![0u8; n + 1];
    let mut carry = 0;
    for i in 0..n {
        let x = if i < a.len() { a[a.len() - 1 - i] } else { 0 };
        let y = if i < b.len() { b[b.len() - 1 - i] } else { 0 };
        let s = x + y + carry;
        out[n - i] = s % 10;
        carry = s / 10;
    }
    out[0] = carry;
    out
}

/// a - b with |a| >= |b|.
fn sub_mag(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = a.to_vec();
    let mut borrow = 0i8;
    for i in 0..a.len() {
        let x = a[a.len() - 1 - i] as i8;
        let y = if i < b.len() { b[b.len() - 1 - i] as i8 } else { 0 };
        let mut d = x - y - borrow;
        if d < 0 {
            d += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out[a.len() - 1 - i] = d as u8;
    }
    out
}

fn add(a: &Dec, b: &Dec) -> Dec {
    if a.digits.is_empty() {
        return b.clone();
    }
    if b.digits.is_empty() {
        return a.clone();
    }
    let exp = a.exp.min(b.exp);
    // Guard against absurd exponent gaps (1e300 + 1e-300): the small one can't matter.
    if (a.exp - b.exp).abs() > 200 {
        let (big, small) = if a.exp + a.digits.len() as i64 > b.exp + b.digits.len() as i64 { (a, b) } else { (b, a) };
        let _ = small;
        return big.clone();
    }
    let (x, y) = (aligned(a, exp), aligned(b, exp));
    let (neg, digits) = if a.neg == b.neg {
        (a.neg, add_mag(&x, &y))
    } else {
        let x = strip(&x);
        let y = strip(&y);
        match cmp_mag(&x, &y) {
            std::cmp::Ordering::Equal => (false, vec![]),
            std::cmp::Ordering::Greater => (a.neg, sub_mag(&x, &y)),
            std::cmp::Ordering::Less => (b.neg, sub_mag(&y, &x)),
        }
    };
    normalize(Dec { neg, digits, exp })
}

fn strip(v: &[u8]) -> Vec<u8> {
    let lead = v.iter().take_while(|&&c| c == 0).count();
    v[lead..].to_vec()
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

/// Is this a valid long-double input (as `string2ld` accepts)?
pub fn valid(s: &[u8]) -> bool {
    parse(s).is_some() || is_inf(s)
}

fn is_inf(s: &[u8]) -> bool {
    let l = s.to_ascii_lowercase();
    let b = l.strip_prefix(b"-").or_else(|| l.strip_prefix(b"+")).unwrap_or(&l);
    b == b"inf" || b == b"infinity"
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
    if is_inf(current) || is_inf(incr) {
        return if valid(current) && valid(incr) { Err(IncrError::NanOrInf) } else { Err(IncrError::NotFloat) };
    }
    let a = parse(current).ok_or(IncrError::NotFloat)?;
    let b = parse(incr).ok_or(IncrError::NotFloat)?;
    let sum = round_sig(add(&round_sig(a), &round_sig(b)));
    // Quad precision overflows past ~1.19e4932.
    if !sum.digits.is_empty() && sum.exp + sum.digits.len() as i64 > 4932 {
        return Err(IncrError::NanOrInf);
    }
    Ok(format(&sum))
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
}
