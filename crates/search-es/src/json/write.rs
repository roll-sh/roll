//! JSON output as Elasticsearch's Jackson generator writes it.
//!
//! - Strings escape `"` and `\`, use the short escapes `\b \t \n \f \r` and `\u00XX` (upper-case
//!   hex) for other control characters, and leave everything else (including non-ASCII) raw.
//! - Doubles print like Java's `Double.toString`: shortest round-trip digits, plain notation in
//!   `[1e-3, 1e7)` and `d.dddE±n` outside, always with a fractional part (`1.0`, `1.0E10`).
//! - `pretty` matches Jackson's `DefaultPrettyPrinter` as Elasticsearch configures it: two-space
//!   indents, `"key" : value`, one array element per line, `{ }` and `[ ]` for empty containers.

use super::{Number, Value};

pub fn write_json(out: &mut Vec<u8>, v: &Value, pretty: bool) {
    if pretty {
        write_pretty(out, v, 0);
    } else {
        write_compact(out, v);
    }
}

fn write_compact(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Obj(m) => {
            out.push(b'{');
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_str(out, k);
                out.push(b':');
                write_child(out, v);
            }
            out.push(b'}');
        }
        Value::Arr(a) => {
            out.push(b'[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_child(out, v);
            }
            out.push(b']');
        }
        Value::Str(s) if let Some(raw) = super::raw::as_raw(s) => out.extend_from_slice(raw.as_bytes()),
        scalar => write_scalar(out, scalar),
    }
}

/// Nested containers recurse through the stack guard (values nest up to 1000 levels).
#[inline]
fn write_child(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Obj(_) | Value::Arr(_) => crate::stack::maybe_grow(|| write_compact(out, v)),
        // Pre-rendered JSON (a stored `_source`) goes out as it is.
        Value::Str(s) if let Some(raw) = super::raw::as_raw(s) => out.extend_from_slice(raw.as_bytes()),
        scalar => write_scalar(out, scalar),
    }
}

#[inline]
fn write_pretty_child(out: &mut Vec<u8>, v: &Value, level: usize) {
    match v {
        Value::Obj(_) | Value::Arr(_) => crate::stack::maybe_grow(|| write_pretty(out, v, level)),
        // Pretty output re-reads pre-rendered JSON, as Jackson copies a raw field.
        Value::Str(s) if let Some(raw) = super::raw::as_raw(s) => {
            let tree = super::raw::parse(raw);
            crate::stack::maybe_grow(|| write_pretty(out, &tree, level))
        }
        scalar => write_scalar(out, scalar),
    }
}

fn indent(out: &mut Vec<u8>, level: usize) {
    out.push(b'\n');
    for _ in 0..level {
        out.extend_from_slice(b"  ");
    }
}

fn write_pretty(out: &mut Vec<u8>, v: &Value, level: usize) {
    match v {
        Value::Obj(m) if m.is_empty() => out.extend_from_slice(b"{ }"),
        Value::Obj(m) => {
            out.push(b'{');
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                indent(out, level + 1);
                write_str(out, k);
                out.extend_from_slice(b" : ");
                write_pretty_child(out, v, level + 1);
            }
            indent(out, level);
            out.push(b'}');
        }
        Value::Arr(a) if a.is_empty() => out.extend_from_slice(b"[ ]"),
        Value::Arr(a) => {
            out.push(b'[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                indent(out, level + 1);
                write_pretty_child(out, v, level + 1);
            }
            indent(out, level);
            out.push(b']');
        }
        Value::Str(s) if super::raw::as_raw(s).is_some() => write_pretty_child(out, v, level),
        scalar => write_scalar(out, scalar),
    }
}

pub(crate) fn write_scalar(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Num(n) => write_number(out, n),
        Value::Str(s) => write_str(out, s),
        Value::Obj(_) | Value::Arr(_) => write_compact(out, v),
    }
}

pub(crate) fn write_number(out: &mut Vec<u8>, n: &Number) {
    match n {
        Number::Int(i) => out.extend_from_slice(itoa::Buffer::new().format(*i).as_bytes()),
        Number::UInt(u) => out.extend_from_slice(itoa::Buffer::new().format(*u).as_bytes()),
        Number::Big(s) => out.extend_from_slice(s.as_bytes()),
        // Jackson quotes non-finite doubles (`QUOTE_NON_NUMERIC_NUMBERS`), so the output stays
        // valid JSON that any parser (including ours) reads back as a string.
        Number::Float(f) if !f.is_finite() => {
            out.push(b'"');
            out.extend_from_slice(java_double(*f).as_bytes());
            out.push(b'"');
        }
        Number::Float(f) => {
            if !write_plain_double(out, *f) {
                out.extend_from_slice(java_double(*f).as_bytes());
            }
        }
    }
}

/// [`java_double`] without allocating, for doubles Java prints in plain notation
/// (`10^-3 <= |d| < 10^7`, the bulk of real values such as coordinates and prices): the
/// shortest round-trip digits (Rust's `Display`, the digits `java_double` uses too) and a
/// `.0` when there is no fraction. Returns false (nothing written) for other values. Java's
/// two-digit minimum only matters for subnormals, far outside this range.
#[inline]
fn write_plain_double(out: &mut Vec<u8>, d: f64) -> bool {
    let a = d.abs();
    if !(1e-3..1e7).contains(&a) {
        return false;
    }
    use std::io::Write;
    let mut buf = [0u8; 48];
    let mut cur = &mut buf[..];
    if write!(cur, "{d}").is_err() {
        return false;
    }
    let n = 48 - cur.len();
    let s = &buf[..n];
    out.extend_from_slice(s);
    if !s.contains(&b'.') {
        out.extend_from_slice(b".0");
    }
    true
}

/// Replaces non-finite doubles with the strings Jackson writes for them, so a value matches what
/// Elasticsearch reads back from the bytes it serialized (an ingest document's `NaN` is indexed
/// as the string `"NaN"`).
pub fn quote_non_finite(v: &mut Value) {
    match v {
        Value::Num(Number::Float(f)) if !f.is_finite() => *v = Value::Str(java_double(*f)),
        Value::Obj(m) => m.values_mut().for_each(quote_non_finite),
        Value::Arr(a) => a.iter_mut().for_each(quote_non_finite),
        _ => {}
    }
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

pub fn write_str(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            0x08 => b"\\b",
            0x09 => b"\\t",
            0x0a => b"\\n",
            0x0c => b"\\f",
            0x0d => b"\\r",
            0..=0x1f => {
                out.extend_from_slice(&bytes[start..i]);
                out.extend_from_slice(&[b'\\', b'u', b'0', b'0', HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]]);
                start = i + 1;
                continue;
            }
            _ => continue,
        };
        out.extend_from_slice(&bytes[start..i]);
        out.extend_from_slice(esc);
        start = i + 1;
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

/// Java's `Double.toString(d)`.
pub fn java_double(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if d == 0.0 {
        return if d.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // Rust's `{:e}` prints the shortest digits that round-trip, like Java 19+. Java renders
    // at least two significant digits and picks the closest two-digit decimal when the shortest
    // has one digit (`4.9E-324`, not `5.0E-324`).
    let sci = format!("{:e}", d.abs());
    let sci = if sci.split('e').next().is_some_and(|m| m.len() == 1) { two_digits(format!("{:.1e}", d.abs()), sci) } else { sci };
    java_format(&sci, d < 0.0)
}

/// Java's `Float.toString(f)`.
pub fn java_float(f: f32) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sci = format!("{:e}", f.abs());
    let sci = if sci.split('e').next().is_some_and(|m| m.len() == 1) { two_digits(format!("{:.1e}", f.abs()), sci) } else { sci };
    let sci = even_on_tie(sci, f.abs() as f64, |s| s.parse::<f32>().ok() == Some(f.abs()));
    java_format(&sci, f < 0.0)
}

/// The double that [`java_float`]'s text reads as, without allocating in the common cases
/// (every hit's `_score`): integral floats are exact, and otherwise the shortest digits are
/// formatted on the stack. The cases where `java_float` may adjust Rust's digits (a one-digit
/// mantissa whose two-digit form differs, or an odd last digit whose neighbour also reads back
/// as the float, a possible tie) go through `java_float` itself.
pub fn java_float_value(f: f32) -> f64 {
    if !f.is_finite() {
        return java_float(f).parse::<f64>().unwrap_or(f64::from(f));
    }
    if f == f.trunc() && f.abs() < 16_777_216.0 {
        return f64::from(f);
    }
    float_text_fast(f).unwrap_or_else(|| java_float(f).parse::<f64>().unwrap_or(f64::from(f)))
}

fn float_text_fast(f: f32) -> Option<f64> {
    use std::io::Write;
    let mut buf = [0u8; 32];
    let n = {
        let mut cur = &mut buf[..];
        write!(cur, "{:e}", f.abs()).ok()?;
        32 - cur.len()
    };
    let e = buf[..n].iter().position(|&b| b == b'e')?;
    let digits = buf[..e].iter().filter(|b| b.is_ascii_digit()).count();
    if digits == 1 {
        let mut two = [0u8; 32];
        let m = {
            let mut cur = &mut two[..];
            write!(cur, "{:.1e}", f.abs()).ok()?;
            32 - cur.len()
        };
        if two[..m].iter().position(|&b| b == b'e').is_none_or(|i| two[i - 1] != b'0') {
            return None;
        }
    }
    let last = buf[e - 1];
    if (last - b'0') % 2 == 1 {
        let mut alt = buf;
        alt[e - 1] -= 1;
        // Both read back as the float: Java differs from Rust only on an exact tie.
        if std::str::from_utf8(&alt[..n]).ok()?.parse::<f32>().ok() == Some(f.abs()) && is_decimal_tie(f.abs(), &buf[..e], std::str::from_utf8(&buf[e + 1..n]).ok()?.parse().ok()?) != Some(false) {
            return None;
        }
    }
    let v = std::str::from_utf8(&buf[..n]).ok()?.parse::<f64>().ok()?;
    Some(if f < 0.0 { -v } else { v })
}

/// Whether finite positive `x` lies exactly halfway between the decimal `mant`e`exp` (digits
/// with an optional point, as `{:e}` writes them) and a neighbour in the last digit (the only
/// case where [`even_on_tie`] changes the digits), in exact integer arithmetic: `x = m * 2^e2`
/// and the midpoints are `(2D -/+ 1) * 10^q / 2` for the digits `D` and
/// `q = exp - (digits - 1)`. `None` when the numbers overflow `u128` (the caller then takes the
/// exact formatting path).
fn is_decimal_tie(x: f32, mant: &[u8], exp: i32) -> Option<bool> {
    let mut d: u128 = 0;
    let mut digits = 0i32;
    for &b in mant.iter().filter(|b| b.is_ascii_digit()) {
        d = d * 10 + u128::from(b - b'0');
        digits += 1;
    }
    let bits = x.to_bits();
    let (frac, be) = (u128::from(bits & 0x7f_ffff), (bits >> 23) & 0xff);
    let (m, e2) = if be == 0 { (frac, -149) } else { (frac | 1 << 23, be as i32 - 150) };
    let q = exp - (digits - 1);
    // 2 * m * 2^e2 == (2D - 1) * 2^q * 5^q, both sides moved to non-negative powers.
    let five = |k: i32| 5u128.checked_pow(k as u32);
    let shl = |v: u128, k: i32| if k >= 128 || (k > 0 && v.leading_zeros() < k as u32) { None } else { Some(v << k) };
    let twos = e2 + 1 - q;
    let mut tie = false;
    for odd in [d.checked_mul(2)?.checked_sub(1)?, d.checked_mul(2)?.checked_add(1)?] {
        let (mut lhs, mut rhs) = (m, odd);
        if q >= 0 {
            rhs = rhs.checked_mul(five(q)?)?;
        } else {
            lhs = lhs.checked_mul(five(-q)?)?;
        }
        if twos >= 0 {
            lhs = shl(lhs, twos)?;
        } else {
            rhs = shl(rhs, -twos)?;
        }
        tie |= lhs == rhs;
    }
    Some(tie)
}

/// Java (19+) picks, of two shortest decimals exactly as close to the value, the one with an
/// even last digit (`Float.toString(6.05078125f)` is `6.0507812`); Rust's shortest form may
/// end in the odd one. `exact` is the value (exactly, as a double); `round_trips` tells whether
/// a candidate reads back as it.
fn even_on_tie(sci: String, exact: f64, round_trips: impl Fn(&str) -> bool) -> String {
    let Some((mant, exp)) = sci.split_once('e') else { return sci };
    let last = mant.as_bytes().last().copied().unwrap_or(b'0');
    if (last - b'0').is_multiple_of(2) {
        return sci;
    }
    let n = mant.bytes().filter(u8::is_ascii_digit).count();
    // The exact expansion, well past the shortest digits: a tie reads `…d5000…` there.
    let full = format!("{:.*e}", n + 40, exact);
    let (fm, fe) = full.split_once('e').unwrap_or((&full, "0"));
    if fe != exp {
        return sci;
    }
    let fd: Vec<u8> = fm.bytes().filter(u8::is_ascii_digit).collect();
    if fd.len() <= n || fd[n] != b'5' || fd[n + 1..].iter().any(|&d| d != b'0') {
        return sci;
    }
    let mut alt = mant.as_bytes().to_vec();
    *alt.last_mut().unwrap() -= 1;
    let alt = format!("{}e{exp}", String::from_utf8(alt).unwrap_or_default());
    if round_trips(&alt) { alt } else { sci }
}

/// Keeps the one-digit shortest form unless the closest two-digit decimal differs from it.
fn two_digits(two: String, one: String) -> String {
    if two.split('e').next().is_some_and(|m| m.ends_with('0')) { one } else { two }
}

fn java_format(sci: &str, negative: bool) -> String {
    let (mant, exp) = sci.split_once('e').unwrap_or((sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let mut out = String::with_capacity(digits.len() + 8);
    if negative {
        out.push('-');
    }
    if (-3..7).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', int_len - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-exp - 1) as usize));
            out.push_str(&digits);
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
        out.push_str(&exp.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allocation-free float path reads exactly as `java_float`'s text, across random bit
    /// patterns, every integral float, scores and the tie/two-digit cases.
    #[test]
    fn java_float_value_matches_java_float() {
        let slow = |f: f32| java_float(f).parse::<f64>().unwrap();
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let check = |f: f32| {
            let got = java_float_value(f);
            let want = slow(f);
            assert!(got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()), "{f:e}: {got} vs {want}");
        };
        for _ in 0..2_000_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            check(f32::from_bits(x as u32));
        }
        for i in -70_000..70_000 {
            check(i as f32);
            check(i as f32 / 1024.0);
            check(i as f32 * 0.37);
        }
        for f in [0.0, -0.0, 0.5, 0.1, 1e10, 3.4028235e38, f32::MIN_POSITIVE, 1e-45, 16_777_216.0, 16_777_217.0, f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
            check(f);
        }
        check(f32::from_bits(0x40C1_A000));
        check(-f32::from_bits(0x40D5_2000));
    }

    /// The integer tie test agrees with the exact decimal expansion (`…d5000…` after the
    /// shortest digits) wherever it decides.
    #[test]
    fn decimal_tie_matches_the_exact_expansion() {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut ties = 0;
        let mut check = |f: f32| {
            let f = f.abs();
            if !f.is_finite() || f == 0.0 {
                return;
            }
            let sci = format!("{f:e}");
            let (mant, exp) = sci.split_once('e').unwrap();
            let n = mant.bytes().filter(u8::is_ascii_digit).count();
            let full = format!("{:.*e}", n + 40, f64::from(f));
            let (fm, fe) = full.split_once('e').unwrap();
            let fd: Vec<u8> = fm.bytes().filter(u8::is_ascii_digit).collect();
            let want = fe == exp && fd[n] == b'5' && fd[n + 1..].iter().all(|&d| d == b'0');
            if let Some(got) = is_decimal_tie(f, mant.as_bytes(), exp.parse().unwrap()) {
                assert_eq!(got, want, "{f:e}");
                ties += usize::from(got);
            }
        };
        for _ in 0..300_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            check(f32::from_bits(x as u32));
        }
        // Floats with few significant bits, where ties live (6.05078125 = 0x40C1A000).
        for m in 0..1u32 << 12 {
            for e in [110u32, 120, 127, 130, 135, 140, 150, 160] {
                check(f32::from_bits((e << 23) | (m << 11)));
            }
        }
        check(f32::from_bits(0x40C1_A000));
        assert!(ties > 0);
    }

    /// The plain-notation fast path writes exactly what `java_double` returns.
    #[test]
    fn plain_double_matches_java_double() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let check = |d: f64| {
            let mut out = Vec::new();
            if write_plain_double(&mut out, d) {
                assert_eq!(String::from_utf8(out).unwrap(), java_double(d), "{d:e}");
            } else {
                assert!(!(1e-3..1e7).contains(&d.abs()), "{d:e}");
            }
        };
        for d in [1e-3, 0.001, 0.0010000000000000002, 9_999_999.999999998, 1e7, 0.1, 0.3, 2.0, 100.0, 4.8951, 52.37, -13.4, 1234567.0, 0.000999] {
            check(d);
            check(-d);
        }
        for _ in 0..300_000 {
            let r = next();
            // uniform bits (mostly out of range), decimals with few digits, and values in range
            check(f64::from_bits(r));
            let digits = (r >> 60) as i32;
            check((r % 100_000_000) as f64 / 10f64.powi(digits));
            check(1e-3 + (r >> 11) as f64 / (1u64 << 53) as f64 * 1e7);
            check(f32::from_bits(r as u32) as f64);
        }
    }
    use crate::{arr, obj};

    /// Half floats Elasticsearch 9.5.5 rendered as sort values: of two equally close shortest
    /// decimals, Java prints the one ending in an even digit.
    #[test]
    fn floats_break_ties_to_even_like_java() {
        assert_eq!(java_float(f32::from_bits(0x40C1_A000)), "6.0507812");
        assert_eq!(java_float(-f32::from_bits(0x40D5_2000)), "-6.6601562");
        assert_eq!(java_float(121.1875), "121.1875");
        assert_eq!(java_float(0.1), "0.1");
        assert_eq!(java_float(1.0e10), "1.0E10");
    }

    #[test]
    fn doubles_like_java() {
        for (d, s) in [
            (1.0, "1.0"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (100.0, "100.0"),
            (1e7, "1.0E7"),
            (9999999.0, "9999999.0"),
            (1000000.5, "1000000.5"),
            (0.001, "0.001"),
            (0.0001, "1.0E-4"),
            (1e-7, "1.0E-7"),
            (1e300, "1.0E300"),
            (-2.5e-10, "-2.5E-10"),
            (123456789.0, "1.23456789E8"),
            (-0.0, "-0.0"),
            (f64::MAX, "1.7976931348623157E308"),
            (f64::MIN_POSITIVE, "2.2250738585072014E-308"),
            (5e-324, "4.9E-324"),
        ] {
            assert_eq!(java_double(d), s, "{d}");
        }
        assert_eq!(java_float(0.1f32), "0.1");
        assert_eq!(java_float(1e10f32), "1.0E10");
        assert_eq!(java_float(3.4028235e38f32), "3.4028235E38");
    }

    #[test]
    fn non_finite_doubles_are_quoted() {
        let v = obj! { "a" => Value::Num(Number::Float(f64::NAN)), "b" => arr![Value::Num(Number::Float(f64::INFINITY)), Value::Num(Number::Float(f64::NEG_INFINITY))] };
        let json = v.to_json();
        assert_eq!(json, r#"{"a":"NaN","b":["Infinity","-Infinity"]}"#);
        assert!(crate::json::parse(json.as_bytes()).is_ok());
        assert_eq!(v.to_pretty_json(), "{\n  \"a\" : \"NaN\",\n  \"b\" : [\n    \"Infinity\",\n    \"-Infinity\"\n  ]\n}");
        let mut q = v.clone();
        quote_non_finite(&mut q);
        assert_eq!(q, obj! { "a" => "NaN", "b" => arr!["Infinity", "-Infinity"] });
    }

    #[test]
    fn escapes() {
        let mut out = Vec::new();
        write_str(&mut out, "a\"b\\c\n\t\u{1}\u{7f}é/");
        assert_eq!(String::from_utf8(out).unwrap(), "\"a\\\"b\\\\c\\n\\t\\u0001\u{7f}é/\"");
    }

    #[test]
    fn pretty_like_jackson() {
        let v = obj! { "a" => arr![1, arr![2, 3], obj! { "x" => Value::Null }], "e" => obj! {}, "ea" => arr![] };
        assert_eq!(
            v.to_pretty_json(),
            "{\n  \"a\" : [\n    1,\n    [\n      2,\n      3\n    ],\n    {\n      \"x\" : null\n    }\n  ],\n  \"e\" : { },\n  \"ea\" : [ ]\n}"
        );
    }
}
