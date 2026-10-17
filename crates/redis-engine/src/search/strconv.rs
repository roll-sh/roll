//! RediSearch's number parsing: `fast_float_strtod` (fast_float's `from_chars` in its general
//! format, built with `FASTFLOAT_ALLOWS_LEADING_PLUS`) and `util/strconv.h`'s `ParseDouble`,
//! `ParseInteger` and `ParseBoolean` over it.

/// Why `from_chars` failed (`errno` after `fast_float_strtod`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfErr {
    /// `ERANGE`: the value overflowed to infinity or underflowed to zero.
    Range,
    /// `EINVAL`: no number at all.
    Invalid,
}

/// `fast_float::from_chars` over `s`: the value, the bytes consumed and the error. An invalid
/// input consumes nothing and yields 0; an out-of-range one still yields its infinity or zero.
pub fn from_chars(s: &[u8]) -> (f64, usize, Option<FfErr>) {
    let invalid = (0.0, 0, Some(FfErr::Invalid));
    if s.is_empty() {
        return invalid;
    }
    let digit = |i: usize| s.get(i).is_some_and(u8::is_ascii_digit);
    let mut p = 0;
    let mut valid = true;
    if matches!(s[0], b'-' | b'+') {
        p = 1;
        if p == s.len() || !(digit(p) || s[p] == b'.') {
            valid = false;
        }
    }
    let mut nonzero = false;
    let mut digits = 0;
    if valid {
        while digit(p) {
            nonzero |= s[p] != b'0';
            digits += 1;
            p += 1;
        }
        if s.get(p) == Some(&b'.') {
            p += 1;
            while digit(p) {
                nonzero |= s[p] != b'0';
                digits += 1;
                p += 1;
            }
        }
        valid = digits > 0;
    }
    if !valid {
        return infnan(s);
    }
    if matches!(s.get(p), Some(b'e' | b'E')) {
        let mut q = p + 1;
        if matches!(s.get(q), Some(b'-' | b'+')) {
            q += 1;
        }
        if digit(q) {
            while digit(q) {
                q += 1;
            }
            p = q;
        }
    }
    let text = std::str::from_utf8(&s[..p]).expect("ASCII");
    let v: f64 = text.parse().unwrap_or(0.0);
    let err = if v.is_infinite() || (v == 0.0 && nonzero) { Some(FfErr::Range) } else { None };
    (v, p, err)
}

/// `parse_infnan`: `nan`, `nan(chars)`, `inf` and `infinity`, any case, after an optional sign.
fn infnan(s: &[u8]) -> (f64, usize, Option<FfErr>) {
    let neg = s[0] == b'-';
    let mut first = usize::from(matches!(s[0], b'-' | b'+'));
    let rest = &s[first..];
    if rest.len() >= 3 {
        if rest[..3].eq_ignore_ascii_case(b"nan") {
            first += 3;
            let mut end = first;
            if s.get(first) == Some(&b'(') {
                for (k, &c) in s.iter().enumerate().skip(first + 1) {
                    if c == b')' {
                        end = k + 1;
                        break;
                    } else if !(c.is_ascii_alphanumeric() || c == b'_') {
                        break;
                    }
                }
            }
            return (if neg { -f64::NAN } else { f64::NAN }, end, None);
        }
        if rest[..3].eq_ignore_ascii_case(b"inf") {
            let n = if rest.len() >= 8 && rest[3..8].eq_ignore_ascii_case(b"inity") { 8 } else { 3 };
            return (if neg { f64::NEG_INFINITY } else { f64::INFINITY }, first + n, None);
        }
    }
    (0.0, 0, Some(FfErr::Invalid))
}

/// `fast_float_strtod` on a C string: reading stops at the first NUL.
pub fn strtod(s: &[u8]) -> (f64, usize, Option<FfErr>) {
    from_chars(crate::util::cstr(s))
}

fn c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// musl's `strtod` on a C string: the value and the bytes consumed (0 when nothing parsed).
/// Leading spaces and a sign, `inf`/`infinity`/`nan`/`nan(…)` in any case, hex floats with an
/// optional binary exponent, decimals; overflow gives ±`HUGE_VAL`.
pub fn c_strtod(s: &[u8]) -> (f64, usize) {
    let s = crate::util::cstr(s);
    let mut i = 0;
    while i < s.len() && c_space(s[i]) {
        i += 1;
    }
    let neg = s.get(i) == Some(&b'-');
    if matches!(s.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let sign = |v: f64| if neg { -v } else { v };
    let rest = &s[i..];
    let starts = |w: &[u8]| rest.len() >= w.len() && rest[..w.len()].eq_ignore_ascii_case(w);
    if starts(b"inf") {
        let n = if starts(b"infinity") { 8 } else { 3 };
        return (sign(f64::INFINITY), i + n);
    }
    if starts(b"nan") {
        let mut end = i + 3;
        if s.get(end) == Some(&b'(') {
            let mut k = end + 1;
            while k < s.len() && (s[k].is_ascii_alphanumeric() || s[k] == b'_') {
                k += 1;
            }
            if s.get(k) == Some(&b')') {
                end = k + 1;
            }
        }
        return (sign(f64::NAN), end);
    }
    if starts(b"0x") {
        let digits = |k: usize| s.get(k).is_some_and(u8::is_ascii_hexdigit);
        let mut k = i + 2;
        let mut mant: f64 = 0.0;
        let mut exp: i64 = 0;
        let mut any = false;
        while digits(k) {
            mant = mant * 16.0 + (s[k] as char).to_digit(16).unwrap_or(0) as f64;
            any = true;
            k += 1;
        }
        if s.get(k) == Some(&b'.') {
            let mut j = k + 1;
            let mut frac = false;
            while digits(j) {
                mant = mant * 16.0 + (s[j] as char).to_digit(16).unwrap_or(0) as f64;
                exp -= 4;
                frac = true;
                j += 1;
            }
            if any || frac {
                any = true;
                k = j;
            }
        }
        if !any {
            // Only the "0" parses.
            return (sign(0.0), i + 1);
        }
        if matches!(s.get(k), Some(b'p' | b'P')) {
            let mut j = k + 1;
            let eneg = s.get(j) == Some(&b'-');
            if matches!(s.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if s.get(j).is_some_and(u8::is_ascii_digit) {
                let mut e: i64 = 0;
                while s.get(j).is_some_and(u8::is_ascii_digit) {
                    e = (e * 10 + (s[j] - b'0') as i64).min(1 << 40);
                    j += 1;
                }
                exp += if eneg { -e } else { e };
                k = j;
            }
        }
        let v = mant * 2f64.powi(exp.clamp(-2200, 2200) as i32);
        return (sign(v), k);
    }
    let digit = |k: usize| s.get(k).is_some_and(u8::is_ascii_digit);
    let mut k = i;
    let mut any = false;
    while digit(k) {
        k += 1;
        any = true;
    }
    if s.get(k) == Some(&b'.') {
        let mut j = k + 1;
        let mut frac = false;
        while digit(j) {
            j += 1;
            frac = true;
        }
        if any || frac {
            any = true;
            k = j;
        }
    }
    if !any {
        return (0.0, 0);
    }
    if matches!(s.get(k), Some(b'e' | b'E')) {
        let mut j = k + 1;
        if matches!(s.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        if digit(j) {
            while digit(j) {
                j += 1;
            }
            k = j;
        }
    }
    let text = std::str::from_utf8(&s[i..k]).expect("ASCII");
    let v: f64 = text.parse().unwrap_or(0.0);
    (sign(v), k)
}

/// `ParseDouble(arg, &d, sign)`: the whole C string must be a number in range; "" is 0.
pub fn parse_double(arg: &[u8], sign: i32) -> Option<f64> {
    let arg = crate::util::cstr(arg);
    if arg.is_empty() {
        return Some(0.0);
    }
    let (d, n, err) = from_chars(arg);
    if err.is_some() || n != arg.len() {
        return None;
    }
    Some(if sign == -1 { -d } else { d })
}

/// `ParseInteger`: `strtoll(arg, &e, 10)` over the whole C string (leading spaces and a sign
/// allowed, no overflow, no trailing bytes).
pub fn parse_integer(arg: &[u8]) -> Option<i64> {
    let arg = crate::util::cstr(arg);
    let mut i = 0;
    while i < arg.len() && matches!(arg[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let neg = arg.get(i) == Some(&b'-');
    if matches!(arg.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    let st = i;
    let mut v: i128 = 0;
    while i < arg.len() && arg[i].is_ascii_digit() {
        v = (v * 10 + i128::from(arg[i] - b'0')).min(i128::from(u64::MAX) * 2);
        i += 1;
    }
    if i == st {
        // No digits: strtoll leaves `e` at the start, so only "" would pass (and it's 0).
        return arg.is_empty().then_some(0);
    }
    if i != arg.len() {
        return None;
    }
    let v = if neg { -v } else { v };
    i64::try_from(v).ok()
}

/// `ParseBoolean`: `true`/`1` or `false`/`0`, any case.
pub fn parse_boolean(arg: &[u8]) -> Option<bool> {
    let arg = crate::util::cstr(arg);
    if arg.eq_ignore_ascii_case(b"true") || arg == b"1" {
        Some(true)
    } else if arg.eq_ignore_ascii_case(b"false") || arg == b"0" {
        Some(false)
    } else {
        None
    }
}

/// C's `%f`.
pub fn fmt_f(v: f64) -> String {
    if v.is_nan() {
        return if v.is_sign_negative() { "-nan" } else { "nan" }.into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf" } else { "inf" }.into();
    }
    format!("{v:.6}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_chars_like_fast_float() {
        assert_eq!(from_chars(b"1.5x"), (1.5, 3, None));
        assert_eq!(from_chars(b"+.5"), (0.5, 3, None));
        assert_eq!(from_chars(b"5."), (5.0, 2, None));
        assert_eq!(from_chars(b"1e"), (1.0, 1, None));
        assert_eq!(from_chars(b"1e+"), (1.0, 1, None));
        assert_eq!(from_chars(b"1e-2"), (0.01, 4, None));
        assert_eq!(from_chars(b"."), (0.0, 0, Some(FfErr::Invalid)));
        assert_eq!(from_chars(b"-"), (0.0, 0, Some(FfErr::Invalid)));
        assert_eq!(from_chars(b"+-1"), (0.0, 0, Some(FfErr::Invalid)));
        assert_eq!(from_chars(b" 1"), (0.0, 0, Some(FfErr::Invalid)));
        assert_eq!(from_chars(b"1e400"), (f64::INFINITY, 5, Some(FfErr::Range)));
        assert_eq!(from_chars(b"1e-400"), (0.0, 6, Some(FfErr::Range)));
        assert_eq!(from_chars(b"0e-400"), (0.0, 6, None));
        assert_eq!(from_chars(b"-Infinity"), (f64::NEG_INFINITY, 9, None));
        assert_eq!(from_chars(b"+infin"), (f64::INFINITY, 4, None));
        let (v, n, e) = from_chars(b"nan(abc)x");
        assert!(v.is_nan() && n == 8 && e.is_none());
        let (v, n, _) = from_chars(b"NaN(a-b)");
        assert!(v.is_nan() && n == 3);
    }

    #[test]
    fn parse_helpers() {
        assert_eq!(parse_double(b"", 1), Some(0.0));
        assert_eq!(parse_double(b"2", -1), Some(-2.0));
        assert_eq!(parse_double(b"2 ", 1), None);
        assert_eq!(parse_double(b"1e999", 1), None);
        assert_eq!(parse_double(b"inf", 1), Some(f64::INFINITY));
        assert_eq!(parse_integer(b" -12"), Some(-12));
        assert_eq!(parse_integer(b"+7"), Some(7));
        assert_eq!(parse_integer(b"7x"), None);
        assert_eq!(parse_integer(b"9223372036854775808"), None);
        assert_eq!(parse_integer(b"-9223372036854775808"), Some(i64::MIN));
        assert_eq!(parse_integer(b"-"), None);
        assert_eq!(parse_boolean(b"TRUE"), Some(true));
        assert_eq!(parse_boolean(b"0"), Some(false));
        assert_eq!(parse_boolean(b"yes"), None);
        assert_eq!(fmt_f(1.0), "1.000000");
        assert_eq!(fmt_f(f64::NEG_INFINITY), "-inf");
    }

    #[test]
    fn c_strtod_like_musl() {
        assert_eq!(c_strtod(b"  -1.5e2x"), (-150.0, 8));
        assert_eq!(c_strtod(b""), (0.0, 0));
        assert_eq!(c_strtod(b"0x1p4"), (16.0, 5));
        assert_eq!(c_strtod(b"0x.8"), (0.5, 4));
        assert_eq!(c_strtod(b"0xg"), (0.0, 1));
        assert_eq!(c_strtod(b"1e"), (1.0, 1));
        assert_eq!(c_strtod(b"INFINITY"), (f64::INFINITY, 8));
        assert_eq!(c_strtod(b"1e999").0, f64::INFINITY);
        let (v, n) = c_strtod(b"nan(x)");
        assert!(v.is_nan() && n == 6);
        assert_eq!(c_strtod(b"5\0x"), (5.0, 1));
    }
}
