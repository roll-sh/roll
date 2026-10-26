//! String functions, LIKE / SIMILAR TO / regular expressions.

use super::{FnCtx, Registry, Ret, int_arg, text, text_arg};
use crate::error::{Error, Result, code};
use crate::types::array::ArrayValue;
use crate::types::collation::case;
use crate::types::text::{bpchar_trim, format_value};
use crate::types::{DataType as T, Value};
use crate::{bail, err};
use crate::types::regex::{
    self as rx, REG_ADVANCED, REG_EXPANDED, REG_EXTENDED, REG_ICASE, REG_NEWLINE, REG_NLANCH,
    REG_NLSTOP, REG_NOSUB, REG_QUOTE, Strategy,
};
use std::sync::Arc;

fn s(v: &Value) -> &str {
    text_arg(v)
}

fn length(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(s(&a[0]).chars().count() as i32))
}
fn bp_length(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(bpchar_trim(s(&a[0])).chars().count() as i32))
}
/// The database's encoding (GetDatabaseEncoding): what a text's bytes are in.
fn db_enc() -> i32 {
    crate::types::collation::database_encoding()
}
fn octet_length(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(match &a[0] {
        Value::Bytea(b) => b.len() as i32,
        v => crate::types::mbconv::server::octet_len(db_enc(), s(v)) as i32,
    }))
}
fn bit_length(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Value::Int4(n) = octet_length(a, c)? else {
        unreachable!()
    };
    Ok(Value::Int4(n * 8))
}
fn bytea_length(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(match &a[0] {
        Value::Bytea(b) => b.len() as i32,
        _ => 0,
    }))
}
// Case mapping follows the call's collation, or without one the database default
// (`collation::default()`; the binder gives calls under a C-ctype default their ASCII
// variants, `c_ctype_variant`).
fn lower(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(text(match c.coll {
        Some(coll) => coll.lower(s(&a[0])),
        None => crate::types::collation::default().lower(s(&a[0])),
    }))
}
fn upper(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(text(match c.coll {
        Some(coll) => coll.upper(s(&a[0])),
        None => crate::types::collation::default().upper(s(&a[0])),
    }))
}
fn casefold(a: &[Value], c: &FnCtx) -> Result<Value> {
    require_utf8("case folding")?;
    Ok(text(match c.coll {
        Some(coll) => coll.casefold(s(&a[0])),
        None => crate::types::collation::default().casefold(s(&a[0])),
    }))
}
fn initcap(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(text(match c.coll {
        Some(coll) => coll.initcap(s(&a[0])),
        None => crate::types::collation::default().initcap(s(&a[0])),
    }))
}

// Under a collation whose ctype is C (C, POSIX, ucs_basic) case mapping, and so ILIKE, only
// knows ASCII letters (asc_tolower / asc_toupper / asc_initcap).
fn casefold_c(a: &[Value], c: &FnCtx) -> Result<Value> {
    require_utf8("case folding")?;
    lower_c(a, c)
}
fn lower_c(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text(s(&a[0]).to_ascii_lowercase()))
}
fn upper_c(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text(s(&a[0]).to_ascii_uppercase()))
}
fn initcap_c(a: &[Value], _: &FnCtx) -> Result<Value> {
    let mut out = String::new();
    let mut prev_alnum = false;
    for c in s(&a[0]).chars() {
        let c = if prev_alnum {
            c.to_ascii_lowercase()
        } else {
            c.to_ascii_uppercase()
        };
        out.push(c);
        prev_alnum = c.is_ascii_alphanumeric();
    }
    Ok(text(out))
}
/// ILIKE lowers both sides with lower() under the collation, then matches.
fn ilike_c(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(like_match(
        &s(&a[0]).to_ascii_lowercase(),
        &s(&a[1]).to_ascii_lowercase(),
        escape_char(a)?,
        false,
    )?))
}
fn not_ilike_c(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Value::Bool(b) = ilike_c(a, c)? else {
        unreachable!()
    };
    Ok(Value::Bool(!b))
}

/// `def` as it runs when its input collation's ctype is C, if that differs.
pub fn c_ctype_variant(def: &super::FuncDef) -> Option<super::FuncDef> {
    if !def.args.first().is_some_and(|t| t.is_string()) {
        return None;
    }
    let second_text = def.args.get(1).is_some_and(|t| t.is_string());
    let mut d = def.clone();
    match def.name {
        "lower" => d.imp = lower_c,
        "upper" => d.imp = upper_c,
        // casefold has nothing but the lowercase mapping in C.
        "casefold" => d.imp = casefold_c,
        "initcap" => d.imp = initcap_c,
        "~~*" | "ilike" => d.imp = ilike_c,
        "!~~*" | "not_ilike" => d.imp = not_ilike_c,
        "~" => d.imp = op_re::<true>,
        "~*" => d.imp = op_re_ci::<true>,
        "!~" => d.imp = op_not_re::<true>,
        "!~*" => d.imp = op_not_re_ci::<true>,
        "similar" => d.imp = similar::<true>,
        "not_similar" => d.imp = not_similar::<true>,
        "substring" if second_text && def.args.len() == 2 => d.imp = substring_regex::<true>,
        "substring" if second_text => d.imp = substring_similar::<true>,
        "regexp_replace" => d.imp = regexp_replace::<true>,
        "regexp_match" => d.imp = regexp_match::<true>,
        "regexp_like" => d.imp = regexp_like::<true>,
        "regexp_count" => d.imp = regexp_count::<true>,
        "regexp_instr" => d.imp = regexp_instr::<true>,
        "regexp_substr" => d.imp = regexp_substr::<true>,
        "regexp_split_to_array" => d.imp = regexp_split_to_array::<true>,
        "regexp_matches" => d.srf = Some(regexp_matches::<true>),
        "regexp_split_to_table" => d.srf = Some(regexp_split_to_table::<true>),
        _ => return None,
    }
    Some(d)
}

/// substring(s from start [for count]), 1-based, clamped like Postgres.
pub fn substr_chars(st: &str, start: i64, count: Option<i64>) -> Result<String> {
    if let Some(c) = count
        && c < 0
    {
        bail!(SUBSTRING_ERROR, "negative substring length not allowed");
    }
    let end = match count {
        Some(c) => start.saturating_add(c),
        None => i64::MAX,
    };
    let from = start.max(1);
    if end <= from {
        return Ok(String::new());
    }
    Ok(st
        .chars()
        .skip((from - 1) as usize)
        .take((end - from) as usize)
        .collect())
}
fn substr(a: &[Value], _: &FnCtx) -> Result<Value> {
    let count = a.get(2).map(int_arg);
    Ok(text(substr_chars(s(&a[0]), int_arg(&a[1]), count)?))
}
fn substr_bytea(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    let start = int_arg(&a[1]);
    let count = a.get(2).map(int_arg);
    if let Some(c) = count
        && c < 0
    {
        bail!(SUBSTRING_ERROR, "negative substring length not allowed");
    }
    let end = count.map(|c| start + c).unwrap_or(i64::MAX);
    let from = start.max(1);
    if end <= from {
        return Ok(Value::bytea(Vec::new()));
    }
    let out: Vec<u8> = b
        .iter()
        .skip((from - 1) as usize)
        .take((end - from) as usize)
        .copied()
        .collect();
    Ok(Value::bytea(out))
}
fn bytes_of(v: &Value) -> &[u8] {
    match v {
        Value::Bytea(b) => b,
        _ => &[],
    }
}
/// byteapos: where `needle` first starts in `hay` (1-based; 0: nowhere).
fn bytea_position(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (hay, needle) = (bytes_of(&a[0]), bytes_of(&a[1]));
    if needle.is_empty() {
        return Ok(Value::Int4(1));
    }
    Ok(Value::Int4(
        hay.windows(needle.len())
            .position(|w| w == needle)
            .map_or(0, |i| i as i32 + 1),
    ))
}
/// bytea_overlay: the bytes before `start`, the replacement, then those after `count` more.
fn bytea_overlay(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (b, repl) = (bytes_of(&a[0]), bytes_of(&a[1]));
    let start = int_arg(&a[2]);
    let count = a.get(3).map_or(repl.len() as i64, int_arg);
    let end = i32::try_from(start + count)
        .map_err(|_| err!(NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"))?
        as i64;
    if start < 1 {
        bail!(SUBSTRING_ERROR, "negative substring length not allowed");
    }
    let mut out: Vec<u8> = b.iter().take((start - 1) as usize).copied().collect();
    out.extend_from_slice(repl);
    out.extend(b.iter().skip((end.max(1) - 1) as usize));
    Ok(Value::bytea(out))
}
/// byteatrim / bytealtrim / byteartrim: the bytes in the set cut from the ends.
fn bytea_trim_ends(a: &[Value], lead: bool, trail: bool) -> Result<Value> {
    let (b, set) = (bytes_of(&a[0]), bytes_of(&a[1]));
    let mut from = 0;
    let mut to = b.len();
    if lead {
        while from < to && set.contains(&b[from]) {
            from += 1;
        }
    }
    if trail {
        while to > from && set.contains(&b[to - 1]) {
            to -= 1;
        }
    }
    Ok(Value::bytea(b[from..to].to_vec()))
}
fn strpos(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (h, n) = (s(&a[0]), s(&a[1]));
    if let Some(coll) = c.coll.filter(|k| !k.deterministic) {
        if n.is_empty() {
            return Ok(Value::Int4(1));
        }
        return Ok(Value::Int4(match coll.find(h, n, 0, false) {
            Some((i, _)) => h[..i].chars().count() as i32 + 1,
            None => 0,
        }));
    }
    Ok(Value::Int4(match h.find(n) {
        Some(i) => h[..i].chars().count() as i32 + 1,
        None => 0,
    }))
}
fn position(a: &[Value], c: &FnCtx) -> Result<Value> {
    // position(substring IN string) is called as position(string, substring).
    strpos(a, c)
}

fn trim_set(a: &[Value]) -> Vec<char> {
    a.get(1)
        .map(|v| s(v).chars().collect())
        .unwrap_or_else(|| vec![' '])
}
fn btrim(a: &[Value], _: &FnCtx) -> Result<Value> {
    let set = trim_set(a);
    Ok(text(s(&a[0]).trim_matches(|c| set.contains(&c))))
}
fn ltrim(a: &[Value], _: &FnCtx) -> Result<Value> {
    let set = trim_set(a);
    Ok(text(s(&a[0]).trim_start_matches(|c| set.contains(&c))))
}
fn rtrim(a: &[Value], _: &FnCtx) -> Result<Value> {
    let set = trim_set(a);
    Ok(text(s(&a[0]).trim_end_matches(|c| set.contains(&c))))
}
fn pad(a: &[Value], left: bool) -> Result<Value> {
    let st = s(&a[0]);
    let len = int_arg(&a[1]).max(0) as usize;
    let fill: Vec<char> = a
        .get(2)
        .map(|v| s(v).chars().collect())
        .unwrap_or_else(|| vec![' ']);
    let cur: Vec<char> = st.chars().collect();
    if cur.len() >= len {
        return Ok(text(cur[..len].iter().collect::<String>()));
    }
    if fill.is_empty() {
        return Ok(text(st));
    }
    let padding: String = fill.iter().cycle().take(len - cur.len()).collect();
    Ok(text(if left {
        format!("{padding}{st}")
    } else {
        format!("{st}{padding}")
    }))
}
fn lpad(a: &[Value], _: &FnCtx) -> Result<Value> {
    pad(a, true)
}
fn rpad(a: &[Value], _: &FnCtx) -> Result<Value> {
    pad(a, false)
}
fn replace(a: &[Value], c: &FnCtx) -> Result<Value> {
    let from = s(&a[1]);
    if from.is_empty() || s(&a[0]).is_empty() {
        return Ok(a[0].clone());
    }
    if let Some(coll) = c.coll.filter(|k| !k.deterministic) {
        return Ok(text(coll.split(s(&a[0]), from).join(s(&a[2]))));
    }
    Ok(text(s(&a[0]).replace(from, s(&a[2]))))
}
fn repeat(a: &[Value], _: &FnCtx) -> Result<Value> {
    let n = int_arg(&a[1]).max(0) as usize;
    let st = s(&a[0]);
    if st.len().saturating_mul(n) > (1 << 30) {
        bail!(PROGRAM_LIMIT_EXCEEDED, "requested length too large");
    }
    Ok(text(st.repeat(n)))
}
fn reverse(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text(s(&a[0]).chars().rev().collect::<String>()))
}
fn left(a: &[Value], _: &FnCtx) -> Result<Value> {
    let st: Vec<char> = s(&a[0]).chars().collect();
    let n = int_arg(&a[1]);
    let k = if n >= 0 {
        (n as usize).min(st.len())
    } else {
        st.len().saturating_sub((-n) as usize)
    };
    Ok(text(st[..k].iter().collect::<String>()))
}
fn right(a: &[Value], _: &FnCtx) -> Result<Value> {
    let st: Vec<char> = s(&a[0]).chars().collect();
    let n = int_arg(&a[1]);
    let k = if n >= 0 {
        (n as usize).min(st.len())
    } else {
        st.len().saturating_sub((-n) as usize)
    };
    Ok(text(st[st.len() - k..].iter().collect::<String>()))
}

fn any_text(v: &Value, ty: &T, c: &FnCtx) -> String {
    match v {
        Value::Text(t) => t.to_string(),
        v => format_value(v, ty, c.exec.fmt()),
    }
}

fn concat(a: &[Value], c: &FnCtx) -> Result<Value> {
    let mut out = String::new();
    for (i, v) in a.iter().enumerate() {
        if !v.is_null() {
            out.push_str(&any_text(v, &c.arg_type(i), c));
        }
    }
    Ok(text(out))
}
fn concat_ws(a: &[Value], c: &FnCtx) -> Result<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let sep = s(&a[0]).to_string();
    let parts: Vec<String> = a[1..]
        .iter()
        .enumerate()
        .filter(|(_, v)| !v.is_null())
        .map(|(i, v)| any_text(v, &c.arg_type(i + 1), c))
        .collect();
    Ok(text(parts.join(&sep)))
}
fn split_part(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (st, d, n) = (s(&a[0]), s(&a[1]), int_arg(&a[2]));
    if n == 0 {
        bail!(INVALID_PARAMETER_VALUE, "field position must not be zero");
    }
    let parts: Vec<&str> = if d.is_empty() {
        vec![st]
    } else if let Some(coll) = c.coll.filter(|k| !k.deterministic) {
        coll.split(st, d)
    } else {
        st.split(d).collect()
    };
    let idx = if n > 0 { n - 1 } else { parts.len() as i64 + n };
    Ok(text(if idx >= 0 {
        parts.get(idx as usize).copied().unwrap_or("")
    } else {
        ""
    }))
}
fn string_to_array(a: &[Value], c: &FnCtx) -> Result<Value> {
    let nd = c.coll.filter(|k| !k.deterministic);
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let st = s(&a[0]);
    let null_str = a.get(2).filter(|v| !v.is_null()).map(|v| s(v).to_string());
    let items: Vec<Value> = if st.is_empty() {
        Vec::new()
    } else if a[1].is_null() {
        st.chars().map(|c| text(c.to_string())).collect()
    } else if s(&a[1]).is_empty() {
        vec![text(st)]
    } else if let Some(coll) = nd {
        coll.split(st, s(&a[1])).into_iter().map(text).collect()
    } else {
        st.split(s(&a[1])).map(text).collect()
    };
    let items = items
        .into_iter()
        .map(|v| {
            let is_null = |n: &str| match (nd, v.as_str()) {
                (Some(coll), Some(x)) => coll.eq(n, x),
                (_, x) => Some(n) == x,
            };
            if null_str.as_deref().is_some_and(is_null) {
                Value::Null
            } else {
                v
            }
        })
        .collect();
    Ok(Value::Array(Arc::new(ArrayValue::from_vec(T::Text, items))))
}
/// string_to_table(string, delimiter [, null_string]): string_to_array's elements as rows.
fn string_to_table(a: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    let items = match string_to_array(a, c)? {
        Value::Array(arr) => arr.items.clone(),
        _ => Vec::new(),
    };
    Ok(Box::new(items.into_iter().map(|v| Ok(vec![v]))))
}
/// parse_ident(qualified_identifier [, strict]): the names of a dotted identifier.
fn parse_ident(a: &[Value], _: &FnCtx) -> Result<Value> {
    let src = s(&a[0]);
    let strict = a.get(1).and_then(|v| v.as_bool()).unwrap_or(true);
    let invalid = || {
        err!(
            INVALID_PARAMETER_VALUE,
            "string is not a valid identifier: \"{src}\""
        )
    };
    let space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b');
    let start = |c: char| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii();
    let cont = |c: char| start(c) || c.is_ascii_digit() || c == '$';
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() && space(chars[i]) {
        i += 1;
    }
    let mut out = Vec::new();
    let mut after_dot = false;
    loop {
        let mut found = false;
        if chars.get(i) == Some(&'"') {
            let mut name = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => {
                        return Err(invalid().detail("String has unclosed double quotes."));
                    }
                    Some('"') if chars.get(i + 1) == Some(&'"') => {
                        name.push('"');
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(c) => {
                        name.push(*c);
                        i += 1;
                    }
                }
            }
            if name.is_empty() {
                return Err(invalid().detail("Quoted identifier must not be empty."));
            }
            out.push(text(name));
            found = true;
        } else if chars.get(i).is_some_and(|c| start(*c)) {
            let b = i;
            while chars.get(i).is_some_and(|c| cont(*c)) {
                i += 1;
            }
            let name: String = chars[b..i].iter().map(|c| c.to_ascii_lowercase()).collect();
            out.push(text(name));
            found = true;
        }
        if !found {
            return Err(if chars.get(i) == Some(&'.') {
                invalid().detail("No valid identifier before \".\".")
            } else if after_dot {
                invalid().detail("No valid identifier after \".\".")
            } else {
                invalid()
            });
        }
        while i < chars.len() && space(chars[i]) {
            i += 1;
        }
        match chars.get(i) {
            Some('.') => {
                after_dot = true;
                i += 1;
                while i < chars.len() && space(chars[i]) {
                    i += 1;
                }
            }
            None => break,
            Some(_) => {
                if strict {
                    return Err(invalid());
                }
                break;
            }
        }
    }
    Ok(Value::Array(Arc::new(ArrayValue::from_vec(T::Text, out))))
}
fn crc32(a: &[Value], _: &FnCtx) -> Result<Value> {
    let b: &[u8] = match &a[0] {
        Value::Bytea(b) => b,
        v => s(v).as_bytes(),
    };
    Ok(Value::Int8(crc32fast::hash(b) as i64))
}
/// CRC-32C (Castagnoli), as pg_crc32c computes it.
fn crc32c(a: &[Value], _: &FnCtx) -> Result<Value> {
    let b: &[u8] = match &a[0] {
        Value::Bytea(b) => b,
        v => s(v).as_bytes(),
    };
    let mut crc: u32 = !0;
    for &x in b {
        crc ^= x as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    Ok(Value::Int8(!crc as i64))
}
fn ascii(a: &[Value], _: &FnCtx) -> Result<Value> {
    let c = s(&a[0]).chars().next().map(|c| c as i32).unwrap_or(0);
    // (In another encoding than UTF8 a single-byte character's code is its byte.)
    let enc = db_enc();
    if enc != crate::types::mbconv::UTF8 && c > 127 && crate::types::mbconv::max_length(enc) > 1 {
        bail!(PROGRAM_LIMIT_EXCEEDED, "requested character too large");
    }
    Ok(Value::Int4(c))
}
fn chr(a: &[Value], _: &FnCtx) -> Result<Value> {
    let n = int_arg(&a[0]);
    if n == 0 {
        bail!(PROGRAM_LIMIT_EXCEEDED, "null character not permitted");
    }
    let enc = db_enc();
    if enc != crate::types::mbconv::UTF8 && n > 127 {
        if n < 0 {
            bail!(INVALID_PARAMETER_VALUE, "character number must be positive");
        }
        if crate::types::mbconv::max_length(enc) > 1 || n > 255 {
            bail!(PROGRAM_LIMIT_EXCEEDED, "requested character too large for encoding: {n}");
        }
    }
    let c = u32::try_from(n)
        .ok()
        .and_then(char::from_u32)
        .ok_or_else(|| {
            err!(
                PROGRAM_LIMIT_EXCEEDED,
                "requested character too large for encoding: {n}"
            )
        })?;
    Ok(text(c.to_string()))
}
fn md5(a: &[Value], _: &FnCtx) -> Result<Value> {
    use md5::Digest;
    let server;
    let bytes: &[u8] = match &a[0] {
        Value::Bytea(b) => b,
        v => {
            server = crate::types::mbconv::server::encode(db_enc(), s(v))?;
            &server
        }
    };
    let h = md5::Md5::digest(bytes);
    Ok(text(
        h.iter().map(|b| format!("{b:02x}")).collect::<String>(),
    ))
}
fn sha256(a: &[Value], _: &FnCtx) -> Result<Value> {
    use sha2::Digest;
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    Ok(Value::bytea(sha2::Sha256::digest(b).to_vec()))
}
fn sha224(a: &[Value], _: &FnCtx) -> Result<Value> {
    use sha2::Digest;
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    Ok(Value::bytea(sha2::Sha224::digest(b).to_vec()))
}
fn sha384(a: &[Value], _: &FnCtx) -> Result<Value> {
    use sha2::Digest;
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    Ok(Value::bytea(sha2::Sha384::digest(b).to_vec()))
}
fn sha512(a: &[Value], _: &FnCtx) -> Result<Value> {
    use sha2::Digest;
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    Ok(Value::bytea(sha2::Sha512::digest(b).to_vec()))
}
fn to_hex(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text(match &a[0] {
        Value::Int4(i) => format!("{:x}", *i as u32),
        Value::Int8(i) => format!("{:x}", *i as u64),
        v => format!("{:x}", v.as_i64().unwrap_or(0)),
    }))
}
fn encode(a: &[Value], _: &FnCtx) -> Result<Value> {
    use base64::Engine;
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    Ok(text(match s(&a[1]).to_ascii_lowercase().as_str() {
        "hex" => b.iter().map(|x| format!("{x:02x}")).collect::<String>(),
        "base64" => {
            let e = base64::engine::general_purpose::STANDARD.encode(b);
            // Postgres wraps base64 output at 76 characters.
            e.as_bytes()
                .chunks(76)
                .map(|c| std::str::from_utf8(c).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
        }
        // esc_encode: only NUL and high-bit bytes as octal (and backslashes doubled).
        "escape" => {
            let mut out = String::with_capacity(b.len());
            for &x in b.iter() {
                if x == 0 || x >= 0x80 {
                    out.push_str(&format!("\\{:03o}", x));
                } else if x == b'\\' {
                    out.push_str("\\\\");
                } else {
                    out.push(x as char);
                }
            }
            out
        }
        other => bail!(
            INVALID_PARAMETER_VALUE,
            "unrecognized encoding: \"{other}\""
        ),
    }))
}
fn decode(a: &[Value], _: &FnCtx) -> Result<Value> {
    use base64::Engine;
    let st = s(&a[0]);
    Ok(Value::bytea(match s(&a[1]).to_ascii_lowercase().as_str() {
        "hex" => crate::types::text::parse_bytea(&format!("\\x{st}"))?,
        "base64" => {
            let clean: String = st.chars().filter(|c| !c.is_whitespace()).collect();
            base64::engine::general_purpose::STANDARD
                .decode(clean)
                .map_err(|_| err!(INVALID_PARAMETER_VALUE, "invalid base64 end sequence"))?
        }
        "escape" => crate::types::text::parse_bytea(st)?,
        other => bail!(
            INVALID_PARAMETER_VALUE,
            "unrecognized encoding: \"{other}\""
        ),
    }))
}
fn translate(a: &[Value], _: &FnCtx) -> Result<Value> {
    let from: Vec<char> = s(&a[1]).chars().collect();
    let to: Vec<char> = s(&a[2]).chars().collect();
    let out: String = s(&a[0])
        .chars()
        .filter_map(|c| match from.iter().position(|f| *f == c) {
            Some(i) => to.get(i).copied(),
            None => Some(c),
        })
        .collect();
    Ok(text(out))
}
fn overlay(a: &[Value], _: &FnCtx) -> Result<Value> {
    let st: Vec<char> = s(&a[0]).chars().collect();
    let repl = s(&a[1]);
    let start = int_arg(&a[2]);
    let count = a.get(3).map(int_arg).unwrap_or(repl.chars().count() as i64);
    if start < 1 {
        bail!(SUBSTRING_ERROR, "negative substring length not allowed");
    }
    let head: String = st.iter().take((start - 1) as usize).collect();
    let tail: String = st
        .iter()
        .skip((start - 1 + count).max(0) as usize)
        .collect();
    Ok(text(format!("{head}{repl}{tail}")))
}
fn starts_with(a: &[Value], c: &FnCtx) -> Result<Value> {
    if c.coll.is_some_and(|k| !k.deterministic) {
        return Err(crate::types::collation::nondeterministic_search_error());
    }
    Ok(Value::Bool(s(&a[0]).starts_with(s(&a[1]))))
}
fn quote_ident(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text(crate::catalog::quote_ident(s(&a[0]))))
}
pub fn quote_literal_str(v: &str) -> String {
    let mut out = String::new();
    if v.contains('\\') {
        out.push('E');
    }
    out.push('\'');
    for c in v.chars() {
        if c == '\'' {
            out.push('\'');
        }
        if c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}
fn quote_literal(a: &[Value], c: &FnCtx) -> Result<Value> {
    let v = any_text(&a[0], &c.arg_type(0), c);
    Ok(text(quote_literal_str(&v)))
}
fn quote_nullable(a: &[Value], c: &FnCtx) -> Result<Value> {
    if a[0].is_null() {
        return Ok(text("NULL"));
    }
    quote_literal(a, c)
}

/// format(fmt, args…): %s, %I, %L with optional position (`%2$s`) and width (`%-10s`, `%*s`).
fn format(a: &[Value], c: &FnCtx) -> Result<Value> {
    // text_format: %[argpos$][flags][width]type, with width a number, `*` (the next
    // argument) or `*n$`.
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let fmt: Vec<char> = s(&a[0]).chars().collect();
    let nargs = a.len();
    let mut out = String::new();
    let unterminated = || {
        err!(INVALID_PARAMETER_VALUE, "unterminated format() type specifier")
            .hint("For a single \"%\" use \"%%\".")
    };
    let arg0 = || {
        err!(
            INVALID_PARAMETER_VALUE,
            "format specifies argument 0, but arguments are numbered from 1"
        )
    };
    let too_few = || err!(INVALID_PARAMETER_VALUE, "too few arguments for format()");
    // text_format_parse_digits: Some(n) when digits start at `*i`.
    let digits = |i: &mut usize| -> Result<Option<i64>> {
        let start = *i;
        let mut n: i64 = 0;
        while *i < fmt.len() && fmt[*i].is_ascii_digit() {
            n = n * 10 + fmt[*i].to_digit(10).unwrap() as i64;
            if n > i32::MAX as i64 {
                bail!(PROGRAM_LIMIT_EXCEEDED, "number is out of range");
            }
            *i += 1;
            if *i >= fmt.len() {
                return Err(unterminated());
            }
        }
        Ok((*i > start).then_some(n))
    };
    let mut arg = 1usize;
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != '%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= fmt.len() {
            return Err(unterminated());
        }
        if fmt[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        let (mut argpos, mut widthpos, mut minus, mut width) = (-1i64, -1i64, false, 0i64);
        let mut done = false;
        if let Some(n) = digits(&mut i)? {
            if fmt[i] != '$' {
                width = n;
                done = true;
            } else {
                argpos = n;
                if n == 0 {
                    return Err(arg0());
                }
                i += 1;
                if i >= fmt.len() {
                    return Err(unterminated());
                }
            }
        }
        if !done {
            while fmt[i] == '-' {
                minus = true;
                i += 1;
                if i >= fmt.len() {
                    return Err(unterminated());
                }
            }
            if fmt[i] == '*' {
                i += 1;
                if i >= fmt.len() {
                    return Err(unterminated());
                }
                match digits(&mut i)? {
                    Some(n) => {
                        if fmt[i] != '$' {
                            bail!(
                                INVALID_PARAMETER_VALUE,
                                "width argument position must be ended by \"$\""
                            );
                        }
                        widthpos = n;
                        if n == 0 {
                            return Err(arg0());
                        }
                        i += 1;
                        if i >= fmt.len() {
                            return Err(unterminated());
                        }
                    }
                    None => widthpos = 0,
                }
            } else if let Some(n) = digits(&mut i)? {
                width = n;
            }
        }
        let kind = fmt[i];
        i += 1;
        if !matches!(kind, 's' | 'I' | 'L') {
            return Err(err!(
                INVALID_PARAMETER_VALUE,
                "unrecognized format() type specifier \"{kind}\""
            )
            .hint("For a single \"%\" use \"%%\"."));
        }
        if widthpos >= 0 {
            if widthpos > 0 {
                arg = widthpos as usize;
            }
            if arg >= nargs {
                return Err(too_few());
            }
            let v = &a[arg];
            width = match v {
                Value::Null => 0,
                Value::Int4(x) => *x as i64,
                Value::Int2(x) => *x as i64,
                v => {
                    let t = any_text(v, &c.arg_type(arg), c);
                    t.trim().parse::<i32>().map_err(|_| {
                        err!(
                            INVALID_TEXT_REPRESENTATION,
                            "invalid input syntax for type integer: \"{t}\""
                        )
                    })? as i64
                }
            };
            arg += 1;
        }
        if argpos > 0 {
            arg = argpos as usize;
        }
        if arg >= nargs {
            return Err(too_few());
        }
        let v = &a[arg];
        let ty = c.arg_type(arg);
        arg += 1;
        let piece = if v.is_null() {
            match kind {
                's' => String::new(),
                'L' => "NULL".into(),
                _ => bail!(
                    NULL_VALUE_NOT_ALLOWED,
                    "null values cannot be formatted as an SQL identifier"
                ),
            }
        } else {
            let t = any_text(v, &ty, c);
            match kind {
                'I' => crate::catalog::quote_ident(&t),
                'L' => quote_literal_str(&t),
                _ => t,
            }
        };
        // text_format_append_string
        if width == 0 {
            out.push_str(&piece);
            continue;
        }
        let left = if width < 0 {
            if width <= i32::MIN as i64 {
                bail!(PROGRAM_LIMIT_EXCEEDED, "number is out of range");
            }
            width = -width;
            true
        } else {
            minus
        };
        let len = piece.chars().count() as i64;
        let pad = " ".repeat((width - len).max(0) as usize);
        if left {
            out.push_str(&piece);
            out.push_str(&pad);
        } else {
            out.push_str(&pad);
            out.push_str(&piece);
        }
    }
    Ok(text(out))
}

// ── LIKE ──

/// A LIKE pattern compiled into tokens.
#[derive(Clone, Copy, PartialEq)]
enum LikeTok {
    Lit(char),
    One,
    Any,
}

thread_local! {
    /// The last pattern compiled (a pure function of the pattern and the escape): a LIKE
    /// over a table's rows compiles its constant pattern once.
    static LIKE_LAST: std::cell::RefCell<Option<(String, Option<char>, std::rc::Rc<[LikeTok]>)>> =
        const { std::cell::RefCell::new(None) };
}

fn like_tokens(pat: &str, escape: Option<char>) -> Result<std::rc::Rc<[LikeTok]>> {
    if let Some(t) = LIKE_LAST.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|(p, e, _)| p == pat && *e == escape)
            .map(|(_, _, t)| t.clone())
    }) {
        return Ok(t);
    }
    let mut toks = Vec::new();
    let mut it = pat.chars();
    while let Some(c) = it.next() {
        if Some(c) == escape {
            match it.next() {
                Some(n) => toks.push(LikeTok::Lit(n)),
                None => bail!(
                    INVALID_ESCAPE_SEQUENCE,
                    "LIKE pattern must not end with escape character"
                ),
            }
        } else if c == '%' {
            if !matches!(toks.last(), Some(LikeTok::Any)) {
                toks.push(LikeTok::Any);
            }
        } else if c == '_' {
            toks.push(LikeTok::One);
        } else {
            toks.push(LikeTok::Lit(c));
        }
    }
    let toks: std::rc::Rc<[LikeTok]> = toks.into();
    LIKE_LAST.with(|c| *c.borrow_mut() = Some((pat.to_string(), escape, toks.clone())));
    Ok(toks)
}

/// Iterative wildcard matching with backtracking on the last '%', over the string's
/// characters (`at(i)`, `n` of them).
fn like_tokens_match(toks: &[LikeTok], n: usize, at: impl Fn(usize) -> char) -> bool {
    let (mut si, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while si < n {
        if ti < toks.len() {
            match toks[ti] {
                LikeTok::Lit(c) if c == at(si) => {
                    si += 1;
                    ti += 1;
                    continue;
                }
                LikeTok::One => {
                    si += 1;
                    ti += 1;
                    continue;
                }
                LikeTok::Any => {
                    star = Some((ti, si));
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some((t, s0)) => {
                ti = t + 1;
                si = s0 + 1;
                star = Some((t, s0 + 1));
            }
            None => return false,
        }
    }
    while ti < toks.len() && matches!(toks[ti], LikeTok::Any) {
        ti += 1;
    }
    ti == toks.len()
}

/// Match `s` against a LIKE pattern with `escape` (None = no escape character).
pub fn like_match(st: &str, pat: &str, escape: Option<char>, ci: bool) -> Result<bool> {
    // ILIKE lowers both sides with lower() first.
    if ci {
        let d = crate::types::collation::default();
        return like_match(&d.lower(st), &d.lower(pat), escape, false);
    }
    let toks = like_tokens(pat, escape)?;
    // (An ASCII string's characters are its bytes.)
    if st.is_ascii() {
        let b = st.as_bytes();
        return Ok(like_tokens_match(&toks, b.len(), |i| b[i] as char));
    }
    let chars: Vec<char> = st.chars().collect();
    Ok(like_tokens_match(&toks, chars.len(), |i| chars[i]))
}

fn escape_char(a: &[Value]) -> Result<Option<char>> {
    match a.get(2) {
        None => Ok(Some('\\')),
        Some(v) => {
            let e = s(v);
            let mut it = e.chars();
            match (it.next(), it.next()) {
                (None, _) => Ok(None),
                (Some(c), None) => Ok(Some(c)),
                _ => Err(err!(INVALID_ESCAPE_SEQUENCE, "invalid escape string")
                    .hint("Escape string must be empty or one character.")),
            }
        }
    }
}

fn like(a: &[Value], c: &FnCtx) -> Result<Value> {
    if let Some(coll) = c.coll.filter(|k| !k.deterministic) {
        let t: Vec<char> = s(&a[0]).chars().collect();
        let p: Vec<char> = s(&a[1]).chars().collect();
        return Ok(Value::Bool(
            like_nondeterministic(&t, &p, escape_char(a)?, coll, c)? == Like::True,
        ));
    }
    Ok(Value::Bool(like_match(
        s(&a[0]),
        s(&a[1]),
        escape_char(a)?,
        false,
    )?))
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Like {
    True,
    False,
    Abort,
}

/// like_match.c's `MatchText` under a nondeterministic collation: the pattern's literal
/// runs match substrings the collation finds equal.
fn like_nondeterministic(
    t: &[char],
    p: &[char],
    esc: Option<char>,
    coll: &crate::types::collation::Collation,
    fc: &FnCtx,
) -> Result<Like> {
    let end_escape = || {
        err!(
            INVALID_ESCAPE_SEQUENCE,
            "LIKE pattern must not end with escape character"
        )
    };
    if p == ['%'] {
        return Ok(Like::True);
    }
    crate::stack::check()?;
    let text_of = |cs: &[char]| cs.iter().collect::<String>();
    let (mut ti, mut pi) = (0usize, 0usize);
    while ti < t.len() && pi < p.len() {
        if p[pi] == '%' {
            pi += 1;
            while pi < p.len() {
                if p[pi] == '%' {
                    pi += 1;
                } else if p[pi] == '_' {
                    if ti >= t.len() {
                        return Ok(Like::Abort);
                    }
                    ti += 1;
                    pi += 1;
                } else {
                    break;
                }
            }
            if pi >= p.len() {
                return Ok(Like::True);
            }
            if Some(p[pi]) == esc && pi + 1 >= p.len() {
                return Err(end_escape());
            }
            while ti < t.len() {
                let m = like_nondeterministic(&t[ti..], &p[pi..], esc, coll, fc)?;
                if m != Like::False {
                    return Ok(m);
                }
                ti += 1;
            }
            return Ok(Like::Abort);
        } else if p[pi] == '_' {
            ti += 1;
            pi += 1;
            continue;
        }
        // The literal run up to the next wildcard, unescaped.
        let mut p1 = pi;
        let mut sub = String::new();
        while p1 < p.len() {
            if Some(p[p1]) == esc {
                p1 += 1;
                if p1 >= p.len() {
                    return Err(end_escape());
                }
            } else if p[p1] == '_' || p[p1] == '%' {
                break;
            }
            sub.push(p[p1]);
            p1 += 1;
        }
        if p1 >= p.len() {
            return Ok(
                if coll.strcoll(&sub, &text_of(&t[ti..])) == std::cmp::Ordering::Equal {
                    Like::True
                } else {
                    Like::False
                },
            );
        }
        let mut t1 = ti;
        loop {
            fc.exec.check()?;
            if coll.strcoll(&sub, &text_of(&t[ti..t1])) == std::cmp::Ordering::Equal
                && like_nondeterministic(&t[t1..], &p[p1..], esc, coll, fc)? == Like::True
            {
                return Ok(Like::True);
            }
            if t1 >= t.len() {
                return Ok(Like::False);
            }
            t1 += 1;
        }
    }
    if ti < t.len() {
        return Ok(Like::False);
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    Ok(if pi >= p.len() { Like::True } else { Like::Abort })
}
fn not_like(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Value::Bool(b) = like(a, c)? else {
        unreachable!()
    };
    Ok(Value::Bool(!b))
}
fn ilike(a: &[Value], c: &FnCtx) -> Result<Value> {
    if let Some(coll) = c.coll {
        if !coll.deterministic {
            bail!(
                FEATURE_NOT_SUPPORTED,
                "nondeterministic collations are not supported for ILIKE"
            );
        }
        // Both sides go through lower() under the collation.
        return Ok(Value::Bool(like_match(
            &coll.lower(s(&a[0])),
            &coll.lower(s(&a[1])),
            escape_char(a)?,
            false,
        )?));
    }
    Ok(Value::Bool(like_match(
        s(&a[0]),
        s(&a[1]),
        escape_char(a)?,
        true,
    )?))
}
fn not_ilike(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Value::Bool(b) = ilike(a, c)? else {
        unreachable!()
    };
    Ok(Value::Bool(!b))
}
/// Bytes map one-to-one onto chars U+0000..U+00FF, so char-wise LIKE is byte-wise LIKE.
fn latin1(v: &Value) -> String {
    match v {
        Value::Bytea(b) => b.iter().map(|&x| x as char).collect(),
        v => s(v).to_string(),
    }
}
fn bytea_like(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(like_match(
        &latin1(&a[0]),
        &latin1(&a[1]),
        Some('\\'),
        false,
    )?))
}
fn bytea_not_like(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Value::Bool(b) = bytea_like(a, c)? else {
        unreachable!()
    };
    Ok(Value::Bool(!b))
}
fn bp_like(a: &[Value], c: &FnCtx) -> Result<Value> {
    like(a, c)
}

// ── regular expressions ──
//
// regexp.c's interface to the regex engine (crate::types::regex, a port of Postgres's): the
// operators, SIMILAR TO, substring() and the regexp_* functions. A collation whose ctype is C
// classifies characters by regc_pg_locale.c's C strategy, any other by the builtin C.UTF-8
// locale's.

/// The regex character classification under a collation whose ctype is C, the call's
/// collation, or the default one (`pg_set_regex_collation`).
fn re_strategy(c_ctype: bool, fc: &FnCtx) -> Result<Strategy> {
    if c_ctype {
        return Ok(Strategy::C);
    }
    match fc.coll {
        Some(coll) => coll.regex_strategy(),
        None => crate::types::collation::default().regex_strategy(),
    }
}

/// The text of characters `from..to` of a matched string.
fn chr_text(data: &[rx::Chr], from: isize, to: isize) -> String {
    data[from as usize..to as usize]
        .iter()
        .map(|&c| char::from_u32(c).unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// `RE_compile_and_execute` of the pattern operators (no submatches wanted).
fn re_match<const C: bool>(a: &[Value], cflags: i32, c: &FnCtx) -> Result<bool> {
    rx::compile_and_match(s(&a[1]), s(&a[0]), cflags, re_strategy(C, c)?)
}
fn op_re_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(re_match::<C>(a, REG_ADVANCED, c)?))
}
fn op_re_ci_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(re_match::<C>(a, REG_ADVANCED | REG_ICASE, c)?))
}
fn op_not_re_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(!re_match::<C>(a, REG_ADVANCED, c)?))
}
fn op_not_re_ci_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(!re_match::<C>(a, REG_ADVANCED | REG_ICASE, c)?))
}

/// Run a regex function with the statement's cancel and timeout checks (the regex engine's
/// INTERRUPT points).
fn rx_scope<T>(c: &FnCtx, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let i = rx::Interrupts {
        cancel: Some(c.exec.txn.borrow().cancel.clone()),
        deadline: c.exec.deadline,
    };
    rx::with_interrupts(i, f)
}

fn op_re<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || op_re_body::<C>(a, c))
}
fn op_re_ci<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || op_re_ci_body::<C>(a, c))
}
fn op_not_re<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || op_not_re_body::<C>(a, c))
}
fn op_not_re_ci<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || op_not_re_ci_body::<C>(a, c))
}
fn regexp_replace<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || regexp_replace_body::<C>(a, c))
}
fn regexp_match<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || regexp_match_body::<C>(a, c))
}
fn regexp_like<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || regexp_like_body::<C>(a, c))
}
fn regexp_count<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || regexp_count_body::<C>(a, c))
}
fn regexp_instr<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || regexp_instr_body::<C>(a, c))
}
fn regexp_substr<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || regexp_substr_body::<C>(a, c))
}
fn regexp_split_to_array<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || regexp_split_to_array_body::<C>(a, c))
}
fn regexp_matches<const C: bool>(a: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    rx_scope(c, || regexp_matches_body::<C>(a, c))
}
fn regexp_split_to_table<const C: bool>(a: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    rx_scope(c, || regexp_split_to_table_body::<C>(a, c))
}
fn similar<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || similar_body::<C>(a, c))
}
fn substring_similar<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || substring_similar_body::<C>(a, c))
}
fn substring_regex<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    rx_scope(c, || substring_regex_body::<C>(a, c))
}

/// `pg_re_flags`: compile flags, and whether to act on every match.
struct Flags {
    cflags: i32,
    glob: bool,
}

/// `parse_re_flags`.
fn parse_flags(f: Option<&Value>) -> Result<Flags> {
    let mut fl = Flags {
        cflags: REG_ADVANCED,
        glob: false,
    };
    if let Some(v) = f {
        for c in s(v).chars() {
            match c {
                'g' => fl.glob = true,
                'b' => fl.cflags &= !(REG_ADVANCED | REG_EXTENDED | REG_QUOTE),
                'c' => fl.cflags &= !REG_ICASE,
                'e' => {
                    fl.cflags |= REG_EXTENDED;
                    fl.cflags &= !(REG_ADVANCED | REG_QUOTE);
                }
                'i' => fl.cflags |= REG_ICASE,
                'm' | 'n' => fl.cflags |= REG_NEWLINE,
                'p' => {
                    fl.cflags |= REG_NLSTOP;
                    fl.cflags &= !REG_NLANCH;
                }
                'q' => {
                    fl.cflags |= REG_QUOTE;
                    fl.cflags &= !(REG_ADVANCED | REG_EXTENDED);
                }
                's' => fl.cflags &= !REG_NEWLINE,
                't' => fl.cflags &= !REG_EXPANDED,
                'w' => {
                    fl.cflags &= !REG_NLSTOP;
                    fl.cflags |= REG_NLANCH;
                }
                'x' => fl.cflags |= REG_EXPANDED,
                _ => bail!(
                    INVALID_PARAMETER_VALUE,
                    "invalid regular expression option: \"{c}\""
                ),
            }
        }
    }
    Ok(fl)
}

/// Flags for a function that finds every match itself and so refuses 'g'.
fn parse_flags_no_global(f: Option<&Value>, func: &str) -> Result<i32> {
    let fl = parse_flags(f)?;
    if fl.glob {
        bail!(
            INVALID_PARAMETER_VALUE,
            "{func} does not support the \"global\" option"
        );
    }
    Ok(fl.cflags)
}

/// A positive (or, `zero_ok`, non-negative) integer parameter.
fn re_param(a: &[Value], i: usize, name: &str, zero_ok: bool) -> Result<Option<i64>> {
    let Some(v) = a.get(i) else { return Ok(None) };
    let n = int_arg(v);
    if n < 0 || (n == 0 && !zero_ok) {
        bail!(
            INVALID_PARAMETER_VALUE,
            "invalid value for parameter \"{name}\": {n}"
        );
    }
    Ok(Some(n))
}

/// `regexp_matches_ctx`: where a pattern matched, as character offsets — per match, its
/// subexpressions (`npatterns` of them) or else the whole match.
struct ReMatches {
    data: Vec<rx::Chr>,
    npatterns: usize,
    locs: Vec<(isize, isize)>,
    nmatches: usize,
}

impl ReMatches {
    fn get(&self, m: usize, p: usize) -> (isize, isize) {
        self.locs[m * self.npatterns + p]
    }
    fn part(&self, (so, eo): (isize, isize)) -> Value {
        if so < 0 || eo < 0 {
            Value::Null
        } else {
            text(chr_text(&self.data, so, eo))
        }
    }
    /// `build_regexp_match_result`.
    fn match_array(&self, m: usize) -> Value {
        let items = (0..self.npatterns).map(|p| self.part(self.get(m, p))).collect();
        Value::Array(Arc::new(ArrayValue::from_vec(T::Text, items)))
    }
    /// The pieces of the string between the matches (`build_regexp_split_result`).
    fn split(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.nmatches + 1);
        let mut start = 0;
        for m in 0..=self.nmatches {
            let end = if m < self.nmatches {
                self.get(m, 0).0
            } else {
                self.data.len() as isize
            };
            out.push(chr_text(&self.data, start, end));
            if m < self.nmatches {
                start = self.get(m, 0).1;
            }
        }
        out
    }
}

/// `setup_regexp_matches`: the matches of `pat` in `st` from character `start_search` on,
/// every one when `glob`. With `ignore_degenerate`, empty matches at either end or right after
/// the previous match are skipped (for splitting).
#[allow(clippy::too_many_arguments)]
fn setup_matches(
    st: &str,
    pat: &str,
    cflags: i32,
    glob: bool,
    strategy: Strategy,
    start_search: usize,
    use_subpatterns: bool,
    ignore_degenerate: bool,
) -> Result<ReMatches> {
    let data = rx::chrs(st);
    let len = data.len() as isize;
    let cflags = if use_subpatterns { cflags } else { cflags | REG_NOSUB };
    let re = rx::compile(pat, cflags, strategy)?;
    let (use_sub, npatterns) = if use_subpatterns && re.nsub() > 0 {
        (true, re.nsub())
    } else {
        (false, 1)
    };
    let pmatch_len = if use_sub { npatterns + 1 } else { 1 };
    let mut locs = Vec::new();
    let mut nmatches = 0;
    let mut prev_match_end = 0;
    let mut start = start_search;
    while let Some(pm) = {
        rx::check_interrupts()?;
        re.exec(&data, start, pmatch_len)?
    } {
        let (so, eo) = pm[0];
        if !ignore_degenerate || (so < len && eo > prev_match_end) {
            if use_sub {
                locs.extend_from_slice(&pm[1..=npatterns]);
            } else {
                locs.push(pm[0]);
            }
            nmatches += 1;
        }
        prev_match_end = eo;
        if !glob {
            break;
        }
        start = prev_match_end as usize;
        if so == eo {
            start += 1;
        }
        if start as isize > len {
            break;
        }
    }
    Ok(ReMatches {
        data,
        npatterns,
        locs,
        nmatches,
    })
}

/// `appendStringInfoRegexpSubstr`: the replacement text, with `\1`..`\9` and `\&` standing
/// for submatches and `\\` for a backslash; any other backslash is kept.
fn append_replacement(out: &mut String, rep: &str, data: &[rx::Chr], pm: &[(isize, isize)]) {
    let mut it = rep.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let (so, eo) = match it.clone().next() {
            None => {
                out.push('\\');
                break;
            }
            Some(d @ '1'..='9') => {
                it.next();
                pm.get(d as usize - '0' as usize).copied().unwrap_or((-1, -1))
            }
            Some('&') => {
                it.next();
                pm[0]
            }
            Some('\\') => {
                it.next();
                out.push('\\');
                continue;
            }
            Some(_) => {
                out.push('\\');
                continue;
            }
        };
        if so >= 0 && eo >= 0 {
            out.push_str(&chr_text(data, so, eo));
        }
    }
}

/// `check_replace_text_has_escape`: 0 without backslashes to process, 2 with `\1`..`\9`.
fn replace_escape_status(rep: &str) -> u8 {
    let b = rep.as_bytes();
    let mut result = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 1;
            if i < b.len() {
                if b[i].is_ascii_digit() && b[i] != b'0' {
                    return 2;
                }
                result = 1;
            }
        }
        i += 1;
    }
    result
}

/// `replace_text_regexp`: replace the `n`th match from character `search_start` on (every
/// match when 0).
fn replace_re(
    src: &str,
    pat: &str,
    rep: &str,
    cflags: i32,
    strategy: Strategy,
    search_start: usize,
    n: i64,
) -> Result<String> {
    let data = rx::chrs(src);
    let len = data.len();
    let escape_status = replace_escape_status(rep);
    let (cflags, nmatch) = if escape_status < 2 {
        (cflags | REG_NOSUB, 1)
    } else {
        (cflags, 10)
    };
    let re = rx::compile(pat, cflags, strategy)?;
    let mut out = String::with_capacity(src.len());
    let mut data_pos = 0usize;
    let mut search_start = search_start;
    let mut nmatches = 0;
    while search_start <= len {
        rx::check_interrupts()?;
        let Some(pm) = re.exec(&data, search_start, nmatch)? else {
            break;
        };
        let (so, eo) = (pm[0].0 as usize, pm[0].1 as usize);
        nmatches += 1;
        if n > 0 && nmatches != n {
            search_start = eo;
            if so == eo {
                search_start += 1;
            }
            continue;
        }
        out.push_str(&chr_text(&data, data_pos as isize, so as isize));
        if escape_status > 0 {
            append_replacement(&mut out, rep, &data, &pm);
        } else {
            out.push_str(rep);
        }
        data_pos = eo;
        if n > 0 {
            break;
        }
        search_start = data_pos;
        if so == eo {
            search_start += 1;
        }
    }
    if data_pos < len {
        out.push_str(&chr_text(&data, data_pos as isize, len as isize));
    }
    Ok(out)
}

/// regexp_replace(source, pattern, replacement [, start [, N]] [, flags]).
fn regexp_replace_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    // The four-argument form takes flags when its last argument is text.
    let text_flags = a.len() == 4 && !matches!(a[3], Value::Int4(_));
    let (start, n, flags) = if text_flags {
        let f = s(&a[3]);
        if f.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(err!(
                INVALID_PARAMETER_VALUE,
                "invalid regular expression option: \"{}\"",
                &f[..1]
            )
            .hint("If you meant to use regexp_replace() with a start parameter, cast the fourth argument to integer explicitly."));
        }
        (1, None, Some(&a[3]))
    } else {
        (
            re_param(a, 3, "start", false)?.unwrap_or(1),
            re_param(a, 4, "n", true)?,
            a.get(5),
        )
    };
    let fl = parse_flags(flags)?;
    // Without an N, 'g' replaces every match and its absence only the first.
    let n = n.unwrap_or(if fl.glob { 0 } else { 1 });
    Ok(text(replace_re(
        s(&a[0]),
        s(&a[1]),
        s(&a[2]),
        fl.cflags,
        re_strategy(C, c)?,
        (start - 1) as usize,
        n,
    )?))
}

fn regexp_match_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    let fl = parse_flags(a.get(2))?;
    if fl.glob {
        return Err(err!(
            INVALID_PARAMETER_VALUE,
            "regexp_match() does not support the \"global\" option"
        )
        .hint("Use the regexp_matches function instead."));
    }
    let m = setup_matches(s(&a[0]), s(&a[1]), fl.cflags, false, re_strategy(C, c)?, 0, true, false)?;
    Ok(if m.nmatches == 0 {
        Value::Null
    } else {
        m.match_array(0)
    })
}

fn regexp_like_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cflags = parse_flags_no_global(a.get(2), "regexp_like()")?;
    Ok(Value::Bool(rx::compile_and_match(
        s(&a[1]),
        s(&a[0]),
        cflags,
        re_strategy(C, c)?,
    )?))
}

/// The matches regexp_count / regexp_instr / regexp_substr look through: from `start`
/// (argument 2, 1-based) on, with the flags in argument `flags_at`.
fn matches_from<const C: bool>(
    a: &[Value],
    c: &FnCtx,
    flags_at: usize,
    func: &str,
    use_subpatterns: bool,
) -> Result<ReMatches> {
    let start = re_param(a, 2, "start", false)?.unwrap_or(1);
    let cflags = parse_flags_no_global(a.get(flags_at), func)?;
    setup_matches(
        s(&a[0]),
        s(&a[1]),
        cflags,
        true,
        re_strategy(C, c)?,
        (start - 1) as usize,
        use_subpatterns,
        false,
    )
}

fn regexp_count_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    let m = matches_from::<C>(a, c, 3, "regexp_count()", false)?;
    Ok(Value::Int4(m.nmatches as i32))
}

/// The location of match `n` (1-based), or of its subexpression `subexpr`: None when there's
/// no such match or subexpression.
fn nth_part(m: &ReMatches, n: i64, subexpr: i64) -> Option<(isize, isize)> {
    if n as usize > m.nmatches || subexpr as usize > m.npatterns {
        return None;
    }
    let p = if subexpr > 0 { subexpr as usize - 1 } else { 0 };
    Some(m.get(n as usize - 1, p))
}

fn regexp_instr_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    re_param(a, 2, "start", false)?;
    let n = re_param(a, 3, "n", false)?.unwrap_or(1);
    let endoption = match a.get(4) {
        Some(v) => {
            let e = int_arg(v);
            if e != 0 && e != 1 {
                bail!(
                    INVALID_PARAMETER_VALUE,
                    "invalid value for parameter \"endoption\": {e}"
                );
            }
            e
        }
        None => 0,
    };
    let subexpr = re_param(a, 6, "subexpr", true)?.unwrap_or(0);
    let m = matches_from::<C>(a, c, 5, "regexp_instr()", subexpr > 0)?;
    Ok(Value::Int4(match nth_part(&m, n, subexpr) {
        Some((so, eo)) => {
            let at = if endoption == 1 { eo } else { so };
            if at >= 0 { at as i32 + 1 } else { 0 }
        }
        None => 0,
    }))
}

fn regexp_substr_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    re_param(a, 2, "start", false)?;
    let n = re_param(a, 3, "n", false)?.unwrap_or(1);
    let subexpr = re_param(a, 5, "subexpr", true)?.unwrap_or(0);
    let m = matches_from::<C>(a, c, 4, "regexp_substr()", subexpr > 0)?;
    Ok(match nth_part(&m, n, subexpr) {
        Some(loc) => m.part(loc),
        None => Value::Null,
    })
}

fn regexp_split_to_array_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cflags = parse_flags_no_global(a.get(2), "regexp_split_to_array()")?;
    let m = setup_matches(s(&a[0]), s(&a[1]), cflags, true, re_strategy(C, c)?, 0, false, true)?;
    let items: Vec<Value> = m.split().into_iter().map(text).collect();
    Ok(Value::Array(Arc::new(ArrayValue::from_vec(T::Text, items))))
}

/// regexp_matches(source, pattern [, flags]): one text[] row per match (all of them with 'g').
fn regexp_matches_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    let fl = parse_flags(a.get(2))?;
    let m = setup_matches(s(&a[0]), s(&a[1]), fl.cflags, fl.glob, re_strategy(C, c)?, 0, true, false)?;
    let rows: Vec<Result<Vec<Value>>> = (0..m.nmatches).map(|i| Ok(vec![m.match_array(i)])).collect();
    Ok(Box::new(rows.into_iter()))
}

fn regexp_split_to_table_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    let cflags = parse_flags_no_global(a.get(2), "regexp_split_to_table()")?;
    let m = setup_matches(s(&a[0]), s(&a[1]), cflags, true, re_strategy(C, c)?, 0, false, true)?;
    Ok(Box::new(m.split().into_iter().map(|p| Ok(vec![text(p)]))))
}

/// `similar_escape_internal`: a SIMILAR TO pattern as an ARE, with escape string `esc`
/// (None: the default backslash).
fn similar_escape(pat: &str, esc: Option<&str>) -> Result<String> {
    let e = match esc {
        None => Some('\\'),
        Some(e) => {
            let mut it = e.chars();
            match (it.next(), it.next()) {
                (None, _) => None,
                (Some(c), None) => Some(c),
                _ => {
                    return Err(err!(INVALID_ESCAPE_SEQUENCE, "invalid escape string")
                        .hint("Escape string must be empty or one character."));
                }
            }
        }
    };
    let wide_escape = e.is_some_and(|e| !e.is_ascii());
    let mut out = String::from("^(?:");
    let mut afterescape = false;
    let mut nquotes = 0;
    let mut bracket_depth = 0;
    let mut charclass_pos = 0;
    for c in pat.chars() {
        // Both multibyte: the slow path, which tracks no brackets.
        if wide_escape && !c.is_ascii() {
            if afterescape {
                out.push('\\');
                out.push(c);
                afterescape = false;
            } else if Some(c) == e {
                afterescape = true;
            } else {
                out.push(c);
            }
            continue;
        }
        if afterescape {
            if c == '"' && bracket_depth < 1 {
                match nquotes {
                    0 => out.push_str("){1,1}?("),
                    1 => out.push_str("){1,1}(?:"),
                    _ => bail!(
                        INVALID_USE_OF_ESCAPE_CHARACTER,
                        "SQL regular expression may not contain more than two escape-double-quote separators"
                    ),
                }
                nquotes += 1;
            } else {
                out.push('\\');
                out.push(c);
                charclass_pos = 3;
            }
            afterescape = false;
        } else if Some(c) == e {
            afterescape = true;
        } else if bracket_depth > 0 {
            if c == '\\' {
                out.push('\\');
            }
            out.push(c);
            if c == ']' && charclass_pos > 2 {
                bracket_depth -= 1;
            } else if c == '[' {
                bracket_depth += 1;
                charclass_pos = 3;
            } else if c == '^' {
                charclass_pos += 1;
            } else {
                charclass_pos = 3;
            }
        } else {
            match c {
                '[' => {
                    out.push('[');
                    bracket_depth = 1;
                    charclass_pos = 1;
                }
                '%' => out.push_str(".*"),
                '_' => out.push('.'),
                '(' => out.push_str("(?:"),
                '\\' | '.' | '^' | '$' => {
                    out.push('\\');
                    out.push(c);
                }
                c => out.push(c),
            }
        }
    }
    out.push_str(")$");
    Ok(out)
}

fn similar_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    let pat = similar_escape(s(&a[1]), a.get(2).map(s))?;
    Ok(Value::Bool(rx::compile_and_match(
        &pat,
        s(&a[0]),
        REG_ADVANCED,
        re_strategy(C, c)?,
    )?))
}
fn not_similar<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Value::Bool(b) = similar::<C>(a, c)? else {
        unreachable!()
    };
    Ok(Value::Bool(!b))
}
fn similar_to_escape(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text(similar_escape(s(&a[0]), a.get(1).map(s))?))
}

/// `textregexsubstr`: the first subexpression's match when the pattern has any (NULL when
/// it took no part), else the whole match.
fn re_substring<const C: bool>(st: &str, pat: &str, c: &FnCtx) -> Result<Value> {
    let re = rx::compile(pat, REG_ADVANCED, re_strategy(C, c)?)?;
    let data = rx::chrs(st);
    let Some(pm) = re.exec(&data, 0, 2)? else {
        return Ok(Value::Null);
    };
    let (so, eo) = if re.nsub() > 0 { pm[1] } else { pm[0] };
    if so < 0 || eo < 0 {
        return Ok(Value::Null);
    }
    Ok(text(chr_text(&data, so, eo)))
}
/// substring(s similar pat escape e): the part between the escape-double-quote markers
/// (a SQL function in Postgres, which its errors name).
fn substring_similar_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    similar_escape(s(&a[1]), Some(s(&a[2])))
        .and_then(|pat| re_substring::<C>(s(&a[0]), &pat, c))
        .map_err(|e| e.context("SQL function \"substring\" statement 1"))
}
fn substring_regex_body<const C: bool>(a: &[Value], c: &FnCtx) -> Result<Value> {
    re_substring::<C>(s(&a[0]), s(&a[1]), c)
}
/// `LIKE pattern ESCAPE e`: rewrite the pattern to use backslash as the escape character.
fn like_escape(a: &[Value], _: &FnCtx) -> Result<Value> {
    let pat = text_arg(&a[0]);
    let esc = text_arg(&a[1]);
    let mut chars = esc.chars();
    let e = chars.next();
    if chars.next().is_some() {
        return Err(err!(INVALID_ESCAPE_SEQUENCE, "invalid escape string")
            .hint("Escape string must be empty or one character."));
    }
    let mut out = String::with_capacity(pat.len());
    let mut it = pat.chars();
    while let Some(c) = it.next() {
        match e {
            None => {
                // No escape character: backslashes are literal.
                if c == '\\' {
                    out.push_str("\\\\");
                } else {
                    out.push(c);
                }
            }
            Some(e) if c == e => match it.next() {
                Some(n) => {
                    out.push('\\');
                    out.push(n);
                }
                None => bail!(
                    INVALID_ESCAPE_SEQUENCE,
                    "LIKE pattern must not end with escape character"
                ),
            },
            Some(_) if c == '\\' => out.push_str("\\\\"),
            Some(_) => out.push(c),
        }
    }
    Ok(text(out))
}

fn gen_random_uuid(_: &[Value], c: &FnCtx) -> Result<Value> {
    let mut b = [0u8; 16];
    c.exec.random_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(Value::Uuid(u128::from_be_bytes(b)))
}
/// Unix-epoch nanoseconds, strictly increasing within the process
/// (get_real_time_ns_ascending).
fn real_time_ns_ascending() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let mut prev = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(prev + 1);
        match LAST.compare_exchange_weak(prev, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(p) => prev = p,
        }
    }
}

/// generate_uuidv7: 48-bit Unix milliseconds, then 12 bits of sub-millisecond fraction.
fn generate_uuidv7(c: &FnCtx, unix_ts_ms: u64, sub_ms_ns: u32) -> Value {
    let mut b = [0u8; 16];
    c.exec.random_bytes(&mut b);
    b[..6].copy_from_slice(&unix_ts_ms.to_be_bytes()[2..]);
    let frac = ((sub_ms_ns as u64) << 12) / 1_000_000;
    b[6] = 0x70 | ((frac >> 8) as u8 & 0x0f);
    b[7] = frac as u8;
    b[8] = (b[8] & 0x3f) | 0x80;
    Value::Uuid(u128::from_be_bytes(b))
}

const UNIX_TO_PG_EPOCH_US: i64 = 946_684_800_000_000;

fn uuidv7(_: &[Value], c: &FnCtx) -> Result<Value> {
    let ns = real_time_ns_ascending();
    Ok(generate_uuidv7(
        c,
        (ns / 1_000_000) as u64,
        (ns % 1_000_000) as u32,
    ))
}

/// uuidv7(interval): the current time shifted by the interval.
fn uuidv7_interval(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Value::Interval(shift) = &a[0] else {
        return Ok(Value::Null);
    };
    if shift.is_pos_inf() || shift.is_neg_inf() {
        return Err(err!(
            DATETIME_FIELD_OVERFLOW,
            "interval out of range for UUID version 7"
        )
        .detail("UUID version 7 does not support infinite intervals."));
    }
    let ns = real_time_ns_ascending();
    let ts = ns / 1000 - UNIX_TO_PG_EPOCH_US;
    let out_of_range = || {
        err!(
            DATETIME_FIELD_OVERFLOW,
            "timestamp out of range for UUID version 7"
        )
        .detail("UUID version 7 supports timestamps from 1970-01-01 to approximately year 10889.")
    };
    let ts = crate::types::datetime::tstz_add_interval(ts, shift, &c.exec.fmt().tz)
        .map_err(|_| out_of_range())?;
    let us = ts
        .checked_add(UNIX_TO_PG_EPOCH_US)
        .filter(|us| *us >= 0 && us / 1000 <= 0xFFFF_FFFF_FFFF)
        .ok_or_else(out_of_range)?;
    Ok(generate_uuidv7(
        c,
        (us / 1000) as u64,
        ((us % 1000) * 1000 + ns % 1000) as u32,
    ))
}

/// uuid_extract_version: NULL unless the RFC 9562 variant.
fn uuid_extract_version(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Value::Uuid(u) = &a[0] else {
        return Ok(Value::Null);
    };
    let b = u.to_be_bytes();
    if b[8] & 0xc0 != 0x80 {
        return Ok(Value::Null);
    }
    Ok(Value::Int2((b[6] >> 4) as i16))
}

/// uuid_extract_timestamp: versions 1 and 7.
fn uuid_extract_timestamp(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Value::Uuid(u) = &a[0] else {
        return Ok(Value::Null);
    };
    let d = u.to_be_bytes().map(|x| x as u64);
    if d[8] & 0xc0 != 0x80 {
        return Ok(Value::Null);
    }
    match d[6] >> 4 {
        1 => {
            let tms = (d[0] << 24)
                + (d[1] << 16)
                + (d[2] << 8)
                + d[3]
                + (d[4] << 40)
                + (d[5] << 32)
                + ((d[6] & 0xf) << 56)
                + (d[7] << 48);
            // 100ns intervals since 1582-10-15 (Julian day 2299161).
            const GREGORIAN_TO_PG_US: i64 = (2_451_545 - 2_299_161) * 86_400 * 1_000_000;
            Ok(Value::Timestamptz((tms / 10) as i64 - GREGORIAN_TO_PG_US))
        }
        7 => {
            let tms = d[5] + (d[4] << 8) + (d[3] << 16) + (d[2] << 24) + (d[1] << 32) + (d[0] << 40);
            Ok(Value::Timestamptz((tms * 1000) as i64 - UNIX_TO_PG_EPOCH_US))
        }
        _ => Ok(Value::Null),
    }
}
fn unistr(a: &[Value], _: &FnCtx) -> Result<Value> {
    let chars: Vec<char> = s(&a[0]).chars().collect();
    let mut out = String::new();
    let invalid_pair = || err!(SYNTAX_ERROR, "invalid Unicode surrogate pair");
    let hex_n = |from: usize, n: usize| -> Option<u32> {
        let h: String = chars.get(from..from + n)?.iter().collect();
        if h.chars().all(|c| c.is_ascii_hexdigit()) {
            u32::from_str_radix(&h, 16).ok()
        } else {
            None
        }
    };
    let mut pair_first: u32 = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' {
            if pair_first != 0 {
                return Err(invalid_pair());
            }
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&'\\') {
            if pair_first != 0 {
                return Err(invalid_pair());
            }
            out.push('\\');
            i += 2;
            continue;
        }
        // \XXXX, \uXXXX, \+XXXXXX, \UXXXXXXXX
        let (cp, len) = if let Some(cp) = hex_n(i + 1, 4) {
            (cp, 5)
        } else if chars.get(i + 1) == Some(&'u') && let Some(cp) = hex_n(i + 2, 4) {
            (cp, 6)
        } else if chars.get(i + 1) == Some(&'+') && let Some(cp) = hex_n(i + 2, 6) {
            (cp, 8)
        } else if chars.get(i + 1) == Some(&'U') && let Some(cp) = hex_n(i + 2, 8) {
            (cp, 10)
        } else {
            return Err(err!(SYNTAX_ERROR, "invalid Unicode escape")
                .hint("Unicode escapes must be \\XXXX, \\+XXXXXX, \\uXXXX, or \\UXXXXXXXX."));
        };
        if cp == 0 || cp > 0x10ffff {
            return Err(err!(INVALID_PARAMETER_VALUE, "invalid Unicode code point: {:04X}", cp));
        }
        let second = (0xdc00..0xe000).contains(&cp);
        let cp = if pair_first != 0 {
            if !second {
                return Err(invalid_pair());
            }
            let c = 0x10000 + ((pair_first - 0xd800) << 10) + (cp - 0xdc00);
            pair_first = 0;
            c
        } else if second {
            return Err(invalid_pair());
        } else {
            cp
        };
        if (0xd800..0xdc00).contains(&cp) {
            pair_first = cp;
        } else {
            let c = char::from_u32(cp)
                .ok_or_else(|| err!(INVALID_PARAMETER_VALUE, "invalid Unicode code point: {:04X}", cp))?;
            // (pg_unicode_to_server: in the database's encoding.)
            out.push_str(&crate::types::mbconv::server::from_unicode(db_enc(), c.encode_utf8(&mut [0; 4]))?);
        }
        i += len;
    }
    if pair_first != 0 {
        return Err(invalid_pair());
    }
    Ok(text(out))
}
/// reverse(bytea).
fn reverse_bytea(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    Ok(Value::bytea(b.iter().rev().copied().collect::<Vec<u8>>()))
}
/// to_bin / to_oct: the two's complement digits of an int4 / int8.
fn to_radix(a: &[Value], radix: u32) -> Value {
    let v: u64 = match &a[0] {
        Value::Int4(i) => *i as u32 as u64,
        Value::Int8(i) => *i as u64,
        v => v.as_i64().unwrap_or(0) as u64,
    };
    text(match radix {
        2 => format!("{v:b}"),
        _ => format!("{v:o}"),
    })
}
fn to_bin(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(to_radix(a, 2))
}
fn to_oct(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(to_radix(a, 8))
}
fn bytea_index_error(n: i64, len: i64) -> Error {
    Error::new(
        code::ARRAY_SUBSCRIPT_ERROR,
        format!("index {n} out of valid range, 0..{}", len - 1),
    )
}
/// get_bit(bytea, int8): bit n counting from the low bit of the first byte.
fn bytea_get_bit(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    let n = int_arg(&a[1]);
    let bits = b.len() as i64 * 8;
    if n < 0 || n >= bits {
        return Err(bytea_index_error(n, bits));
    }
    Ok(Value::Int4(((b[(n / 8) as usize] >> (n % 8)) & 1) as i32))
}
/// set_bit(bytea, int8, int4).
fn bytea_set_bit(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    let n = int_arg(&a[1]);
    let bit = int_arg(&a[2]);
    let bits = b.len() as i64 * 8;
    if n < 0 || n >= bits {
        return Err(bytea_index_error(n, bits));
    }
    if bit != 0 && bit != 1 {
        bail!(INVALID_PARAMETER_VALUE, "new bit must be 0 or 1");
    }
    let mut v = b.to_vec();
    let (i, m) = ((n / 8) as usize, 1u8 << (n % 8));
    if bit == 1 {
        v[i] |= m;
    } else {
        v[i] &= !m;
    }
    Ok(Value::bytea(v))
}
/// set_byte(bytea, int4, int4).
fn bytea_set_byte(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    let n = int_arg(&a[1]);
    if n < 0 || n >= b.len() as i64 {
        return Err(bytea_index_error(n, b.len() as i64));
    }
    let mut v = b.to_vec();
    v[n as usize] = int_arg(&a[2]) as u8;
    Ok(Value::bytea(v))
}
/// bit_count(bytea).
fn bytea_bit_count(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    Ok(Value::Int8(b.iter().map(|x| x.count_ones() as i64).sum()))
}
/// like_escape(bytea, bytea): the pattern with `\` as its escape character.
fn bytea_like_escape(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (Value::Bytea(pat), Value::Bytea(esc)) = (&a[0], &a[1]) else {
        return Ok(Value::Null);
    };
    if esc.len() > 1 {
        return Err(err!(INVALID_ESCAPE_SEQUENCE, "invalid escape string")
            .hint("Escape string must be empty or one character."));
    }
    let e = esc.first().copied();
    let mut out = Vec::with_capacity(pat.len());
    let mut it = pat.iter().copied();
    while let Some(c) = it.next() {
        match e {
            Some(e) if c == e => match it.next() {
                Some(n) => {
                    out.push(b'\\');
                    out.push(n);
                }
                None => bail!(
                    INVALID_ESCAPE_SEQUENCE,
                    "LIKE pattern must not end with escape character"
                ),
            },
            _ if c == b'\\' => out.extend_from_slice(b"\\\\"),
            _ => out.push(c),
        }
    }
    Ok(Value::bytea(out))
}
fn get_byte(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Value::Bytea(b) = &a[0] else {
        return Ok(Value::Null);
    };
    let n = int_arg(&a[1]);
    if n < 0 || n as usize >= b.len() {
        return Err(Error::new(
            code::ARRAY_SUBSCRIPT_ERROR,
            format!("index {n} out of valid range, 0..{}", b.len() as i64 - 1),
        ));
    }
    Ok(Value::Int4(b[n as usize] as i32))
}
/// An encoding named by a `name` argument (`pg_char_to_encoding`), or the error
/// `pg_convert` raises for it (`what`: "source" / "destination").
fn encoding_arg(v: &Value, what: &str) -> Result<i32> {
    let name = s(v);
    crate::types::encnames::encoding_id(name)
        .ok_or_else(|| err!(INVALID_PARAMETER_VALUE, "invalid {what} encoding name \"{name}\""))
}
fn bytea_bytes(v: &Value) -> &[u8] {
    match v {
        Value::Bytea(b) => b,
        v => s(v).as_bytes(),
    }
}
/// convert_from(bytea, name): `pg_convert` into the database encoding.
fn convert_from(a: &[Value], _: &FnCtx) -> Result<Value> {
    use crate::types::mbconv;
    let from = encoding_arg(&a[1], "source")?;
    let enc = db_enc();
    let out = mbconv::pg_convert(bytea_bytes(&a[0]), from, enc)?;
    Ok(text(mbconv::server::decode(enc, &out)?))
}
/// convert_to(text, name): `pg_convert` from the database encoding.
fn convert_to(a: &[Value], _: &FnCtx) -> Result<Value> {
    use crate::types::mbconv;
    let to = encoding_arg(&a[1], "destination")?;
    let enc = db_enc();
    let src = mbconv::server::encode(enc, s(&a[0]))?;
    Ok(Value::bytea(mbconv::pg_convert(&src, enc, to)?))
}
/// convert(bytea, name, name).
fn convert(a: &[Value], _: &FnCtx) -> Result<Value> {
    let from = encoding_arg(&a[1], "source")?;
    let to = encoding_arg(&a[2], "destination")?;
    Ok(Value::bytea(crate::types::mbconv::pg_convert(
        bytea_bytes(&a[0]),
        from,
        to,
    )?))
}
/// length(bytea, name): the number of characters in that encoding (`length_in_encoding`).
fn length_in_encoding(a: &[Value], _: &FnCtx) -> Result<Value> {
    let name = s(&a[1]);
    let enc = crate::types::encnames::encoding_id(name)
        .ok_or_else(|| err!(INVALID_PARAMETER_VALUE, "invalid encoding name \"{name}\""))?;
    Ok(Value::Int4(crate::types::mbconv::verify_len(
        enc,
        bytea_bytes(&a[0]),
    )?))
}
fn pg_encoding_max_length(a: &[Value], _: &FnCtx) -> Result<Value> {
    let n = int_arg(&a[0]);
    Ok(match crate::types::encnames::encoding_name(n) {
        Some(_) => Value::Int4(crate::types::mbconv::max_length(n as i32)),
        None => Value::Null,
    })
}
/// A Unicode normalization form named as `normalize` takes it (any case).
fn normal_form(a: &[Value]) -> Result<&'static str> {
    let form = a.get(1).map_or("NFC".to_string(), |f| s(f).to_string());
    ["NFC", "NFD", "NFKC", "NFKD"]
        .into_iter()
        .find(|f| f.eq_ignore_ascii_case(&form))
        .ok_or_else(|| {
            err!(
                INVALID_PARAMETER_VALUE,
                "invalid normalization form: {form}"
            )
        })
}
fn normalized(text: &str, form: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    match form {
        "NFD" => text.nfd().collect(),
        "NFKC" => text.nfkc().collect(),
        "NFKD" => text.nfkd().collect(),
        _ => text.nfc().collect(),
    }
}
/// Unicode functions work in a UTF8 database only (`unicode_normalize_func`).
fn require_utf8(what: &str) -> Result<()> {
    if db_enc() != crate::types::mbconv::UTF8 {
        bail!(SYNTAX_ERROR, "Unicode {what} can only be performed if server encoding is UTF8");
    }
    Ok(())
}
fn normalize(a: &[Value], _: &FnCtx) -> Result<Value> {
    require_utf8("normalization")?;
    let form = normal_form(a)?;
    Ok(text(normalized(s(&a[0]), form)))
}
fn is_normalized(a: &[Value], _: &FnCtx) -> Result<Value> {
    require_utf8("normalization")?;
    let form = normal_form(a)?;
    Ok(Value::Bool(normalized(s(&a[0]), form) == s(&a[0])))
}
/// to_ascii: the text's bytes read in a single-byte encoding, accented letters as their
/// ASCII base (ascii.c); by default in the database's encoding.
fn to_ascii(a: &[Value], _: &FnCtx) -> Result<Value> {
    use crate::types::encnames::{encoding_id, encoding_name};
    let enc = match a.get(1) {
        None => db_enc(),
        Some(Value::Int4(n)) => {
            if encoding_name(*n as i64).is_none() {
                bail!(UNDEFINED_OBJECT, "{n} is not a valid encoding code");
            }
            *n
        }
        Some(v) => {
            let name = s(v);
            encoding_id(name)
                .ok_or_else(|| err!(UNDEFINED_OBJECT, "{name} is not a valid encoding name"))?
        }
    };
    let (table, range): (&[u8], u8) = match enc {
        8 => (b"  cL Y  \"Ca  -R     'u .,      ?AAAAAAACEEEEIIII NOOOOOxOUUUUYTBaaaaaaaceeeeiiii nooooo/ouuuuyty", 160),
        9 => (b" A L LS \"SSTZ-ZZ a,l'ls ,sstz\"zzRAAAALCCCEEEEIIDDNNOOOOxRUUUUYTBraaaalccceeeeiiddnnoooo/ruuuuyt.", 160),
        16 => (b"  cL YS sCa  -R     Zu .z   EeY?AAAAAAACEEEEIIII NOOOOOxOUUUUYTBaaaaaaaceeeeiiii nooooo/ouuuuyty", 160),
        29 => (b"  ' \"    %S<STZZ `'\"\".--  s>stzz   L A  \"CS  -RZ  ,l'u .,as L\"lzRAAAALCCCEEEEIIDDNNOOOOxRUUUUYTBraaaalccceeeeiiddnnoooo/ruuuuyt ", 128),
        _ => bail!(FEATURE_NOT_SUPPORTED, "encoding conversion from {} to ASCII not supported", encoding_name(enc as i64).unwrap_or("")),
    };
    let out: Vec<u8> = crate::types::mbconv::server::encode(db_enc(), s(&a[0]))?
        .iter()
        .map(|&b| match b {
            b if b < 128 => b,
            b if b < range => b' ',
            b => table.get((b - range) as usize).copied().unwrap_or(b' '),
        })
        .collect();
    Ok(text(String::from_utf8_lossy(&out).into_owned()))
}

pub fn register(r: &mut Registry) {
    use T::*;
    for name in ["length", "char_length", "character_length"] {
        r.func(name, vec![Text], Int4, length);
        r.func(name, vec![Bpchar(-1)], Int4, bp_length);
    }
    r.func("length", vec![Bytea], Int4, bytea_length);
    r.func("textlen", vec![Text], Int4, length);
    r.func("octet_length", vec![Text], Int4, octet_length);
    r.func("octet_length", vec![Bytea], Int4, octet_length);
    r.func("octet_length", vec![Bpchar(-1)], Int4, octet_length);
    r.func("bit_length", vec![Text], Int4, bit_length);
    r.func("bit_length", vec![Bytea], Int4, bit_length);
    r.func("lower", vec![Text], Text, lower);
    r.func("upper", vec![Text], Text, upper);
    r.func("initcap", vec![Text], Text, initcap);
    r.func("casefold", vec![Text], Text, casefold);
    for name in ["substr", "substring"] {
        r.func(name, vec![Text, Int4], Text, substr);
        r.func(name, vec![Text, Int4, Int4], Text, substr);
        r.func(name, vec![Bytea, Int4], Bytea, substr_bytea);
        r.func(name, vec![Bytea, Int4, Int4], Bytea, substr_bytea);
    }
    r.func("substring", vec![Text, Text], Text, substring_regex::<false>);
    r.func("substring", vec![Text, Text, Text], Text, substring_similar::<false>);
    r.func("strpos", vec![Text, Text], Int4, strpos);
    // The Unicode versions of Postgres's own tables and of the reference server's ICU.
    r.func("unicode_version", vec![], Text, |_, _| Ok(text("16.0")));
    r.func("icu_unicode_version", vec![], Text, |_, _| Ok(text("17.0")));
    r.func("unicode_assigned", vec![Text], Bool, |a, _| {
        if db_enc() != crate::types::mbconv::UTF8 {
            bail!(INTERNAL_ERROR, "Unicode categorization can only be performed if server encoding is UTF8");
        }
        Ok(Value::Bool(s(&a[0]).chars().all(|c| case::category(c as u32) != 0)))
    });
    r.func("position", vec![Text, Text], Int4, position);
    r.func("btrim", vec![Text], Text, btrim);
    r.func("btrim", vec![Text, Text], Text, btrim);
    r.func("ltrim", vec![Text], Text, ltrim);
    r.func("ltrim", vec![Text, Text], Text, ltrim);
    r.func("rtrim", vec![Text], Text, rtrim);
    r.func("rtrim", vec![Text, Text], Text, rtrim);
    r.func("lpad", vec![Text, Int4], Text, lpad);
    r.func("lpad", vec![Text, Int4, Text], Text, lpad);
    r.func("rpad", vec![Text, Int4], Text, rpad);
    r.func("rpad", vec![Text, Int4, Text], Text, rpad);
    r.func("replace", vec![Text, Text, Text], Text, replace);
    r.func("repeat", vec![Text, Int4], Text, repeat);
    r.func("reverse", vec![Text], Text, reverse);
    r.func("left", vec![Text, Int4], Text, left);
    r.func("right", vec![Text, Int4], Text, right);
    r.func("concat", vec![T::array_of(Any)], Text, concat)
        .variadic()
        .non_strict()
        .stable();
    r.func("concat_ws", vec![Text, T::array_of(Any)], Text, concat_ws)
        .variadic()
        .non_strict()
        .stable();
    r.func("split_part", vec![Text, Text, Int4], Text, split_part);
    r.func(
        "string_to_array",
        vec![Text, Text],
        T::array_of(Text),
        string_to_array,
    )
    .non_strict();
    r.func(
        "string_to_array",
        vec![Text, Text, Text],
        T::array_of(Text),
        string_to_array,
    )
    .non_strict();
    r.srf("string_to_table", vec![Text, Text], Text, string_to_table)
        .non_strict();
    r.srf("string_to_table", vec![Text, Text, Text], Text, string_to_table)
        .non_strict();
    r.func("parse_ident", vec![Text, Bool], T::array_of(Text), parse_ident)
        .names(&["str", "strict"])
        .min_args(1)
        .defaults(vec![Value::Bool(true)]);
    r.func("crc32", vec![Bytea], Int8, crc32);
    r.func("crc32c", vec![Bytea], Int8, crc32c);
    r.func("ascii", vec![Text], Int4, ascii);
    r.func("chr", vec![Int4], Text, chr);
    r.func("md5", vec![Text], Text, md5);
    r.func("md5", vec![Bytea], Text, md5);
    r.func("sha224", vec![Bytea], Bytea, sha224);
    r.func("sha256", vec![Bytea], Bytea, sha256);
    r.func("sha384", vec![Bytea], Bytea, sha384);
    r.func("sha512", vec![Bytea], Bytea, sha512);
    r.func("to_hex", vec![Int4], Text, to_hex);
    r.func("to_hex", vec![Int8], Text, to_hex);
    r.func("encode", vec![Bytea, Text], Text, encode);
    r.func("decode", vec![Text, Text], Bytea, decode);
    r.func("translate", vec![Text, Text, Text], Text, translate);
    r.func("overlay", vec![Text, Text, Int4], Text, overlay);
    r.func("overlay", vec![Text, Text, Int4, Int4], Text, overlay);
    r.func("overlay", vec![Bytea, Bytea, Int4], Bytea, bytea_overlay);
    r.func(
        "overlay",
        vec![Bytea, Bytea, Int4, Int4],
        Bytea,
        bytea_overlay,
    );
    r.func("position", vec![Bytea, Bytea], Int4, bytea_position);
    r.func("btrim", vec![Bytea, Bytea], Bytea, |a, _| {
        bytea_trim_ends(a, true, true)
    });
    r.func("ltrim", vec![Bytea, Bytea], Bytea, |a, _| {
        bytea_trim_ends(a, true, false)
    });
    r.func("rtrim", vec![Bytea, Bytea], Bytea, |a, _| {
        bytea_trim_ends(a, false, true)
    });
    r.func("starts_with", vec![Text, Text], Bool, starts_with);
    r.func("quote_ident", vec![Text], Text, quote_ident);
    r.func("quote_literal", vec![Text], Text, quote_literal);
    r.func("quote_literal", vec![AnyElement], Text, quote_literal)
        .stable();
    r.func("quote_nullable", vec![Text], Text, quote_nullable)
        .non_strict();
    r.func("quote_nullable", vec![AnyElement], Text, quote_nullable)
        .non_strict()
        .stable();
    r.func("format", vec![Text], Text, format)
        .non_strict()
        .stable();
    r.func("format", vec![Text, T::array_of(Any)], Text, format)
        .variadic()
        .non_strict()
        .stable();
    let text_array = T::array_of(Text);
    for args in [
        vec![Text, Text, Text],
        vec![Text, Text, Text, Text],
        vec![Text, Text, Text, Int4],
        vec![Text, Text, Text, Int4, Int4],
        vec![Text, Text, Text, Int4, Int4, Text],
    ] {
        r.func("regexp_replace", args, Text, regexp_replace::<false>);
    }
    for n in 2..=3 {
        let args = vec![Text; n];
        r.func("regexp_match", args.clone(), text_array.clone(), regexp_match::<false>);
        r.func("regexp_like", args.clone(), Bool, regexp_like::<false>);
        r.func(
            "regexp_split_to_array",
            args.clone(),
            text_array.clone(),
            regexp_split_to_array::<false>,
        );
        r.srf("regexp_matches", args.clone(), text_array.clone(), regexp_matches::<false>);
        r.srf("regexp_split_to_table", args, Text, regexp_split_to_table::<false>);
    }
    // (string, pattern [, start [, N [, endoption [, flags [, subexpr]]]]]) and the like.
    let full = [Text, Text, Int4, Int4, Int4, Text, Int4];
    for n in 2..=4 {
        let args = vec![Text, Text, Int4, Text][..n].to_vec();
        r.func("regexp_count", args, Int4, regexp_count::<false>);
    }
    for n in 2..=7 {
        r.func("regexp_instr", full[..n].to_vec(), Int4, regexp_instr::<false>);
    }
    let full = [Text, Text, Int4, Int4, Text, Int4];
    for n in 2..=6 {
        r.func("regexp_substr", full[..n].to_vec(), Text, regexp_substr::<false>);
    }
    r.func("like_escape", vec![Text, Text], Text, like_escape);
    r.func("similar_to_escape", vec![Text], Text, similar_to_escape);
    r.func(
        "similar_to_escape",
        vec![Text, Text],
        Text,
        similar_to_escape,
    );
    r.func("gen_random_uuid", vec![], Uuid, gen_random_uuid)
        .volatile();
    r.func("uuidv4", vec![], Uuid, gen_random_uuid).volatile();
    r.func("uuidv7", vec![], Uuid, uuidv7).volatile();
    r.func("uuidv7", vec![Interval(-1)], Uuid, uuidv7_interval)
        .volatile();
    r.func("uuid_extract_version", vec![Uuid], Int2, uuid_extract_version);
    r.func("uuid_extract_timestamp", vec![Uuid], Timestamptz(-1), uuid_extract_timestamp);
    r.func("unistr", vec![Text], Text, unistr);
    r.func("get_byte", vec![Bytea, Int4], Int4, get_byte);
    r.func("set_byte", vec![Bytea, Int4, Int4], Bytea, bytea_set_byte);
    r.func("get_bit", vec![Bytea, Int8], Int4, bytea_get_bit);
    r.func("set_bit", vec![Bytea, Int8, Int4], Bytea, bytea_set_bit);
    r.func("bit_count", vec![Bytea], Int8, bytea_bit_count);
    r.func("reverse", vec![Bytea], Bytea, reverse_bytea);
    r.func("to_bin", vec![Int4], Text, to_bin);
    r.func("to_bin", vec![Int8], Text, to_bin);
    r.func("to_oct", vec![Int4], Text, to_oct);
    r.func("to_oct", vec![Int8], Text, to_oct);
    r.func("like_escape", vec![Bytea, Bytea], Bytea, bytea_like_escape);
    r.func("convert_from", vec![Bytea, Name], Text, convert_from)
        .stable();
    r.func("convert_to", vec![Text, Name], Bytea, convert_to)
        .stable();
    r.func("convert", vec![Bytea, Name, Name], Bytea, convert)
        .stable();
    r.func("length", vec![Bytea, Name], Int4, length_in_encoding)
        .stable();
    r.func("pg_encoding_max_length", vec![Int4], Int4, pg_encoding_max_length);
    r.func("normalize", vec![Text], Text, normalize);
    r.func("normalize", vec![Text, Text], Text, normalize);
    r.func("is_normalized", vec![Text], Bool, is_normalized);
    r.func("is_normalized", vec![Text, Text], Bool, is_normalized);
    r.func("to_ascii", vec![Text], Text, to_ascii);
    r.func("to_ascii", vec![Text, Int4], Text, to_ascii);
    r.func("to_ascii", vec![Text, Name], Text, to_ascii);
    // LIKE family as operators and functions.
    for (op, f) in [
        ("~~", like as super::FnImpl),
        ("!~~", not_like),
        ("~~*", ilike),
        ("!~~*", not_ilike),
    ] {
        r.binop(op, Text, Text, Bool, f);
        r.binop(op, Name, Text, Bool, f);
        r.binop(
            op,
            Bpchar(-1),
            Text,
            Bool,
            if op == "~~" { bp_like } else { f },
        );
    }
    r.binop("~~", Bytea, Bytea, Bool, bytea_like);
    r.binop("!~~", Bytea, Bytea, Bool, bytea_not_like);
    r.func("like", vec![Text, Text], Bool, like);
    r.func("like", vec![Text, Text, Text], Bool, like);
    r.func("ilike", vec![Text, Text, Text], Bool, ilike);
    r.func("not_like", vec![Text, Text, Text], Bool, not_like);
    r.func("not_ilike", vec![Text, Text, Text], Bool, not_ilike);
    r.func("similar", vec![Text, Text], Bool, similar::<false>);
    r.func("similar", vec![Text, Text, Text], Bool, similar::<false>);
    r.func("not_similar", vec![Text, Text], Bool, not_similar::<false>);
    r.func("not_similar", vec![Text, Text, Text], Bool, not_similar::<false>);
    for (op, f) in [
        ("~", op_re::<false> as super::FnImpl),
        ("~*", op_re_ci::<false>),
        ("!~", op_not_re::<false>),
        ("!~*", op_not_re_ci::<false>),
    ] {
        r.binop(op, Text, Text, Bool, f);
        r.binop(op, Name, Text, Bool, f);
        r.binop(op, Bpchar(-1), Text, Bool, f);
    }
    r.binop("^@", Text, Text, Bool, starts_with);
    let _ = Ret::Poly;
}

// ── pure implementations ──

// (Two arguments: the default escape, as `escape_char` gives it.)
fn p_like(x: &Value, y: &Value, _: &crate::types::DataType) -> Result<Value> {
    Ok(Value::Bool(like_match(s(x), s(y), Some('\\'), false)?))
}
fn p_not_like(x: &Value, y: &Value, _: &crate::types::DataType) -> Result<Value> {
    Ok(Value::Bool(!like_match(s(x), s(y), Some('\\'), false)?))
}

/// `ops::pure` for LIKE (without a collation of the call's own: a nondeterministic one
/// matches otherwise).
pub(super) fn pure(imp: super::FnImpl) -> Option<super::ops::PureFn> {
    let table: [(super::FnImpl, super::ops::PureFn); 3] =
        [(like, p_like), (bp_like, p_like), (not_like, p_not_like)];
    table.iter().find(|(f, _)| std::ptr::fn_addr_eq(*f, imp)).map(|(_, p)| *p)
}
