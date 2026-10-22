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
        scalar => write_scalar(out, scalar),
    }
}

/// Nested containers recurse through the stack guard (values nest up to 1000 levels).
#[inline]
fn write_child(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Obj(_) | Value::Arr(_) => crate::stack::maybe_grow(|| write_compact(out, v)),
        scalar => write_scalar(out, scalar),
    }
}

#[inline]
fn write_pretty_child(out: &mut Vec<u8>, v: &Value, level: usize) {
    match v {
        Value::Obj(_) | Value::Arr(_) => crate::stack::maybe_grow(|| write_pretty(out, v, level)),
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
        Number::Float(f) => out.extend_from_slice(java_double(*f).as_bytes()),
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
    java_format(&sci, f < 0.0)
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
    use crate::{arr, obj};

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
