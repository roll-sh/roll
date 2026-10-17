//! Number formatting and parsing exactly as Redis does it, so replies match byte for byte.

/// Format a double like Redis' `d2string` (Redis 7.2+): integral values in the `long long` range
/// print as integers, everything else as the shortest round-trip form in fpconv's notation
/// (`0.1`, `1e+20`, `1e-7`), and `inf` / `-inf` / `nan`.
pub fn fmt_double(d: f64) -> String {
    if d.is_nan() {
        return "nan".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if d == 0.0 {
        return if d.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    // Redis: integral values in the `long long` range print as integers.
    if d.fract() == 0.0 && d < 9.223_372_036_854_776e18 && d > -9.223_372_036_854_776e18 {
        return (d as i64).to_string();
    }
    fpconv(d)
}

/// fpconv_dtoa: grisu shortest digits, then `emit_digits` layout.
fn fpconv(d: f64) -> String {
    // Rust's `{:e}` gives the shortest round-trip digits too: "d.ddddde±x".
    let e = format!("{:e}", d.abs());
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let nd = digits.len() as i32;
    // value = digits * 10^k
    let k = exp - (nd - 1);
    let neg = d < 0.0;
    let mut out = String::with_capacity(24);
    if neg {
        out.push('-');
    }
    let absexp = (k + nd - 1).abs();
    if k >= 0 && absexp < nd + 7 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', k as usize));
        return out;
    }
    if k < 0 && (k > -7 || absexp < 4) {
        let offset = nd - k.abs();
        if offset <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-offset) as usize));
            out.push_str(&digits);
        } else {
            out.push_str(&digits[..offset as usize]);
            out.push('.');
            out.push_str(&digits[offset as usize..]);
        }
        return out;
    }
    let nd = nd.min(18 - neg as i32) as usize;
    out.push_str(&digits[..1]);
    if nd > 1 {
        out.push('.');
        out.push_str(&digits[1..nd]);
    }
    out.push('e');
    out.push(if k + digits.len() as i32 - 1 < 0 { '-' } else { '+' });
    let mut x = absexp;
    let mut cent = 0;
    if x > 99 {
        cent = x / 100;
        out.push((b'0' + cent as u8) as char);
        x -= cent * 100;
    }
    if x > 9 {
        let dec = x / 10;
        out.push((b'0' + dec as u8) as char);
        x -= dec * 10;
    } else if cent != 0 {
        out.push('0');
    }
    out.push((b'0' + (x % 10) as u8) as char);
    out
}

/// Parse a double the way Redis' `string2d` / `strtod` do: no surrounding whitespace, `inf`,
/// `+inf`, `-inf` and `infinity` accepted (any case), NaN rejected.
pub fn parse_double(s: &[u8]) -> Option<f64> {
    if s.is_empty() || s.len() > 5120 || s[0].is_ascii_whitespace() || s[s.len() - 1].is_ascii_whitespace() {
        return None;
    }
    let s = std::str::from_utf8(s).ok()?;
    let lower = s.to_ascii_lowercase();
    let body = lower.strip_prefix(['+', '-']).unwrap_or(&lower);
    let v = match body {
        "inf" | "infinity" => {
            if lower.starts_with('-') {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        }
        _ => {
            // Rust accepts "nan", "inf" spellings and "1e5"; reject the ones strtod wouldn't produce
            // as numbers here.
            if body.contains("nan") || body.contains("inf") || body.starts_with('_') {
                return None;
            }
            if let Some(hex) = body.strip_prefix("0x") {
                let v = parse_hex_float(hex)?;
                if lower.starts_with('-') { -v } else { v }
            } else {
                s.parse::<f64>().ok()?
            }
        }
    };
    if v.is_nan() { None } else { Some(v) }
}

/// strtod accepts C99 hex floats (`0x1.8p3`).
fn parse_hex_float(s: &str) -> Option<f64> {
    let (mant, exp) = match s.split_once('p') {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (s, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let mut v = 0f64;
    for c in int.chars() {
        v = v * 16.0 + c.to_digit(16)? as f64;
    }
    let mut scale = 1.0 / 16.0;
    for c in frac.chars() {
        v += c.to_digit(16)? as f64 * scale;
        scale /= 16.0;
    }
    Some(v * 2f64.powi(exp))
}

/// `long double` formatting used by INCRBYFLOAT / HINCRBYFLOAT (`%.17Lf`, trailing zeros removed).
/// Callers that need extended precision pass the exact decimal string instead; this is the
/// fallback for values computed in binary.
pub fn fmt_long_double(d: f64) -> String {
    if d.is_infinite() {
        return if d > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let mut s = format!("{d:.17}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" { "0".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_redis_d2string() {
        for (d, want) in [
            (1.5, "1.5"),
            (0.1, "0.1"),
            (3.0, "3"),
            (-2.0, "-2"),
            (1e20, "1e+20"),
            (1e17, "100000000000000000"),
            (123456789012345678.0, "123456789012345680"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (-1e-7, "-1e-7"),
            (1e21, "1e+21"),
            (1.5e300, "1.5e+300"),
            (1e-7, "1e-7"),
            (0.000001, "0.000001"),
            (123.456, "123.456"),
            (0.30000000000000004, "0.30000000000000004"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (-0.0, "-0"),
            (9007199254740993.0, "9007199254740992"),
            (3.14159e-10, "3.14159e-10"),
        ] {
            assert_eq!(fmt_double(d), want, "{d:e}");
        }
    }

    #[test]
    fn parses_like_strtod() {
        assert_eq!(parse_double(b"1.5"), Some(1.5));
        assert_eq!(parse_double(b"-inf"), Some(f64::NEG_INFINITY));
        assert_eq!(parse_double(b"+Inf"), Some(f64::INFINITY));
        assert_eq!(parse_double(b"1e3"), Some(1000.0));
        assert_eq!(parse_double(b".5"), Some(0.5));
        assert_eq!(parse_double(b"0x10"), Some(16.0));
        assert_eq!(parse_double(b"nan"), None);
        assert_eq!(parse_double(b" 1"), None);
        assert_eq!(parse_double(b"1 "), None);
        assert_eq!(parse_double(b"abc"), None);
        assert_eq!(parse_double(b""), None);
    }
}
