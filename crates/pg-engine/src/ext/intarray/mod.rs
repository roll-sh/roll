//! intarray (`contrib/intarray`, library `_int`): functions and operators over int4[] and the
//! `query_int` type. Arrays are taken as PostgreSQL's C code takes them: every element in
//! storage order whatever the dimensions, no NULLs ("array must not contain nulls").

pub mod query;

use super::native::Native;
use super::types::{ExtType, INTBIG_GKEY, QUERY_INT};
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::FnCtx;
use crate::types::array::ArrayValue;
use crate::types::{DataType, Value};
use std::sync::Arc;

pub(super) const QUERY_TYPE: ExtType = ExtType {
    kind: QUERY_INT,
    extension: "intarray",
    name: "query_int",
    input: |s| Ok(query::encode(&query::parse(s)?)),
    output: |b, _| query::output(&query::decode(b)),
    cmp: |a, b| a.cmp(b),
    canon: |b| std::borrow::Cow::Borrowed(b),
    key: super::types::key_bytes,
    to_builtin: None,
    from_builtin: None,
    hash: false,
};

pub(super) const GKEY_TYPE: ExtType = ExtType {
    kind: INTBIG_GKEY,
    extension: "intarray",
    name: "intbig_gkey",
    input: |_| {
        Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot accept a value of type intbig_gkey"))
    },
    output: |_, _| String::new(),
    cmp: |a, b| a.cmp(b),
    canon: |b| std::borrow::Cow::Borrowed(b),
    key: super::types::key_bytes,
    to_builtin: None,
    from_builtin: None,
    hash: false,
};

/// An int4[] argument: its dimensions and elements (CHECKARRVALID).
struct IntArr {
    dims: Vec<(usize, i32)>,
    v: Vec<i32>,
}

fn arr(v: &Value) -> Result<IntArr> {
    let Value::Array(a) = v else {
        return Ok(IntArr { dims: Vec::new(), v: Vec::new() });
    };
    let mut out = Vec::with_capacity(a.items.len());
    for x in &a.items {
        match x.as_i64() {
            Some(n) => out.push(n as i32),
            None => {
                return Err(Error::new(code::NULL_VALUE_NOT_ALLOWED, "array must not contain nulls"));
            }
        }
    }
    Ok(IntArr { dims: a.dims.clone(), v: out })
}

impl IntArr {
    /// The same array with `n` elements (`resize_intArrayType`): the first dimension's
    /// length becomes `n` and the others 1; empty when `n` is 0.
    fn resized(mut self, v: Vec<i32>) -> Value {
        if v.is_empty() {
            return empty();
        }
        if v.len() != self.v.len() {
            let mut n = v.len();
            for d in self.dims.iter_mut() {
                d.0 = n;
                n = 1;
            }
        }
        self.v = v;
        self.value()
    }

    fn value(self) -> Value {
        if self.v.is_empty() {
            return empty();
        }
        Value::Array(Arc::new(ArrayValue {
            elem: DataType::Int4,
            dims: self.dims,
            items: self.v.into_iter().map(Value::Int4).collect(),
        }))
    }
}

fn empty() -> Value {
    Value::Array(Arc::new(ArrayValue::empty(DataType::Int4)))
}

/// A new one-dimensional array (`new_intArrayType`).
fn new_arr(v: Vec<i32>) -> Value {
    Value::Array(Arc::new(ArrayValue::from_vec(DataType::Int4, v.into_iter().map(Value::Int4).collect())))
}

fn sorted(mut v: Vec<i32>) -> Vec<i32> {
    v.sort_unstable();
    v
}

fn uniq_sorted(mut v: Vec<i32>) -> Vec<i32> {
    v.sort_unstable();
    v.dedup();
    v
}

fn contains(a: &[i32], b: &[i32]) -> bool {
    // inner_int_contains over sorted, unique arrays.
    let (mut i, mut j, mut n) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            i += 1;
        } else if a[i] == b[j] {
            n += 1;
            i += 1;
            j += 1;
        } else {
            break;
        }
    }
    n == b.len()
}

fn overlap(a: &[i32], b: &[i32]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            i += 1;
        } else if a[i] == b[j] {
            return true;
        } else {
            j += 1;
        }
    }
    false
}

fn b(v: bool) -> Result<Value> {
    Ok(Value::Bool(v))
}

fn int_contains(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (arr(&a[0])?, arr(&a[1])?);
    b(contains(&uniq_sorted(x.v), &uniq_sorted(y.v)))
}
fn int_contained(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (arr(&a[0])?, arr(&a[1])?);
    b(contains(&uniq_sorted(y.v), &uniq_sorted(x.v)))
}
fn same(a: &[Value]) -> Result<bool> {
    let (x, y) = (arr(&a[0])?, arr(&a[1])?);
    Ok(x.v.len() == y.v.len() && sorted(x.v) == sorted(y.v))
}
fn int_same(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(same(a)?)
}
fn int_different(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(!same(a)?)
}
fn int_overlap(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (arr(&a[0])?, arr(&a[1])?);
    b(overlap(&sorted(x.v), &sorted(y.v)))
}
fn int_union(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (arr(&a[0])?, arr(&a[1])?);
    let mut v = x.v;
    v.extend(y.v);
    Ok(new_arr(uniq_sorted(v)))
}
fn int_inter(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (arr(&a[0])?, arr(&a[1])?);
    let (p, q) = (sorted(x.v), sorted(y.v));
    let (mut i, mut j) = (0, 0);
    let mut r: Vec<i32> = Vec::new();
    while i < p.len() && j < q.len() {
        if p[i] < q[j] {
            i += 1;
        } else if p[i] == q[j] {
            if r.last() != Some(&q[j]) {
                r.push(q[j]);
            }
            i += 1;
            j += 1;
        } else {
            j += 1;
        }
    }
    Ok(new_arr(r))
}
fn intset(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(new_arr(vec![a[0].as_i64().unwrap_or(0) as i32]))
}
fn icount(a: &[Value], _: &FnCtx) -> Result<Value> {
    // (ARRNELEMS, without the null check.)
    Ok(Value::Int4(match &a[0] {
        Value::Array(x) => x.items.len() as i32,
        _ => 0,
    }))
}
fn sort_dir(a: &[Value], asc: bool) -> Result<Value> {
    let mut x = arr(&a[0])?;
    if asc {
        x.v.sort_unstable();
    } else {
        x.v.sort_unstable_by(|p, q| q.cmp(p));
    }
    Ok(x.value())
}
fn sort(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = arr(&a[0])?;
    if x.v.len() < 2 {
        return Ok(x.value());
    }
    let asc = match a.get(1) {
        None => true,
        Some(d) => match crate::sql::func::text_arg(d) {
            d if d.eq_ignore_ascii_case("asc") => true,
            d if d.eq_ignore_ascii_case("desc") => false,
            _ => {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    "second parameter must be \"ASC\" or \"DESC\"",
                ));
            }
        },
    };
    sort_dir(a, asc)
}
fn sort_asc(a: &[Value], _: &FnCtx) -> Result<Value> {
    sort_dir(a, true)
}
fn sort_desc(a: &[Value], _: &FnCtx) -> Result<Value> {
    sort_dir(a, false)
}
fn uniq(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = arr(&a[0])?;
    if x.v.len() < 2 {
        return Ok(x.value());
    }
    let mut v = x.v.clone();
    v.dedup();
    Ok(x.resized(v))
}
fn idx(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = arr(&a[0])?;
    let e = a[1].as_i64().unwrap_or(0) as i32;
    Ok(Value::Int4(x.v.iter().position(|&n| n == e).map_or(0, |p| p as i32 + 1)))
}
fn subarray(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = arr(&a[0])?;
    let start = a[1].as_i64().unwrap_or(0) as i32;
    let len = a.get(2).and_then(Value::as_i64).unwrap_or(0) as i32;
    let mut start = if start > 0 { start - 1 } else { start };
    if x.v.is_empty() {
        return Ok(empty());
    }
    let c = x.v.len() as i32;
    if start < 0 {
        start += c;
    }
    let mut end = if len < 0 {
        c + len
    } else if len == 0 {
        c
    } else {
        start + len
    };
    end = end.min(c);
    start = start.max(0);
    if start >= end || end <= 0 {
        return Ok(empty());
    }
    Ok(new_arr(x.v[start as usize..end as usize].to_vec()))
}
fn push_elem(a: &[Value], _: &FnCtx) -> Result<Value> {
    let mut v = arr(&a[0])?.v;
    v.push(a[1].as_i64().unwrap_or(0) as i32);
    Ok(new_arr(v))
}
fn push_array(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (arr(&a[0])?, arr(&a[1])?);
    let mut v = x.v;
    v.extend(y.v);
    Ok(new_arr(v))
}
fn del_elem(a: &[Value], _: &FnCtx) -> Result<Value> {
    let x = arr(&a[0])?;
    if x.v.is_empty() {
        return Ok(x.value());
    }
    let e = a[1].as_i64().unwrap_or(0) as i32;
    let v: Vec<i32> = x.v.iter().copied().filter(|&n| n != e).collect();
    Ok(x.resized(v))
}
fn union_elem(a: &[Value], _: &FnCtx) -> Result<Value> {
    let mut v = arr(&a[0])?.v;
    v.push(a[1].as_i64().unwrap_or(0) as i32);
    Ok(new_arr(uniq_sorted(v)))
}
fn subtract(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (arr(&a[0])?, arr(&a[1])?);
    let (p, q) = (uniq_sorted(x.v), uniq_sorted(y.v));
    let r: Vec<i32> = p.into_iter().filter(|n| q.binary_search(n).is_err()).collect();
    Ok(new_arr(r))
}

fn query_items(v: &Value) -> Vec<query::Item> {
    match v {
        Value::Ext(_, b) => query::decode(b),
        _ => Vec::new(),
    }
}

/// `boolop`: the query over the array's sorted, unique elements.
fn boolop_of(arr_v: &Value, q: &Value) -> Result<Value> {
    let v = uniq_sorted(arr(arr_v)?.v);
    let items = query_items(q);
    if items.is_empty() {
        return b(false);
    }
    b(query::execute(&items, items.len() - 1, &v, true))
}
fn boolop(a: &[Value], _: &FnCtx) -> Result<Value> {
    boolop_of(&a[0], &a[1])
}
fn rboolop(a: &[Value], _: &FnCtx) -> Result<Value> {
    boolop_of(&a[1], &a[0])
}
fn querytree(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(Error::new(code::INTERNAL_ERROR, "querytree is no longer implemented"))
}
fn bqarr_in(a: &[Value], _: &FnCtx) -> Result<Value> {
    super::types::input(QUERY_INT, crate::sql::func::text_arg(&a[0]))
}
fn bqarr_out(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::text(super::types::output_plain(QUERY_INT, match &a[0] {
        Value::Ext(_, b) => b,
        _ => &[],
    })))
}
fn intbig_in(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot accept a value of type intbig_gkey"))
}
fn intbig_out(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot display a value of type intbig_gkey"))
}

/// Index access method support and selectivity functions: they take `internal` arguments, so
/// SQL never calls them; the engine's indexes do their work their own way.
fn support(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::error::internal("an index support function cannot be called from SQL"))
}

const SUPPORT: &[&str] = &[
    "_int_matchsel",
    "_int_overlap_sel",
    "_int_contains_sel",
    "_int_contained_sel",
    "_int_overlap_joinsel",
    "_int_contains_joinsel",
    "_int_contained_joinsel",
    "g_int_consistent",
    "g_int_compress",
    "g_int_decompress",
    "g_int_penalty",
    "g_int_picksplit",
    "g_int_union",
    "g_int_same",
    "g_int_options",
    "g_intbig_consistent",
    "g_intbig_compress",
    "g_intbig_decompress",
    "g_intbig_penalty",
    "g_intbig_picksplit",
    "g_intbig_union",
    "g_intbig_same",
    "g_intbig_options",
    "ginint4_queryextract",
    "ginint4_consistent",
];

/// The options an operator class's options support function accepts (its
/// `add_local_int_reloption`s): (name, default, min, max).
pub(super) fn options(sym: &str) -> Option<Vec<(&'static str, i64, i64, i64, i64)>> {
    Some(match sym {
        // G_INT_NUMRANGES_MAX: (GISTMaxIndexKeySize - VARHDRSZ) / (2 * sizeof(int32)).
        "g_int_options" => vec![("numranges", 100, 1, 252, 1)],
        "g_intbig_options" => vec![("siglen", 252, 1, 2024, 1)],
        _ => return None,
    })
}

/// What an index of the operator class whose compress function is `compress` checks of a
/// new key (`g_int_compress`: a leaf key of `2 * numranges` distinct elements or more).
pub(super) fn check_key(compress: &str, key: &Value, opts: &[String]) -> Result<()> {
    if compress != "g_int_compress" || key.is_null() {
        return Ok(());
    }
    let ranges = opts
        .iter()
        .find_map(|o| o.strip_prefix("numranges="))
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(100);
    let n = uniq_sorted(arr(key)?.v).len() as i64;
    if n >= 2 * ranges {
        return Err(Error::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            format!(
                "input array is too big ({} maximum allowed, {n} current), use gist__intbig_ops opclass instead",
                2 * ranges - 1
            ),
        ));
    }
    Ok(())
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    if SUPPORT.contains(&sym) {
        return Some(Native::scalar(support));
    }
    Some(Native::scalar(match sym {
        "_int_contains" => int_contains,
        "_int_contained" => int_contained,
        "_int_same" => int_same,
        "_int_different" => int_different,
        "_int_overlap" => int_overlap,
        "_int_union" => int_union,
        "_int_inter" => int_inter,
        "intset" => intset,
        "icount" => icount,
        "sort" => sort,
        "sort_asc" => sort_asc,
        "sort_desc" => sort_desc,
        "uniq" => uniq,
        "idx" => idx,
        "subarray" => subarray,
        "intarray_push_elem" => push_elem,
        "intarray_push_array" => push_array,
        "intarray_del_elem" => del_elem,
        "intset_union_elem" => union_elem,
        "intset_subtract" => subtract,
        "boolop" => boolop,
        "rboolop" => rboolop,
        "querytree" => querytree,
        "bqarr_in" => bqarr_in,
        "bqarr_out" => bqarr_out,
        "_intbig_in" => intbig_in,
        "_intbig_out" => intbig_out,
        _ => return None,
    }))
}
