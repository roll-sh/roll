//! `float` / `double` text forms.
//!
//! - Cassandra prints with Java's `Float.toString` / `Double.toString`: plain with at least one
//!   fraction digit for `1e-3 <= |x| < 1e7` (`100.0`, `0.001`), otherwise computerized scientific
//!   (`1.0E7`, `1.0E-4`); `NaN`, `Infinity`, `-Infinity`, `-0.0`.
//! - ScyllaDB prints the shortest round-trip digits in `{fmt}`'s layout: scientific when the
//!   decimal exponent is below -4 or at least 7 (float) / 16 (double), with a sign and at least two
//!   exponent digits (`1e+07`, `1.5e-05`), plain otherwise without a trailing `.0`; `nan`, `inf`,
//!   `-inf`, `-0`.
//!
//! ScyllaDB uses the shortest digits that round-trip. Java 17 (the Cassandra image's JVM) mostly
//! does too, except that integral values print their exact digits (minus the ones a float cannot
//! hold: `123456789f` is `1.23456792E8`, `2^60` is `1.15292150460684698E18`), which we reproduce;
//! subnormals print longer (`4.9E-324`, `1.4E-45`, special-cased for the two extremes) and a
//! handful of other values get one extra digit, which we do not.

/// Shortest round-trip digits and decimal exponent of a finite, non-zero magnitude:
/// `x = d.ddd × 10^exp`.
fn shortest(s: String) -> (String, i32) {
    // Rust's `{:e}` is the shortest representation that round-trips: `1.2345679e8`.
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    (digits, exp.parse().unwrap_or(0))
}

fn digits_f64(x: f64) -> (String, i32) {
    shortest(format!("{:e}", x.abs()))
}

fn digits_f32(x: f32) -> (String, i32) {
    shortest(format!("{:e}", x.abs()))
}

/// Java's exact-integer digits: for an integral value with binary exponent in `0..=62`, the
/// exact value, rounded to drop the decimal digits the significand cannot carry. `mant` has its
/// leading bit at position 52; `n_sig` is the significand width (53 double, 24 float).
fn java_integer_digits(mant: u64, bin_exp: i32, n_sig: i32) -> Option<(String, i32)> {
    if mant == 0 || !(0..=62).contains(&bin_exp) {
        return None;
    }
    let n_fract_bits = 53 - mant.trailing_zeros() as i32;
    if n_fract_bits - bin_exp - 1 > 0 {
        return None;
    }
    let p2 = bin_exp - n_sig - 1;
    let insig = if bin_exp > n_sig && p2 > 1 {
        (p2 as f64 * std::f64::consts::LOG10_2).floor() as u32
    } else {
        0
    };
    let mut v = if bin_exp >= 52 {
        mant << (bin_exp - 52)
    } else {
        mant >> (52 - bin_exp)
    };
    if insig > 0 {
        let p = 10u64.pow(insig);
        let r = v % p;
        v /= p;
        if r >= p / 2 {
            v += 1;
        }
    }
    let all = v.to_string();
    let exp = all.len() as i32 - 1 + insig as i32;
    let digits = all.trim_end_matches('0');
    Some((
        if digits.is_empty() {
            "0".into()
        } else {
            digits.into()
        },
        exp,
    ))
}

fn java_digits_f64(x: f64) -> (String, i32) {
    let bits = x.abs().to_bits();
    let e = ((bits >> 52) & 0x7ff) as i32;
    if e != 0
        && let Some(d) = java_integer_digits((bits & ((1 << 52) - 1)) | (1 << 52), e - 1023, 53)
    {
        return d;
    }
    digits_f64(x)
}

fn java_digits_f32(x: f32) -> (String, i32) {
    let bits = x.abs().to_bits();
    let e = ((bits >> 23) & 0xff) as i32;
    if e != 0 {
        let mant = (((bits & ((1 << 23) - 1)) | (1 << 23)) as u64) << 29;
        if let Some(d) = java_integer_digits(mant, e - 127, 24) {
            return d;
        }
    }
    digits_f32(x)
}

fn java_layout(neg: bool, digits: &str, exp: i32) -> String {
    let mut out = String::with_capacity(digits.len() + 8);
    if neg {
        out.push('-');
    }
    if (-3..7).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() > int_len {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            } else {
                out.push_str(digits);
                out.extend(std::iter::repeat_n('0', int_len - digits.len()));
                out.push_str(".0");
            }
        } else {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-exp - 1) as usize));
            out.push_str(digits);
        }
    } else {
        out.push_str(&digits[..1]);
        out.push('.');
        out.push_str(if digits.len() > 1 { &digits[1..] } else { "0" });
        out.push('E');
        out.push_str(&exp.to_string());
    }
    out
}

/// `Double.toString` (Java 17).
pub fn java_double(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    if x.abs() == f64::from_bits(1) {
        return if x < 0.0 {
            "-4.9E-324".into()
        } else {
            "4.9E-324".into()
        };
    }
    let (d, e) = java_digits_f64(x);
    java_layout(x < 0.0, &d, e)
}

/// `Float.toString` (Java 17).
pub fn java_float(x: f32) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    if x.abs() == f32::from_bits(1) {
        return if x < 0.0 {
            "-1.4E-45".into()
        } else {
            "1.4E-45".into()
        };
    }
    let (d, e) = java_digits_f32(x);
    java_layout(x < 0.0, &d, e)
}

fn scylla_layout(neg: bool, digits: &str, exp: i32, sci_from: i32) -> String {
    let mut out = String::with_capacity(digits.len() + 8);
    if neg {
        out.push('-');
    }
    if exp < -4 || exp >= sci_from {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        let a = exp.unsigned_abs();
        if a < 10 {
            out.push('0');
        }
        out.push_str(&a.to_string());
    } else if exp >= 0 {
        let int_len = exp as usize + 1;
        if digits.len() > int_len {
            out.push_str(&digits[..int_len]);
            out.push('.');
            out.push_str(&digits[int_len..]);
        } else {
            out.push_str(digits);
            out.extend(std::iter::repeat_n('0', int_len - digits.len()));
        }
    } else {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-exp - 1) as usize));
        out.push_str(digits);
    }
    out
}

fn scylla_special(nan: bool, inf: bool, zero: bool, neg: bool) -> Option<String> {
    if nan {
        return Some("nan".into());
    }
    if inf {
        return Some(if neg { "-inf".into() } else { "inf".into() });
    }
    if zero {
        return Some(if neg { "-0".into() } else { "0".into() });
    }
    None
}

/// ScyllaDB's `double` text.
pub fn scylla_double(x: f64) -> String {
    if let Some(s) = scylla_special(x.is_nan(), x.is_infinite(), x == 0.0, x.is_sign_negative()) {
        return s;
    }
    let (d, e) = digits_f64(x);
    scylla_layout(x < 0.0, &d, e, 16)
}

/// ScyllaDB's `float` text.
pub fn scylla_float(x: f32) -> String {
    if let Some(s) = scylla_special(x.is_nan(), x.is_infinite(), x == 0.0, x.is_sign_negative()) {
        return s;
    }
    let (d, e) = digits_f32(x);
    scylla_layout(x < 0.0, &d, e, 7)
}

/// Normalises Java's `Double.parseDouble` grammar into something Rust's correctly rounded
/// parser accepts: surrounding characters `<= ' '` are trimmed, `NaN` / `Infinity` take an
/// optional sign, decimals may end in `f`, `F`, `d` or `D`. Hexadecimal floats are not accepted.
fn java_float_text(s: &str) -> Option<String> {
    let t = s.trim_matches(|c: char| c <= ' ');
    let (sign, body) = match t.as_bytes().first()? {
        b'-' => ("-", &t[1..]),
        b'+' => ("", &t[1..]),
        _ => ("", t),
    };
    if body == "NaN" {
        return Some("NaN".into());
    }
    if body == "Infinity" {
        return Some(format!("{sign}inf"));
    }
    let body = body.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(body);
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    if !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(e) = exp {
        let d = e.strip_prefix(['-', '+']).unwrap_or(e);
        if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some(format!("{sign}{body}"))
}

/// `Double.parseDouble`.
pub fn parse_java_double(s: &str) -> Option<f64> {
    java_float_text(s)?.parse().ok()
}

/// `Float.parseFloat` (rounded once, directly to `f32`).
pub fn parse_java_float(s: &str) -> Option<f32> {
    java_float_text(s)?.parse().ok()
}

/// ScyllaDB's float grammar: an optional sign, then `nan`, `inf`, `infinity` (any case) or a
/// decimal with an optional exponent. No surrounding blanks; a finite text that overflows is an
/// error.
fn scylla_float_text(s: &str) -> Option<(String, bool)> {
    let body = s.strip_prefix(['-', '+']).unwrap_or(s);
    let lower = body.to_ascii_lowercase();
    if lower == "nan" || lower == "inf" || lower == "infinity" {
        return Some((s.to_string(), true));
    }
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
    {
        return None;
    }
    if let Some(e) = exp {
        let d = e.strip_prefix(['-', '+']).unwrap_or(e);
        if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some((s.to_string(), false))
}

pub fn parse_scylla_double(s: &str) -> Option<f64> {
    let (t, special) = scylla_float_text(s)?;
    let v: f64 = t.parse().ok()?;
    (special || v.is_finite()).then_some(v)
}

pub fn parse_scylla_float(s: &str) -> Option<f32> {
    let (t, special) = scylla_float_text(s)?;
    let v: f32 = t.parse().ok()?;
    (special || v.is_finite()).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_doubles_as_cassandra_prints_them() {
        // CAST(d AS text) / toJson on cassandra:5.0.9. Known divergences (Java's non-shortest
        // general path), not reproduced: 1e23 -> 9.999999999999999E22, 1.17549435E-38f, 2.8E-45f.
        for (x, s) in [
            (1.1, "1.1"),
            (1e7, "1.0E7"),
            (1234567.0, "1234567.0"),
            (0.0001, "1.0E-4"),
            (0.001, "0.001"),
            (100.0, "100.0"),
            (4.9e-324, "4.9E-324"),
            (-1e300, "-1.0E300"),
            (0.1, "0.1"),
            (9999999.0, "9999999.0"),
            (123456.789, "123456.789"),
            (0.30000000000000004, "0.30000000000000004"),
            (1.7976931348623157e308, "1.7976931348623157E308"),
            (-0.0, "-0.0"),
            (0.0, "0.0"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
            (1e21, "1.0E21"),
            (1.5e-5, "1.5E-5"),
            (1152921504606846976.0, "1.15292150460684698E18"),
            (9007199254740992.0, "9.007199254740992E15"),
            (1e17, "1.0E17"),
            (123456789012345678.0, "1.2345678901234568E17"),
            (-9.223372036854776e18, "-9.223372036854776E18"),
            (4611686018427387904.0, "4.6116860184273879E18"),
            (1e22, "1.0E22"),
            (2e-323, "2.0E-323"),
        ] {
            assert_eq!(java_double(x), s, "{x:e}");
        }
    }

    #[test]
    fn java_floats_as_cassandra_prints_them() {
        for (x, s) in [
            (1.1f32, "1.1"),
            (1e7, "1.0E7"),
            (123456789.0, "1.23456792E8"),
            (0.001, "0.001"),
            (3.4028235e38, "3.4028235E38"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (f32::NAN, "NaN"),
            (1.4e-45, "1.4E-45"),
            (16777216.0, "1.6777216E7"),
            (-3.7, "-3.7"),
            (1e10, "1.0E10"),
            (2147483647.0, "2.14748365E9"),
            (33554436.0, "3.3554436E7"),
            (1e20, "1.0E20"),
        ] {
            assert_eq!(java_float(x), s, "{x:e}");
        }
    }

    #[test]
    fn scylla_numbers_as_scylla_prints_them() {
        // CAST(x AS text) on scylladb/scylla:2026.3.3.
        for (x, s) in [
            (1.1f32, "1.1"),
            (123456789.0, "1.2345679e+08"),
            (1e7, "1e+07"),
            (1e6, "1000000"),
            (1234567.0, "1234567"),
            (9999999.0, "9999999"),
            (12345678.0, "1.2345678e+07"),
            (16777216.0, "1.6777216e+07"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-5, "1.5e-05"),
            (100.0, "100"),
            (3e-45, "3e-45"),
            (0.3, "0.3"),
            (3.4028235e38, "3.4028235e+38"),
            (-0.0, "-0"),
            (f32::NAN, "nan"),
            (f32::NEG_INFINITY, "-inf"),
            (-2147483648.0, "-2.1474836e+09"),
            (300.0, "300"),
        ] {
            assert_eq!(scylla_float(x), s, "{x:e}");
        }
        for (x, s) in [
            (1.1f64, "1.1"),
            (1234567.0, "1234567"),
            (1e7, "10000000"),
            (1e15, "1000000000000000"),
            (1e16, "1e+16"),
            (1e17, "1e+17"),
            (9999999999999998.0, "9999999999999998"),
            (123456789012345678.0, "1.2345678901234568e+17"),
            (0.00001, "1e-05"),
            (0.1, "0.1"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (0.30000000000000004, "0.30000000000000004"),
            (123456.789, "123456.789"),
            (2e-7, "2e-07"),
            (5e-324, "5e-324"),
            (1e300, "1e+300"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::INFINITY, "inf"),
            (9007199254740992.0, "9007199254740992"),
            (-9.223372036854776e18, "-9.223372036854776e+18"),
        ] {
            assert_eq!(scylla_double(x), s, "{x:e}");
        }
    }

    #[test]
    fn java_parsing() {
        assert_eq!(parse_java_double("1.5"), Some(1.5));
        assert_eq!(parse_java_double(" 1e3 "), Some(1000.0));
        assert_eq!(parse_java_double("-Infinity"), Some(f64::NEG_INFINITY));
        assert!(parse_java_double("NaN").unwrap().is_nan());
        assert!(parse_java_double("-NaN").unwrap().is_nan());
        assert_eq!(parse_java_double("2d"), Some(2.0));
        assert_eq!(parse_java_double(".5"), Some(0.5));
        assert_eq!(parse_java_double("5."), Some(5.0));
        assert_eq!(parse_java_double("1e400"), Some(f64::INFINITY));
        assert_eq!(parse_java_float("3.4028236e38"), Some(f32::INFINITY));
        assert_eq!(parse_java_float("1.1"), Some(1.1f32));
        for bad in ["", "nan", "inf", "1e", "abc", "1.2.3", "0x10", "--1", "."] {
            assert!(parse_java_double(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn scylla_parsing() {
        assert_eq!(parse_scylla_float("Infinity"), Some(f32::INFINITY));
        assert_eq!(parse_scylla_float("-inf"), Some(f32::NEG_INFINITY));
        assert!(parse_scylla_double("nan").unwrap().is_nan());
        assert_eq!(parse_scylla_float("1e40"), None);
        assert_eq!(parse_scylla_double("1e400"), None);
        assert_eq!(parse_scylla_double("1.5e-3"), Some(0.0015));
        for bad in [" 5", "5 ", "0x10", "", "1f", "abc"] {
            assert!(parse_scylla_double(bad).is_none(), "{bad}");
        }
    }
}
