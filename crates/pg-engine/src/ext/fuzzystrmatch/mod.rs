//! fuzzystrmatch (`contrib/fuzzystrmatch`): soundex, difference, levenshtein, metaphone,
//! double metaphone and Daitch-Mokotoff soundex. The database's C locale makes `isalpha` /
//! `toupper` ASCII-only, so the byte-wise algorithms work on bytes as fuzzystrmatch.c does.

mod daitch_mokotoff;
mod dmetaphone;
mod metaphone;

use super::native::Native;
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::{FnCtx, text_arg};
use crate::types::Value;

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(Native::scalar(match sym {
        "levenshtein" => levenshtein,
        "levenshtein_with_costs" => levenshtein_with_costs,
        "levenshtein_less_equal" => levenshtein_less_equal,
        "levenshtein_less_equal_with_costs" => levenshtein_less_equal_with_costs,
        "metaphone" => metaphone::metaphone,
        "soundex" => soundex,
        "difference" => difference,
        "dmetaphone" => |a, _| Ok(Value::text(dmetaphone::double_metaphone(text_arg(&a[0]).as_bytes()).0)),
        "dmetaphone_alt" => |a, _| Ok(Value::text(dmetaphone::double_metaphone(text_arg(&a[0]).as_bytes()).1)),
        "daitch_mokotoff" => daitch_mokotoff::daitch_mokotoff,
        _ => return None,
    }))
}

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

/// `_soundex`: four characters (all NULs, so an empty string, without a letter).
fn soundex_of(s: &[u8]) -> Vec<u8> {
    const TABLE: &[u8; 26] = b"01230120022455012623010202";
    let code = |c: u8| {
        let u = c.to_ascii_uppercase();
        if u.is_ascii_uppercase() { TABLE[(u - b'A') as usize] } else { u }
    };
    let Some(start) = s.iter().position(|c| c.is_ascii_alphabetic()) else {
        return Vec::new();
    };
    let mut out = vec![s[start].to_ascii_uppercase()];
    let mut i = start + 1;
    while i < s.len() && out.len() < 4 {
        if s[i].is_ascii_alphabetic() && code(s[i]) != code(s[i - 1]) {
            let c = code(s[i]);
            if c != b'0' {
                out.push(c);
            }
        }
        i += 1;
    }
    out.resize(4, b'0');
    out
}

fn soundex(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::text(String::from_utf8_lossy(&soundex_of(text_arg(&a[0]).as_bytes())).into_owned()))
}

fn difference(a: &[Value], _: &FnCtx) -> Result<Value> {
    let mut x = soundex_of(text_arg(&a[0]).as_bytes());
    let mut y = soundex_of(text_arg(&a[1]).as_bytes());
    // (An empty code is four NULs: equal to another empty one.)
    x.resize(4, 0);
    y.resize(4, 0);
    Ok(Value::Int4(x.iter().zip(&y).filter(|(p, q)| p == q).count() as i32))
}

fn levenshtein(a: &[Value], _: &FnCtx) -> Result<Value> {
    lev(a, 1, 1, 1, None)
}

fn levenshtein_with_costs(a: &[Value], _: &FnCtx) -> Result<Value> {
    lev(a, int(&a[2]), int(&a[3]), int(&a[4]), None)
}

fn levenshtein_less_equal(a: &[Value], _: &FnCtx) -> Result<Value> {
    lev(a, 1, 1, 1, Some(int(&a[2])))
}

fn levenshtein_less_equal_with_costs(a: &[Value], _: &FnCtx) -> Result<Value> {
    lev(a, int(&a[2]), int(&a[3]), int(&a[4]), Some(int(&a[5])))
}

fn lev(a: &[Value], ins: i64, del: i64, sub: i64, max_d: Option<i64>) -> Result<Value> {
    let s: Vec<char> = text_arg(&a[0]).chars().collect();
    let t: Vec<char> = text_arg(&a[1]).chars().collect();
    varstr_levenshtein(&s, &t, ins, del, sub, max_d).map(|d| Value::Int4(d as i32))
}

/// `levenshtein_result`.
fn result(d: i64) -> Result<i64> {
    if d < i32::MIN as i64 || d > i32::MAX as i64 {
        return Err(Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "levenshtein distance out of range"));
    }
    Ok(d)
}

/// `varstr_levenshtein` / `varstr_levenshtein_less_equal` (levenshtein.c), over characters.
/// With `max_d`: once the distance is sure to exceed it, `max_d + 1`.
fn varstr_levenshtein(s: &[char], t: &[char], ins_c: i64, del_c: i64, sub_c: i64, max_d: Option<i64>) -> Result<i64> {
    const MAX_STRLEN: usize = 255;
    let (m, n) = (s.len(), t.len());
    if m == 0 {
        return result(n as i64 * ins_c);
    }
    if n == 0 {
        return result(m as i64 * del_c);
    }
    if m > MAX_STRLEN || n > MAX_STRLEN {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("levenshtein argument exceeds maximum length of {MAX_STRLEN} characters"),
        ));
    }
    let less_equal = max_d.is_some();
    let mut max_d = max_d.unwrap_or(-1);
    let mut sub_c = sub_c;
    let mut start_column = 0usize;
    let mut stop_column = m + 1;
    if less_equal && max_d >= 0 {
        let net_inserts = n as i64 - m as i64;
        let min_theo_d = if net_inserts < 0 { -net_inserts * del_c } else { net_inserts * ins_c };
        if min_theo_d > max_d {
            return result(max_d + 1);
        }
        if ins_c + del_c < sub_c {
            sub_c = ins_c + del_c;
        }
        let max_theo_d = min_theo_d + sub_c * m.min(n) as i64;
        if max_d >= max_theo_d {
            max_d = -1;
        } else if ins_c + del_c > 0 {
            let slack_d = max_d - min_theo_d;
            let best_column = if net_inserts < 0 { -net_inserts } else { 0 };
            let tmp = best_column + slack_d / (ins_c + del_c) + 1;
            stop_column = (tmp.max(0) as usize).min(m + 1);
        }
    }
    let (m, n) = (m + 1, n + 1);
    let mut prev = vec![0i64; m];
    let mut curr = vec![0i64; m];
    for (i, p) in prev.iter_mut().enumerate().take(stop_column).skip(start_column) {
        *p = i as i64 * del_c;
    }
    for j in 1..n {
        let y = t[j - 1];
        let mut i;
        if less_equal {
            if stop_column < m {
                prev[stop_column] = max_d + 1;
                stop_column += 1;
            }
            if start_column == 0 {
                curr[0] = j as i64 * ins_c;
                i = 1;
            } else {
                i = start_column;
            }
        } else {
            curr[0] = j as i64 * ins_c;
            i = 1;
        }
        let stop = if less_equal { stop_column } else { m };
        while i < stop {
            let ins = prev[i] + ins_c;
            let del = curr[i - 1] + del_c;
            let sub = prev[i - 1] + if s[i - 1] == y { 0 } else { sub_c };
            curr[i] = ins.min(del).min(sub);
            i += 1;
        }
        std::mem::swap(&mut prev, &mut curr);
        if less_equal && max_d >= 0 {
            let zp = j as i64 - (n as i64 - m as i64);
            while stop_column > 0 {
                let ii = stop_column - 1;
                let net = ii as i64 - zp;
                if prev[ii] + if net > 0 { net * ins_c } else { -net * del_c } <= max_d {
                    break;
                }
                stop_column -= 1;
            }
            while start_column < stop_column {
                let net = start_column as i64 - zp;
                if prev[start_column] + if net > 0 { net * ins_c } else { -net * del_c } <= max_d {
                    break;
                }
                prev[start_column] = max_d + 1;
                curr[start_column] = max_d + 1;
                start_column += 1;
            }
            if start_column >= stop_column {
                return result(max_d + 1);
            }
        }
    }
    result(prev[m - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(a: &str, b: &str, i: i64, de: i64, s: i64, max: Option<i64>) -> i64 {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        varstr_levenshtein(&a, &b, i, de, s, max).unwrap()
    }

    #[test]
    fn levenshtein_vectors() {
        assert_eq!(d("GUMBO", "GAMBOL", 1, 1, 1, None), 2);
        assert_eq!(d("GUMBO", "GAMBOL", 2, 1, 1, None), 3);
        assert_eq!(d("kitten", "sitting", 1, 1, 1, None), 3);
        assert_eq!(d("extensive", "exhaustive", 1, 1, 1, Some(2)), 3);
        assert_eq!(d("extensive", "exhaustive", 1, 1, 1, Some(4)), 4);
        assert_eq!(d("aaa", "aaaaa", 1073741824, 0, 1073741824, Some(10)), 11);
    }

    #[test]
    fn soundex_vectors() {
        assert_eq!(soundex_of(b"Robert"), b"R163");
        assert_eq!(soundex_of(b"Tymczak"), b"T522");
        assert_eq!(soundex_of(b"hello world!"), b"H464");
        assert_eq!(soundex_of(b"Anne"), b"A500");
    }
}
