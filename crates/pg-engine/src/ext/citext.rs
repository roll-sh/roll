//! citext (`contrib/citext/citext.c`): text compared case-insensitively. Values are the
//! text's bytes; equality and hashing compare them lowercased (`str_tolower` under the
//! database's default collation, which for the C locale lowers ASCII letters only),
//! ordering compares the lowercased strings under the call's collation.

use super::native::Native;
use super::types::{CITEXT, ExtType, key_bytes};
use crate::Result;
use crate::sql::func::FnCtx;
use crate::types::Value;
use std::borrow::Cow;
use std::cmp::Ordering;

pub(super) const TYPE: ExtType = ExtType {
    kind: CITEXT,
    extension: "citext",
    name: "citext",
    input: |s| Ok(s.as_bytes().to_vec()),
    output: |b, _| String::from_utf8_lossy(b).into_owned(),
    // (Under the database default, citext's collation unless a COLLATE says otherwise.)
    cmp: |a, b| default_cmp(&lower(a), &lower(b)),
    canon: lower,
    key: |a, out| key_bytes(&lower(a), out),
    to_builtin: Some(|b| Value::Text(String::from_utf8_lossy(b).into())),
    from_builtin: Some(|v| match v {
        Value::Text(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    }),
    hash: true,
};

/// `str_tolower` under the database's default collation (citext lowers with
/// DEFAULT_COLLATION_OID): ASCII letters only under a C ctype.
fn lower(b: &[u8]) -> Cow<'_, [u8]> {
    let d = crate::types::collation::default();
    if !d.ctype_is_c()
        && let Ok(s) = std::str::from_utf8(b)
    {
        return Cow::Owned(d.lower(s).into_bytes());
    }
    if b.iter().any(u8::is_ascii_uppercase) {
        Cow::Owned(b.to_ascii_lowercase())
    } else {
        Cow::Borrowed(b)
    }
}

/// Lowercased strings under the database default (`varstr_cmp` with DEFAULT_COLLATION_OID).
fn default_cmp(l: &[u8], r: &[u8]) -> Ordering {
    match crate::types::collation::default_unless_bytewise() {
        Some(c) => c.cmp(&String::from_utf8_lossy(l), &String::from_utf8_lossy(r)),
        None => l.cmp(r),
    }
}

fn bytes(v: &Value) -> &[u8] {
    match v {
        Value::Ext(_, b) => b,
        Value::Text(s) => s.as_bytes(),
        _ => &[],
    }
}

/// `citextcmp`: the lowercased strings under the input collation.
fn citextcmp(a: &Value, b: &Value, c: &FnCtx) -> Ordering {
    let (l, r) = (lower(bytes(a)), lower(bytes(b)));
    match c.coll {
        Some(coll) => coll.cmp(&String::from_utf8_lossy(&l), &String::from_utf8_lossy(&r)),
        None => l.cmp(&r),
    }
}

/// `internal_citext_pattern_cmp`: the lowercased bytes, then the lengths.
fn pattern_cmp(a: &Value, b: &Value) -> Ordering {
    let (l, r) = (lower(bytes(a)), lower(bytes(b)));
    let n = l.len().min(r.len());
    l[..n].cmp(&r[..n]).then(l.len().cmp(&r.len()))
}

fn ord(o: Ordering) -> Value {
    Value::Int4(match o {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    })
}

fn eq(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(lower(bytes(&a[0])) == lower(bytes(&a[1]))))
}
fn ne(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(lower(bytes(&a[0])) != lower(bytes(&a[1]))))
}
fn lt(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(citextcmp(&a[0], &a[1], c).is_lt()))
}
fn le(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(citextcmp(&a[0], &a[1], c).is_le()))
}
fn gt(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(citextcmp(&a[0], &a[1], c).is_gt()))
}
fn ge(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(citextcmp(&a[0], &a[1], c).is_ge()))
}
fn cmp(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(ord(citextcmp(&a[0], &a[1], c)))
}
fn smaller(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(if citextcmp(&a[0], &a[1], c).is_lt() { a[0].clone() } else { a[1].clone() })
}
fn larger(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(if citextcmp(&a[0], &a[1], c).is_gt() { a[0].clone() } else { a[1].clone() })
}
fn hash(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h = crate::types::hash::hash_bytes_extended(&lower(bytes(&a[0])), 0);
    Ok(Value::Int4(h as u32 as i32))
}
fn hash_extended(a: &[Value], _: &FnCtx) -> Result<Value> {
    let seed = a[1].as_i64().unwrap_or(0) as u64;
    let h = crate::types::hash::hash_bytes_extended(&lower(bytes(&a[0])), seed);
    Ok(Value::Int8(h as i64))
}
fn pattern_lt(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(pattern_cmp(&a[0], &a[1]).is_lt()))
}
fn pattern_le(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(pattern_cmp(&a[0], &a[1]).is_le()))
}
fn pattern_gt(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(pattern_cmp(&a[0], &a[1]).is_gt()))
}
fn pattern_ge(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(pattern_cmp(&a[0], &a[1]).is_ge()))
}
fn pattern_cmp_fn(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(ord(pattern_cmp(&a[0], &a[1])))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(Native::scalar(match sym {
        "citext_eq" => eq,
        "citext_ne" => ne,
        "citext_lt" => lt,
        "citext_le" => le,
        "citext_gt" => gt,
        "citext_ge" => ge,
        "citext_cmp" => cmp,
        "citext_smaller" => smaller,
        "citext_larger" => larger,
        "citext_hash" => hash,
        "citext_hash_extended" => hash_extended,
        "citext_pattern_lt" => pattern_lt,
        "citext_pattern_le" => pattern_le,
        "citext_pattern_gt" => pattern_gt,
        "citext_pattern_ge" => pattern_ge,
        "citext_pattern_cmp" => pattern_cmp_fn,
        _ => return None,
    }))
}
