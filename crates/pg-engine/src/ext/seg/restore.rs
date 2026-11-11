//! seg.c's `restore`: a float printed with exactly n significant digits (as `%.ng`, but
//! keeping trailing zeros: 8.00), ported with its buffer arithmetic.

const FLT_DIG: i32 = 6;

/// C's `%.{prec}e` (musl: `inf`, `nan`).
pub(crate) fn c_e(v: f64, prec: usize) -> String {
    if v.is_nan() {
        return if v.is_sign_negative() { "-nan" } else { "nan" }.into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf" } else { "inf" }.into();
    }
    let s = format!("{v:.prec$e}");
    let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
    let e: i32 = e.parse().unwrap_or(0);
    format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
}

/// C's `%g` (6 significant digits).
pub(crate) fn c_g(v: f64) -> String {
    if v.is_nan() || v.is_infinite() {
        return c_e(v, 0);
    }
    let s = c_e(v, 5);
    let (m, e) = s.split_once('e').unwrap();
    let x: i32 = e.parse().unwrap_or(0);
    let trim = |t: &str| -> String {
        if t.contains('.') { t.trim_end_matches('0').trim_end_matches('.').to_string() } else { t.to_string() }
    };
    if !(-4..6).contains(&x) {
        format!("{}e{}{:02}", trim(m), if x < 0 { '-' } else { '+' }, x.abs())
    } else {
        let decimals = (5 - x).max(0) as usize;
        trim(&format!("{v:.decimals$}"))
    }
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// sprintf into `buf` at `at` (with its NUL).
fn put(buf: &mut [u8], at: usize, s: &str) {
    buf[at..at + s.len()].copy_from_slice(s.as_bytes());
    buf[at + s.len()] = 0;
}

pub(crate) fn restore(val: f32, n: i32) -> String {
    let mut buf = [b'0'; 25];
    buf[24] = 0;
    let n = if n <= 0 { FLT_DIG } else { n.min(FLT_DIG) } as usize;
    let sign = usize::from(val < 0.0);
    let result = c_e(val as f64, n - 1);
    let Some(epos) = result.find('e') else {
        return result;
    };
    let mut exp: i32 = result[epos + 1..].parse().unwrap_or(0);
    if exp == 0 {
        return result[..epos].to_string();
    }
    if exp.abs() > 4 {
        return result;
    }
    let r = result.as_bytes();
    // The mantissa's digits into buf from position 10, the decimal point skipped.
    let (mut i, mut dp) = (10usize, 0usize);
    let mut p = sign;
    while r[p] != b'e' {
        buf[i] = r[p];
        if r[p] == b'.' {
            dp = i;
            i -= 1;
        }
        p += 1;
        i += 1;
    }
    if dp == 0 {
        dp = i;
    }
    let out_from = |buf: &mut [u8; 25], at: usize| -> String {
        if sign == 1 {
            buf[at - 1] = b'-';
            cstr(&buf[at - 1..])
        } else {
            cstr(&buf[at..])
        }
    };
    if exp > 0 {
        if dp as i32 - 10 + exp >= n as i32 {
            // The decimal point is behind the last significant digit: an exponent.
            exp = dp as i32 - 10 + exp - n as i32;
            buf[10 + n] = 0;
            if n > 1 {
                let dp = 11;
                let mut k = 23;
                while k > dp {
                    buf[k] = buf[k - 1];
                    k -= 1;
                }
                buf[dp] = b'.';
                put(&mut buf, 11 + n, &format!("e{}", exp + n as i32 - 1));
            } else {
                put(&mut buf, 11, &format!("e{}", exp + n as i32 - 1));
            }
            out_from(&mut buf, 10)
        } else {
            dp = (dp as i32 + exp) as usize;
            let mut k = 23;
            while k > dp {
                buf[k] = buf[k - 1];
                k -= 1;
            }
            buf[11 + n] = 0;
            buf[dp] = b'.';
            out_from(&mut buf, 10)
        }
    } else {
        let dp = (dp as i32 + exp - 1) as usize;
        buf[10 + n] = 0;
        buf[dp] = b'.';
        if sign == 1 {
            buf[dp - 2] = b'-';
            cstr(&buf[dp - 2..])
        } else {
            cstr(&buf[dp - 1..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits() {
        assert_eq!(restore(8.0, 3), "8.00");
        assert_eq!(restore(-0.0001, 1), "-0.0001");
        assert_eq!(restore(12345.0, 2), "1.2e4");
        assert_eq!(restore(120.0, 3), "1.20e2");
        assert_eq!(restore(1.5e10, 2), "1.5e+10");
        assert_eq!(c_g(0.0001), "0.0001");
        assert_eq!(c_g(1e-5), "1e-05");
        assert_eq!(c_g(123456789.0), "1.23457e+08");
    }
}
