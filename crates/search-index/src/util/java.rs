//! Java's `Float.toString` / `Double.toString`, for texts Elasticsearch builds from Java numbers
//! (explanation descriptions, query `toString`s).
//!
//! Since JDK 19 Java prints the shortest decimal that rounds back to the same value, which is
//! also what Rust's formatting produces; only the layout differs: Java keeps at least one digit
//! after the point (`1.0`) and switches to scientific notation (`1.0E7`, `1.0E-4`) outside
//! `[1e-3, 1e7)`. We take Rust's shortest digits and lay them out the Java way.

/// `Float.toString(v)`.
pub fn float_to_string(v: f32) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    layout(&format!("{v:e}"), v == 0.0 && v.is_sign_negative())
}

/// `Double.toString(v)`.
pub fn double_to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    layout(&format!("{v:e}"), v == 0.0 && v.is_sign_negative())
}

/// Lays out Rust's `{:e}` output (`-1.2345e-5`, `1e7`) as Java does.
fn layout(sci: &str, neg_zero: bool) -> String {
    let (mant, exp) = sci.split_once('e').unwrap_or((sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (neg, mant) = match mant.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mant),
    };
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let mut out = String::with_capacity(digits.len() + 8);
    if neg || neg_zero {
        out.push('-');
    }
    if digits.bytes().all(|b| b == b'0') {
        out.push_str("0.0");
        return out;
    }
    if (-3..7).contains(&exp) {
        if exp < 0 {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                for _ in digits.len()..int_len {
                    out.push('0');
                }
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
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

    #[test]
    fn java_layout() {
        assert_eq!(float_to_string(1.0), "1.0");
        assert_eq!(float_to_string(2.2), "2.2");
        assert_eq!(float_to_string(0.47000363), "0.47000363");
        assert_eq!(float_to_string(4.3333335), "4.3333335");
        assert_eq!(float_to_string(0.0), "0.0");
        assert_eq!(float_to_string(-0.0), "-0.0");
        assert_eq!(float_to_string(100.0), "100.0");
        assert_eq!(float_to_string(1e7), "1.0E7");
        assert_eq!(float_to_string(1.5e-4), "1.5E-4");
        assert_eq!(float_to_string(0.001), "0.001");
        assert_eq!(float_to_string(1234567.0), "1234567.0");
        assert_eq!(float_to_string(-3.5), "-3.5");
        assert_eq!(float_to_string(f32::MAX), "3.4028235E38");
        assert_eq!(float_to_string(f32::MIN_POSITIVE), "1.1754944E-38");
        assert_eq!(double_to_string(0.1), "0.1");
        assert_eq!(double_to_string(1e21), "1.0E21");
        assert_eq!(double_to_string(123456.789), "123456.789");
        assert_eq!(double_to_string(f64::NAN), "NaN");
        assert_eq!(float_to_string(f32::NEG_INFINITY), "-Infinity");
    }
}
