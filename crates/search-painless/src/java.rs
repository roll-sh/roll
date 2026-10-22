//! Java library semantics that scripts can observe: how numbers print (`Double.toString`),
//! how strings parse into numbers (`Integer.parseInt`, `Double.parseDouble`), how strings hash,
//! and how `String` indexes (UTF-16 code units).

/// `Double.toString`: the shortest decimal that round-trips (the JDK 19+ algorithm), laid out
/// as plain notation for magnitudes in [10^-3, 10^7) and computerized scientific notation
/// otherwise.
pub fn fmt_double(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if d == 0.0 {
        return if d.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let mut buf = ryu::Buffer::new();
    let s = buf.format_finite(d);
    if significant_digits(s) == 1 {
        // Java never prints fewer than two significant digits: among the two-digit decimals
        // that round to the value it picks the closest one (`4.9E-324`, not `5.0E-324`)
        let two = format!("{d:.1e}");
        if two.parse::<f64>() == Ok(d) {
            return layout(&two);
        }
    }
    layout(s)
}

/// Number of significant digits in a decimal string (`ryu` or `{:e}` output).
fn significant_digits(s: &str) -> usize {
    let mantissa = s.split(['e', 'E']).next().unwrap_or("");
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let t = digits.trim_start_matches('0').trim_end_matches('0');
    t.len().max(1)
}

/// `Float.toString`, same layout as [`fmt_double`] with float precision.
pub fn fmt_float(f: f32) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let mut buf = ryu::Buffer::new();
    let s = buf.format_finite(f);
    if significant_digits(s) == 1 {
        let two = format!("{f:.1e}");
        if two.parse::<f32>() == Ok(f) {
            return layout(&two);
        }
    }
    layout(s)
}

/// Re-lays a shortest round-trip representation (in any of ryu's notations) out the Java way.
fn layout(s: &str) -> String {
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (mant, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i32>().unwrap_or(0)),
        None => (s, 0),
    };
    // digits and position of the decimal point relative to the digit string
    let (int_part, frac_part) = match mant.find('.') {
        Some(i) => (&mant[..i], &mant[i + 1..]),
        None => (mant, ""),
    };
    let mut digits: String = format!("{int_part}{frac_part}");
    let mut point = int_part.len() as i32 + exp;
    // strip leading zeros
    let lead = digits.len() - digits.trim_start_matches('0').len();
    if lead == digits.len() {
        return if neg { "-0.0".into() } else { "0.0".into() };
    }
    digits.drain(..lead);
    point -= lead as i32;
    let trimmed = digits.trim_end_matches('0').len();
    digits.truncate(trimmed);
    // value = 0.d1d2d3... * 10^point ; decimal exponent of first digit = point - 1
    let sci_exp = point - 1;
    let mut out = String::with_capacity(digits.len() + 8);
    if neg {
        out.push('-');
    }
    if (-3..7).contains(&sci_exp) {
        if point <= 0 {
            out.push_str("0.");
            for _ in 0..(-point) {
                out.push('0');
            }
            out.push_str(&digits);
        } else if point as usize >= digits.len() {
            out.push_str(&digits);
            for _ in 0..(point as usize - digits.len()) {
                out.push('0');
            }
            out.push_str(".0");
        } else {
            out.push_str(&digits[..point as usize]);
            out.push('.');
            out.push_str(&digits[point as usize..]);
        }
    } else {
        out.push_str(&digits[..1]);
        out.push('.');
        if digits.len() > 1 {
            out.push_str(&digits[1..]);
        } else {
            out.push('0');
        }
        out.push('E');
        out.push_str(&sci_exp.to_string());
    }
    out
}

/// Characters `String.trim` removes (everything up to and including U+0020).
fn is_java_trim(c: char) -> bool {
    (c as u32) <= 0x20
}

pub fn java_trim(s: &str) -> &str {
    s.trim_matches(is_java_trim)
}

/// `Double.parseDouble` / `Double.valueOf(String)`: leading/trailing whitespace, an optional
/// sign, `NaN`, `Infinity`, decimal or hexadecimal floating point with an optional `f`/`d`
/// suffix. `None` means a `NumberFormatException`.
pub fn parse_double(s: &str) -> Option<f64> {
    let t = java_trim(s);
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let v = if body == "NaN" {
        f64::NAN
    } else if body == "Infinity" {
        f64::INFINITY
    } else if body.starts_with("0x") || body.starts_with("0X") {
        parse_hex_float(&body[2..])?
    } else {
        let b = body.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(body);
        if b.is_empty() || !b.bytes().all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-')) {
            return None;
        }
        // Java requires at least one digit in the mantissa and digits after an exponent
        let (mant, ex) = match b.find(['e', 'E']) {
            Some(i) => (&b[..i], Some(&b[i + 1..])),
            None => (b, None),
        };
        if !mant.bytes().any(|c| c.is_ascii_digit()) || mant.contains(['+', '-']) || mant.matches('.').count() > 1 {
            return None;
        }
        if let Some(ex) = ex {
            let ex = ex.strip_prefix(['+', '-']).unwrap_or(ex);
            if ex.is_empty() || !ex.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
        }
        b.parse::<f64>().ok()?
    };
    Some(if neg { -v } else { v })
}

fn parse_hex_float(s: &str) -> Option<f64> {
    let s = s.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(s);
    let p = s.find(['p', 'P'])?;
    let (mant, ex) = (&s[..p], &s[p + 1..]);
    let ex: i32 = ex.parse().ok()?;
    let (ip, fp) = match mant.find('.') {
        Some(i) => (&mant[..i], &mant[i + 1..]),
        None => (mant, ""),
    };
    if ip.is_empty() && fp.is_empty() {
        return None;
    }
    let mut v = 0f64;
    for c in ip.chars() {
        v = v * 16.0 + c.to_digit(16)? as f64;
    }
    let mut scale = 1.0 / 16.0;
    for c in fp.chars() {
        v += c.to_digit(16)? as f64 * scale;
        scale /= 16.0;
    }
    Some(v * 2f64.powi(ex))
}

/// `Float.parseFloat`.
pub fn parse_float(s: &str) -> Option<f32> {
    let t = java_trim(s);
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    if body == "NaN" || body == "Infinity" || body.starts_with("0x") || body.starts_with("0X") {
        return parse_double(s).map(|d| d as f32);
    }
    // parse directly as f32 to avoid double rounding
    let b = body.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(body);
    parse_double(s)?;
    let v: f32 = b.parse().ok()?;
    Some(if t.starts_with('-') { -v } else { v })
}

/// `Long.parseLong(s, radix)`; `None` is a `NumberFormatException`.
pub fn parse_long(s: &str, radix: u32) -> Option<i64> {
    let b = s.as_bytes();
    if b.is_empty() || !(2..=36).contains(&radix) {
        return None;
    }
    let (neg, digits) = match b[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    let mut acc: i64 = 0;
    for c in digits.chars() {
        // `Character.digit`: any Unicode decimal digit counts
        let d = crate::unicode::digit(c as i32, radix as i32);
        if d < 0 {
            return None;
        }
        let d = d as i64;
        // accumulate negatively like the JDK so Long.MIN_VALUE parses
        acc = acc.checked_mul(radix as i64)?.checked_sub(d)?;
    }
    if neg {
        Some(acc)
    } else {
        acc.checked_neg()
    }
}

/// `Integer.parseInt(s, radix)`.
pub fn parse_int(s: &str, radix: u32) -> Option<i32> {
    let v = parse_long(s, radix)?;
    i32::try_from(v).ok()
}

/// Message of the `NumberFormatException` the JDK throws for a bad number string.
pub fn number_format_message(s: &str, radix: u32) -> String {
    if radix == 10 {
        format!("For input string: \"{s}\"")
    } else {
        format!("For input string: \"{s}\" under radix {radix}")
    }
}

/// `String.hashCode` over UTF-16 code units.
pub fn string_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    if s.is_ascii() {
        for &b in s.as_bytes() {
            h = h.wrapping_mul(31).wrapping_add(b as i32);
        }
    } else {
        for u in s.encode_utf16() {
            h = h.wrapping_mul(31).wrapping_add(u as i32);
        }
    }
    h
}

pub fn double_hash(d: f64) -> i32 {
    let bits = if d.is_nan() { 0x7ff8000000000000u64 } else { d.to_bits() };
    (bits ^ (bits >> 32)) as i32
}

pub fn float_hash(f: f32) -> i32 {
    if f.is_nan() {
        0x7fc00000
    } else {
        f.to_bits() as i32
    }
}

pub fn long_hash(v: i64) -> i32 {
    (v ^ ((v as u64) >> 32) as i64) as i32
}

/// Number of UTF-16 code units in `s`.
pub fn utf16_len(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.chars().map(char::len_utf16).sum()
    }
}

/// Byte offset of UTF-16 index `idx` (which may equal the length). `None` when out of range;
/// an index inside a surrogate pair rounds down to the start of the character.
pub fn utf16_to_byte(s: &str, idx: usize) -> Option<usize> {
    if s.is_ascii() {
        return if idx <= s.len() { Some(idx) } else { None };
    }
    let mut u = 0usize;
    for (b, c) in s.char_indices() {
        if u >= idx {
            return Some(b);
        }
        u += c.len_utf16();
        if u > idx {
            return Some(b);
        }
    }
    if u == idx {
        Some(s.len())
    } else {
        None
    }
}

/// UTF-16 code unit at `idx`.
pub fn utf16_at(s: &str, idx: usize) -> Option<u16> {
    if s.is_ascii() {
        return s.as_bytes().get(idx).map(|&b| b as u16);
    }
    s.encode_utf16().nth(idx)
}

/// UTF-16 index of a byte offset.
pub fn byte_to_utf16(s: &str, byte: usize) -> usize {
    if s.is_ascii() {
        byte
    } else {
        s[..byte].chars().map(char::len_utf16).sum()
    }
}

/// The string for a single UTF-16 code unit (`String.valueOf(char)`); lone surrogates become
/// U+FFFD since Rust strings cannot hold them.
pub fn char_to_string(c: u16) -> String {
    char::from_u32(c as u32).map(|c| c.to_string()).unwrap_or_else(|| "\u{FFFD}".into())
}

/// `String.compareTo`: lexicographic over UTF-16 code units.
pub fn compare_strings(a: &str, b: &str) -> i32 {
    if a.is_ascii() && b.is_ascii() {
        let (x, y) = (a.as_bytes(), b.as_bytes());
        let n = x.len().min(y.len());
        for i in 0..n {
            if x[i] != y[i] {
                return x[i] as i32 - y[i] as i32;
            }
        }
        return x.len() as i32 - y.len() as i32;
    }
    let mut ia = a.encode_utf16();
    let mut ib = b.encode_utf16();
    loop {
        match (ia.next(), ib.next()) {
            (Some(x), Some(y)) if x != y => return x as i32 - y as i32,
            (Some(_), Some(_)) => {}
            (None, None) => return 0,
            (Some(_), None) => return 1 + ia.count() as i32,
            (None, Some(_)) => return -(1 + ib.count() as i32),
        }
    }
}

/// `Character.toUpperCase(char)` / `toLowerCase` on one code unit.
pub fn char_upper(c: u16) -> u16 {
    match char::from_u32(c as u32) {
        Some(ch) => {
            let mut up = ch.to_uppercase();
            match (up.next(), up.next()) {
                (Some(u), None) if (u as u32) <= 0xFFFF => u as u16,
                _ => c,
            }
        }
        None => c,
    }
}

pub fn char_lower(c: u16) -> u16 {
    match char::from_u32(c as u32) {
        Some(ch) => {
            let mut lo = ch.to_lowercase();
            match (lo.next(), lo.next()) {
                (Some(u), None) if (u as u32) <= 0xFFFF => u as u16,
                _ => c,
            }
        }
        None => c,
    }
}

/// `Integer.toString(i, radix)`.
pub fn int_to_string_radix(v: i64, radix: u32) -> String {
    let radix = if (2..=36).contains(&radix) { radix } else { 10 };
    if v == 0 {
        return "0".into();
    }
    let neg = v < 0;
    let mut n = (v as i128).unsigned_abs();
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(std::char::from_digit((n % radix as u128) as u32, radix).unwrap());
        n /= radix as u128;
    }
    let mut s = String::with_capacity(digits.len() + 1);
    if neg {
        s.push('-');
    }
    s.extend(digits.iter().rev());
    s
}

/// `Long.toUnsignedString(v, radix)` and friends.
pub fn unsigned_to_string_radix(v: u64, radix: u32) -> String {
    let radix = if (2..=36).contains(&radix) { radix } else { 10 };
    if v == 0 {
        return "0".into();
    }
    let mut n = v;
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(std::char::from_digit((n % radix as u64) as u32, radix).unwrap());
        n /= radix as u64;
    }
    digits.iter().rev().collect()
}

/// Java's `(int)` conversion of a double: NaN to 0, saturating at the int range.
pub fn d2i(d: f64) -> i32 {
    d as i32
}
pub fn d2l(d: f64) -> i64 {
    d as i64
}

/// `Math.round(double)`: floor(x + 0.5) with Java's exact tie handling.
pub fn math_round(d: f64) -> i64 {
    if d.is_nan() {
        return 0;
    }
    let f = d.floor();
    let diff = d - f;
    let r = if diff >= 0.5 { f + 1.0 } else { f };
    r as i64
}

/// `Math.rint`: round half to even.
pub fn math_rint(d: f64) -> f64 {
    d.round_ties_even()
}

/// `Math.signum`.
pub fn math_signum(d: f64) -> f64 {
    if d.is_nan() || d == 0.0 {
        d
    } else if d > 0.0 {
        1.0
    } else {
        -1.0
    }
}

/// `Math.IEEEremainder`.
pub fn ieee_remainder(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() || x.is_infinite() || y == 0.0 {
        return f64::NAN;
    }
    if y.is_infinite() {
        return x;
    }
    let n = (x / y).round_ties_even();
    let r = x - n * y;
    if r == 0.0 {
        0.0f64.copysign(x)
    } else {
        r
    }
}

/// `Math.ulp(double)`.
pub fn ulp(d: f64) -> f64 {
    if d.is_nan() {
        return f64::NAN;
    }
    if d.is_infinite() {
        return f64::INFINITY;
    }
    let a = d.abs();
    if a == f64::MAX {
        return 2f64.powi(971);
    }
    f64::from_bits(a.to_bits() + 1) - a
}

pub fn next_up(d: f64) -> f64 {
    if d.is_nan() || d == f64::INFINITY {
        d
    } else if d == 0.0 {
        f64::from_bits(1)
    } else if d > 0.0 {
        f64::from_bits(d.to_bits() + 1)
    } else {
        f64::from_bits(d.to_bits() - 1)
    }
}

pub fn next_down(d: f64) -> f64 {
    if d.is_nan() || d == f64::NEG_INFINITY {
        d
    } else if d == 0.0 {
        -f64::from_bits(1)
    } else if d > 0.0 {
        f64::from_bits(d.to_bits() - 1)
    } else {
        f64::from_bits(d.to_bits() + 1)
    }
}

pub fn next_after(start: f64, direction: f64) -> f64 {
    if start.is_nan() || direction.is_nan() {
        f64::NAN
    } else if start == direction {
        direction
    } else if start < direction {
        next_up(start)
    } else {
        next_down(start)
    }
}

/// `Double.toHexString`.
pub fn double_to_hex(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let neg = d.is_sign_negative();
    let a = d.abs();
    let sign = if neg { "-" } else { "" };
    if a == 0.0 {
        return format!("{sign}0x0.0p0");
    }
    let bits = a.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let mant = bits & ((1u64 << 52) - 1);
    let mut hex = format!("{mant:013x}");
    while hex.len() > 1 && hex.ends_with('0') {
        hex.pop();
    }
    if exp == 0 {
        format!("{sign}0x0.{hex}p-1022")
    } else {
        format!("{sign}0x1.{hex}p{}", exp - 1023)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_to_string_matches_java() {
        assert_eq!(fmt_double(1.0 / 3.0), "0.3333333333333333");
        assert_eq!(fmt_double(1e20), "1.0E20");
        assert_eq!(fmt_double(100.0 * 1000000.0), "1.0E8");
        assert_eq!(fmt_double(0.001), "0.001");
        assert_eq!(fmt_double(0.0001), "1.0E-4");
        assert_eq!(fmt_double(1.0), "1.0");
        assert_eq!(fmt_double(-2.5), "-2.5");
        assert_eq!(fmt_double(1234567.0), "1234567.0");
        assert_eq!(fmt_double(12345678.0), "1.2345678E7");
        assert_eq!(fmt_double(1.5e-10), "1.5E-10");
        assert_eq!(fmt_double(f64::MAX), "1.7976931348623157E308");
        assert_eq!(fmt_double(f64::MIN_POSITIVE), "2.2250738585072014E-308");
        assert_eq!(fmt_double(4.9e-324), "4.9E-324");
        assert_eq!(fmt_double(100.0), "100.0");
        assert_eq!(fmt_double(0.1 + 0.2), "0.30000000000000004");
    }

    #[test]
    fn float_to_string_matches_java() {
        assert_eq!(fmt_float(1.0 / 3.0), "0.33333334");
        assert_eq!(fmt_float(2.5), "2.5");
        assert_eq!(fmt_float(100000.0), "100000.0");
        assert_eq!(fmt_float(1.0e10), "1.0E10");
        assert_eq!(fmt_float(0.1), "0.1");
    }

    #[test]
    fn parsing() {
        assert_eq!(parse_int("-2147483648", 10), Some(i32::MIN));
        assert_eq!(parse_int("2147483648", 10), None);
        assert_eq!(parse_int("+7", 10), Some(7));
        assert_eq!(parse_int("", 10), None);
        assert_eq!(parse_int("ff", 16), Some(255));
        assert_eq!(parse_long("-9223372036854775808", 10), Some(i64::MIN));
        assert_eq!(parse_double(" 1.5d "), Some(1.5));
        assert_eq!(parse_double("1e3"), Some(1000.0));
        assert_eq!(parse_double("abc"), None);
        assert_eq!(parse_double("-Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(parse_double("0x1.8p1"), Some(3.0));
        assert_eq!(parse_double(".5"), Some(0.5));
        assert_eq!(parse_double("5."), Some(5.0));
        assert_eq!(parse_double("."), None);
    }

    #[test]
    fn hashing() {
        assert_eq!(string_hash("hello"), 99162322);
        assert_eq!(long_hash(1), 1);
        assert_eq!(long_hash(-1), 0);
        assert_eq!(double_hash(1.0), 1072693248);
    }

    #[test]
    fn rounding() {
        assert_eq!(math_round(2.5), 3);
        assert_eq!(math_round(-2.5), -2);
        assert_eq!(math_round(0.49999999999999994), 0);
        assert_eq!(math_rint(2.5), 2.0);
    }
}
