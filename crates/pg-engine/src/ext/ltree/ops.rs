//! ltree's operators and functions (`ltree_op.c`, `lquery_op.c`, `_ltree_op.c`).

use super::data::{self, LQL_COUNT, LQL_NOT, LTREE_MAX_LEVELS, LVAR_ANYEND, LVAR_INCASE, LVAR_SUBLEXEME};
use super::lquery::Level;
use crate::Result;
use crate::error::{Error, code};
use crate::ext::types::{LQUERY, LTREE, LTXTQUERY};
use crate::sql::func::FnCtx;
use crate::types::Value;

/// `ltree_label_match` (the database's C locale: ASCII case folding).
pub fn label_match(pred: &[u8], label: &[u8], prefix: bool, ci: bool) -> bool {
    let fits = pred.len() == label.len() || (prefix && pred.len() < label.len());
    if fits && label.starts_with(pred) {
        return true;
    }
    if !ci || !fits {
        return false;
    }
    pred.iter().zip(label).all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// The `_`-separated words of a label (`getlexeme`).
fn lexemes(s: &[u8]) -> impl Iterator<Item = &[u8]> {
    s.split(|&c| c == b'_').filter(|w| !w.is_empty())
}

/// `compare_subnode`: every word of the query label matches some word of the label.
pub fn compare_subnode(label: &[u8], q: &[u8], prefix: bool, ci: bool) -> bool {
    lexemes(q).all(|qw| lexemes(label).any(|tw| label_match(qw, tw, prefix, ci)))
}

/// `checkLevel`.
fn check_level(q: &Level, label: &[u8]) -> bool {
    let success = q.flag & LQL_NOT == 0;
    if q.vars.is_empty() {
        return success;
    }
    for v in &q.vars {
        let prefix = v.flag & LVAR_ANYEND != 0;
        let ci = v.flag & LVAR_INCASE != 0;
        let hit = if v.flag & LVAR_SUBLEXEME != 0 {
            compare_subnode(label, &v.name, prefix, ci)
        } else {
            label_match(&v.name, label, prefix, ci)
        };
        if hit {
            return success;
        }
    }
    !success
}

/// `checkCond`: do levels `q` match labels `t`?
fn check_cond(q: &[Level], t: &[&[u8]]) -> Result<bool> {
    crate::stack::check()?;
    let (mut q, mut t) = (q, t);
    while let Some((cur, rest)) = q.split_first() {
        let (low, mut high) = if cur.flag & LQL_COUNT != 0 || cur.vars.is_empty() {
            (cur.low as usize, cur.high as usize)
        } else {
            (1, 1)
        };
        high = high.min(t.len());
        if high < low {
            return Ok(false);
        }
        for matchcnt in 0..high {
            if matchcnt >= low && check_cond(rest, t)? {
                return Ok(true);
            }
            if !check_level(cur, t[0]) {
                return Ok(false);
            }
            t = &t[1..];
        }
        q = rest;
    }
    Ok(t.is_empty())
}

pub fn lquery_matches(tree: &[u8], query: &[u8]) -> Result<bool> {
    check_cond(&super::lquery::decode(query), &data::labels(tree))
}

fn bytes(v: &Value) -> &[u8] {
    match v {
        Value::Ext(_, b) => b,
        _ => &[],
    }
}

fn ltree(b: Vec<u8>) -> Value {
    Value::Ext(LTREE, b.into())
}

fn cmp(a: &[Value]) -> std::cmp::Ordering {
    data::compare(bytes(&a[0]), bytes(&a[1]))
}

macro_rules! cmp_fns {
    ($($name:ident => $test:expr),* $(,)?) => {
        $(pub(super) fn $name(a: &[Value], _: &FnCtx) -> Result<Value> {
            Ok(Value::Bool($test(cmp(a))))
        })*
    };
}

cmp_fns! {
    lt => |o: std::cmp::Ordering| o.is_lt(),
    le => |o: std::cmp::Ordering| o.is_le(),
    eq => |o: std::cmp::Ordering| o.is_eq(),
    ge => |o: std::cmp::Ordering| o.is_ge(),
    gt => |o: std::cmp::Ordering| o.is_gt(),
    ne => |o: std::cmp::Ordering| o.is_ne(),
}

/// `ltree_cmp`: the first differing label's byte difference (or length difference), else
/// the difference in levels.
pub(super) fn ltree_cmp(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (data::labels(bytes(&a[0])), data::labels(bytes(&a[1])));
    for (p, q) in x.iter().zip(&y) {
        let n = p.len().min(q.len());
        if let Some((c, d)) = p[..n].iter().zip(&q[..n]).find(|(c, d)| c != d) {
            return Ok(Value::Int4(*c as i32 - *d as i32));
        }
        if p.len() != q.len() {
            return Ok(Value::Int4(p.len() as i32 - q.len() as i32));
        }
    }
    Ok(Value::Int4(x.len() as i32 - y.len() as i32))
}

pub(super) fn nlevel(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(data::labels(bytes(&a[0])).len() as i32))
}

/// `inner_isparent`: is `p` a prefix (by labels) of `c`?
fn isparent(c: &[u8], p: &[u8]) -> bool {
    let (c, p) = (data::labels(c), data::labels(p));
    p.len() <= c.len() && p.iter().zip(&c).all(|(x, y)| x == y)
}

pub(super) fn ltree_isparent(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(isparent(bytes(&a[1]), bytes(&a[0]))))
}

pub(super) fn ltree_risparent(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(isparent(bytes(&a[0]), bytes(&a[1]))))
}

/// `inner_subltree`.
fn subltree_of(t: &[u8], start: i32, mut end: i32) -> Result<Value> {
    let l = data::labels(t);
    let n = l.len() as i32;
    if start < 0 || end < 0 || start >= n || start > end {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid positions"));
    }
    end = end.min(n);
    Ok(ltree(data::encode(&l[start as usize..end as usize])))
}

pub(super) fn subltree(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (s, e) = (a[1].as_i64().unwrap_or(0) as i32, a[2].as_i64().unwrap_or(0) as i32);
    subltree_of(bytes(&a[0]), s, e)
}

pub(super) fn subpath(a: &[Value], _: &FnCtx) -> Result<Value> {
    let t = bytes(&a[0]);
    let n = data::labels(t).len() as i32;
    let mut start = a[1].as_i64().unwrap_or(0) as i32;
    let len = a.get(2).and_then(Value::as_i64).unwrap_or(0) as i32;
    let mut end = start.wrapping_add(len);
    if start < 0 {
        start += n;
        end = start + len;
    }
    if start < 0 {
        start += n;
        end = start + len;
    }
    if len < 0 {
        end = n + len;
    } else if len == 0 {
        end = if a.len() == 3 { start } else { 0xffff };
    }
    subltree_of(t, start, end)
}

fn concat(a: &[u8], b: &[u8]) -> Result<Value> {
    let n = data::labels(a).len() + data::labels(b).len();
    if n > LTREE_MAX_LEVELS {
        return Err(Error::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            format!("number of ltree levels ({n}) exceeds the maximum allowed ({LTREE_MAX_LEVELS})"),
        ));
    }
    let mut v = a.to_vec();
    v.extend_from_slice(b);
    Ok(ltree(v))
}

pub(super) fn addltree(a: &[Value], _: &FnCtx) -> Result<Value> {
    concat(bytes(&a[0]), bytes(&a[1]))
}

pub(super) fn addtext(a: &[Value], _: &FnCtx) -> Result<Value> {
    let t = data::parse(crate::sql::func::text_arg(&a[1]))?;
    concat(bytes(&a[0]), &t)
}

pub(super) fn textadd(a: &[Value], _: &FnCtx) -> Result<Value> {
    let t = data::parse(crate::sql::func::text_arg(&a[0]))?;
    concat(&t, bytes(&a[1]))
}

pub(super) fn ltree_index(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (data::labels(bytes(&a[0])), data::labels(bytes(&a[1])));
    let (an, bn) = (x.len() as i32, y.len() as i32);
    let mut start = a.get(2).and_then(Value::as_i64).unwrap_or(0) as i32;
    if start < 0 {
        start = if -start >= an { 0 } else { an + start };
    }
    if an - start < bn || an == 0 || bn == 0 {
        return Ok(Value::Int4(-1));
    }
    for i in start.max(0)..=(an - bn) {
        if x[i as usize..(i + bn) as usize] == y[..] {
            return Ok(Value::Int4(i));
        }
    }
    Ok(Value::Int4(-1))
}

/// `lca_inner`: the longest common prefix, short of each one's last label; None when one
/// is empty.
fn lca_of(all: &[&[u8]]) -> Option<Value> {
    let first = data::labels(all.first()?);
    if first.is_empty() {
        return None;
    }
    let mut num = first.len() - 1;
    for t in &all[1..] {
        let l = data::labels(t);
        if l.is_empty() {
            return None;
        } else if l.len() == 1 {
            num = 0;
        } else {
            let tmp = num.min(l.len() - 1);
            num = (0..tmp).take_while(|&i| first[i] == l[i]).count();
        }
    }
    Some(ltree(data::encode(&first[..num])))
}

pub(super) fn lca(a: &[Value], _: &FnCtx) -> Result<Value> {
    let all: Vec<&[u8]> = a.iter().map(bytes).collect();
    Ok(lca_of(&all).unwrap_or(Value::Null))
}

pub(super) fn ltree2text(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::text(data::output(bytes(&a[0]))))
}

pub(super) fn text2ltree(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(ltree(data::parse(crate::sql::func::text_arg(&a[0]))?))
}

pub(super) fn ltq_regex(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(lquery_matches(bytes(&a[0]), bytes(&a[1]))?))
}

pub(super) fn ltq_rregex(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(lquery_matches(bytes(&a[1]), bytes(&a[0]))?))
}

/// An array argument's elements: one-dimensional, no NULLs.
fn elems(v: &Value) -> Result<Vec<&[u8]>> {
    let Value::Array(a) = v else {
        return Ok(Vec::new());
    };
    if a.dims.len() > 1 {
        return Err(Error::new(code::ARRAY_SUBSCRIPT_ERROR, "array must be one-dimensional"));
    }
    if a.items.iter().any(Value::is_null) {
        return Err(Error::new(code::NULL_VALUE_NOT_ALLOWED, "array must not contain nulls"));
    }
    Ok(a.items.iter().map(bytes).collect())
}

pub(super) fn lt_q_regex(a: &[Value], _: &FnCtx) -> Result<Value> {
    let t = bytes(&a[0]);
    for q in elems(&a[1])? {
        if lquery_matches(t, q)? {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

pub(super) fn lt_q_rregex(a: &[Value], c: &FnCtx) -> Result<Value> {
    lt_q_regex(&[a[1].clone(), a[0].clone()], c)
}

fn txt_matches(t: &[u8], q: &[u8]) -> bool {
    let items = super::txt::decode(q);
    !items.is_empty() && super::txt::execute(&items, 0, &data::labels(t))
}

pub(super) fn ltxtq_exec(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(txt_matches(bytes(&a[0]), bytes(&a[1]))))
}

pub(super) fn ltxtq_rexec(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(txt_matches(bytes(&a[1]), bytes(&a[0]))))
}

/// `array_iterator`: the first element `test` accepts.
fn first_match(arr: &Value, mut test: impl FnMut(&[u8]) -> Result<bool>) -> Result<Option<&[u8]>> {
    for e in elems(arr)? {
        if test(e)? {
            return Ok(Some(e));
        }
    }
    Ok(None)
}

macro_rules! array_fns {
    ($($name:ident: ($arr:expr, $q:expr) => |$e:ident, $p:ident| $test:expr;)*) => {
        $(pub(super) fn $name(a: &[Value], _: &FnCtx) -> Result<Value> {
            let $p = bytes(&a[$q]);
            Ok(Value::Bool(first_match(&a[$arr], |$e| Ok::<bool, Error>($test))?.is_some()))
        })*
    };
}

// (`ltree_isparent(item, q)`: is the item a parent of q; `ltree_risparent(item, q)`: of q.)
array_fns! {
    a_isparent: (0, 1) => |e, p| isparent(p, e);
    a_r_isparent: (1, 0) => |e, p| isparent(p, e);
    a_risparent: (0, 1) => |e, p| isparent(e, p);
    a_r_risparent: (1, 0) => |e, p| isparent(e, p);
    a_ltxtq_exec: (0, 1) => |e, p| txt_matches(e, p);
    a_ltxtq_rexec: (1, 0) => |e, p| txt_matches(e, p);
}

pub(super) fn a_ltq_regex(a: &[Value], _: &FnCtx) -> Result<Value> {
    let q = bytes(&a[1]);
    Ok(Value::Bool(first_match(&a[0], |e| lquery_matches(e, q))?.is_some()))
}

pub(super) fn a_ltq_rregex(a: &[Value], c: &FnCtx) -> Result<Value> {
    a_ltq_regex(&[a[1].clone(), a[0].clone()], c)
}

pub(super) fn a_lt_q_regex(a: &[Value], _: &FnCtx) -> Result<Value> {
    for q in elems(&a[1])? {
        if first_match(&a[0], |e| lquery_matches(e, q))?.is_some() {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

pub(super) fn a_lt_q_rregex(a: &[Value], c: &FnCtx) -> Result<Value> {
    a_lt_q_regex(&[a[1].clone(), a[0].clone()], c)
}

fn found(f: Option<&[u8]>) -> Value {
    f.map_or(Value::Null, |b| ltree(b.to_vec()))
}

pub(super) fn extract_isparent(a: &[Value], _: &FnCtx) -> Result<Value> {
    let p = bytes(&a[1]);
    Ok(found(first_match(&a[0], |e| Ok(isparent(p, e)))?))
}

pub(super) fn extract_risparent(a: &[Value], _: &FnCtx) -> Result<Value> {
    let p = bytes(&a[1]);
    Ok(found(first_match(&a[0], |e| Ok(isparent(e, p)))?))
}

pub(super) fn extract_regex(a: &[Value], _: &FnCtx) -> Result<Value> {
    let q = bytes(&a[1]);
    Ok(found(first_match(&a[0], |e| lquery_matches(e, q))?))
}

pub(super) fn extract_exec(a: &[Value], _: &FnCtx) -> Result<Value> {
    let q = bytes(&a[1]);
    Ok(found(first_match(&a[0], |e| Ok(txt_matches(e, q)))?))
}

pub(super) fn a_lca(a: &[Value], _: &FnCtx) -> Result<Value> {
    let mut all = elems(&a[0])?;
    all.reverse();
    Ok(lca_of(&all).unwrap_or(Value::Null))
}

pub(super) fn hash_ltree(a: &[Value], _: &FnCtx) -> Result<Value> {
    let mut r: u32 = 1;
    for l in data::labels(bytes(&a[0])) {
        let h = crate::types::hash::hash_bytes_extended(l, 0) as u32;
        r = (r << 5).wrapping_sub(r).wrapping_add(h);
    }
    Ok(Value::Int4(r as i32))
}

pub(super) fn hash_ltree_extended(a: &[Value], _: &FnCtx) -> Result<Value> {
    let seed = a[1].as_i64().unwrap_or(0) as u64;
    let l = data::labels(bytes(&a[0]));
    if l.is_empty() {
        return Ok(Value::Int8(1u64.wrapping_add(seed) as i64));
    }
    let mut r: u64 = 1;
    for x in l {
        let h = crate::types::hash::hash_bytes_extended(x, seed);
        r = (r << 5).wrapping_sub(r).wrapping_add(h);
    }
    Ok(Value::Int8(r as i64))
}

/// `*_send`: version 1, then the text.
fn send(kind: u16, a: &[Value]) -> Value {
    let mut out = vec![1u8];
    out.extend(crate::ext::types::output_plain(kind, bytes(&a[0])).into_bytes());
    Value::Bytea(out.into())
}

pub(super) fn ltree_send(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(send(LTREE, a))
}
pub(super) fn lquery_send(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(send(LQUERY, a))
}
pub(super) fn ltxtq_send(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(send(LTXTQUERY, a))
}
