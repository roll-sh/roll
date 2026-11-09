//! How the module writes and reads sample values: RESP2 replies print doubles with its bundled
//! dragonbox (`dragonbox_double_to_chars`), and TS.ADD-like values go through
//! fast_double_parser plus a NaN spelling check (`parse_double`).

/// `dragonbox_double_to_chars`: the shortest round-trip digits, printed plainly when the
/// decimal point falls inside them (or right after) and the exponent dragonbox picked is in
/// [-16, 0], else as `d.dddE[-]x`.
pub fn double(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Infinity".into() } else { "Infinity".into() };
    }
    let mut out = String::new();
    if v.is_sign_negative() {
        out.push('-');
    }
    if v == 0.0 {
        out.push('0');
        return out;
    }
    let a = v.abs();
    // Shortest digits `d` with the value `d × 10^x`.
    let sci = format!("{a:e}");
    let (mant, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let lead: i32 = exp.parse().expect("exponent");
    let x = lead - (digits.len() as i32 - 1);

    // The exponent dragonbox's own significand carries (it doesn't strip trailing zeros for
    // doubles): the larger of its two candidate powers that still fits the digits.
    let bits = a.to_bits();
    let exp_bits = ((bits >> 52) & 0x7ff) as i64;
    let sig = bits & ((1 << 52) - 1);
    let e = if exp_bits == 0 { -1074 } else { exp_bits - 1075 };
    let e1 = if sig == 0 && exp_bits != 0 { ((e * 631305 - 261663) >> 21) + 1 } else { ((e * 315653) >> 20) + 1 } as i32;
    let dexp = if x >= e1 { e1 } else { (e1 - 1).min(x) };
    let n_digits = digits.len() as i32 + (x - dexp);

    if (-16..=0).contains(&dexp) && n_digits + dexp > 0 {
        let mut full = digits.clone();
        full.extend(std::iter::repeat_n('0', (x - dexp) as usize));
        let tz = full.bytes().rev().take(n_digits as usize - 1).take_while(|b| *b == b'0').count() as i32;
        let point = (n_digits + dexp) as usize;
        if tz + dexp >= 0 {
            out.push_str(&full[..point]);
        } else {
            out.push_str(&full[..point]);
            out.push('.');
            out.push_str(&full[point..(n_digits - tz) as usize]);
        }
        return out;
    }
    out.push_str(&digits[..1]);
    if digits.len() > 1 {
        out.push('.');
        out.push_str(&digits[1..]);
    }
    out.push('E');
    out.push_str(&lead.to_string());
    out
}

/// `parse_double`: fast_double_parser's grammar over the whole string (an optional `-`, digits
/// without a leading zero, an optional `.digits`, an optional exponent; a finite result), or a
/// NaN spelling (`nan`, `-nan`, `+nan`, any case).
pub fn parse_value(b: &[u8]) -> Option<f64> {
    if fast_number(b) {
        let s = std::str::from_utf8(b).ok()?;
        let v: f64 = s.parse().ok()?;
        if v.is_finite() {
            return Some(v);
        }
        return None;
    }
    let nan = match b.len() {
        3 => b.eq_ignore_ascii_case(b"nan"),
        4 => b.eq_ignore_ascii_case(b"-nan") || b.eq_ignore_ascii_case(b"+nan"),
        _ => false,
    };
    nan.then_some(f64::NAN)
}

fn fast_number(b: &[u8]) -> bool {
    let mut i = 0;
    if b.first() == Some(&b'-') {
        i += 1;
    }
    let digit = |i: usize| b.get(i).is_some_and(|c| c.is_ascii_digit());
    if !digit(i) {
        return false;
    }
    if b[i] == b'0' {
        i += 1;
        if digit(i) {
            return false;
        }
    } else {
        while digit(i) {
            i += 1;
        }
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digit(i) {
            return false;
        }
        while digit(i) {
            i += 1;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digit(i) {
            return false;
        }
        while digit(i) {
            i += 1;
        }
    }
    i == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_like_dragonbox() {
        for (v, s) in [
            (100.0, "100"),
            (0.5, "5E-1"),
            (1e16, "1E16"),
            (1e15, "1000000000000000"),
            (12345678901234568.0, "12345678901234568"),
            (0.1, "1E-1"),
            (99.99, "99.99"),
            (5e-324, "5E-324"),
            (-0.0, "-0"),
            (f64::NAN, "NaN"),
            (f64::NEG_INFINITY, "-Infinity"),
            (1.5, "1.5"),
            (-2.0, "-2"),
        ] {
            assert_eq!(double(v), s, "{v}");
        }
    }

    #[test]
    fn parses_like_the_module() {
        assert_eq!(parse_value(b"1.5"), Some(1.5));
        assert_eq!(parse_value(b"-0"), Some(-0.0));
        assert_eq!(parse_value(b"1e3"), Some(1000.0));
        assert!(parse_value(b"NaN").unwrap().is_nan());
        assert!(parse_value(b"+nan").unwrap().is_nan());
        for bad in [&b"01"[..], b"+1", b".5", b"5.", b"1e", b"inf", b"1e309", b"", b" 1", b"1x"] {
            assert_eq!(parse_value(bad), None, "{:?}", std::str::from_utf8(bad));
        }
    }
}
