//! Range and multirange functions and operators.

use super::{FnCtx, Registry, Ret, SrfIter};
use crate::error::{Error, Result, code};
use crate::types::range::{self as rg, Bound, Range, RangeType};
use crate::types::{DataType as T, Value};
use std::cmp::Ordering;
use std::sync::Arc;

/// The range type of argument `i` (or of the result).
fn rtype(c: &FnCtx, i: usize) -> Arc<RangeType> {
    match (&c.arg_type(i), c.ret) {
        (T::Range(rt) | T::Multirange(rt), _) | (_, T::Range(rt) | T::Multirange(rt)) => rt.clone(),
        _ => rg::builtins()[0].clone(),
    }
}

fn ret_type(c: &FnCtx) -> Arc<RangeType> {
    match c.ret {
        T::Range(rt) | T::Multirange(rt) => rt.clone(),
        _ => rtype(c, 0),
    }
}

fn rng(v: &Value) -> Range {
    match v {
        Value::Range(r) => (**r).clone(),
        _ => Range::empty(),
    }
}

fn multi(v: &Value) -> Vec<Range> {
    match v {
        Value::Multirange(m) => (**m).clone(),
        Value::Range(r) if !r.empty => vec![(**r).clone()],
        _ => Vec::new(),
    }
}

fn b(x: bool) -> Result<Value> {
    Ok(Value::Bool(x))
}

// ── constructors ──

fn flags(s: &str) -> Result<(bool, bool)> {
    match s {
        "[]" => Ok((true, true)),
        "[)" => Ok((true, false)),
        "(]" => Ok((false, true)),
        "()" => Ok((false, false)),
        _ => Err(Error::new(code::SYNTAX_ERROR, "invalid range bound flags")
            .hint("Valid values are \"[]\", \"[)\", \"(]\", and \"()\".")),
    }
}

fn construct(a: &[Value], c: &FnCtx) -> Result<Value> {
    let rt = ret_type(c);
    let (li, ui) = match a.get(2) {
        None => (true, false),
        Some(Value::Null) => {
            return Err(Error::new(
                code::DATA_EXCEPTION,
                "range constructor flags argument must not be null",
            ));
        }
        Some(Value::Text(s)) => flags(s)?,
        Some(_) => (true, false),
    };
    let val = |v: &Value| if v.is_null() { None } else { Some(v.clone()) };
    let r = rg::make(
        &rt,
        Bound {
            val: val(&a[0]),
            inclusive: li,
            lower: true,
        },
        Bound {
            val: val(&a[1]),
            inclusive: ui,
            lower: false,
        },
    )?;
    Ok(rg::value(r))
}

/// `int4range_canonical` and friends: stored ranges are canonical already.
fn canonical(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(a[0].clone())
}

fn multi_construct(a: &[Value], c: &FnCtx) -> Result<Value> {
    let rt = ret_type(c);
    let mut ranges = Vec::new();
    for v in a {
        match v {
            Value::Range(r) => ranges.push((**r).clone()),
            Value::Array(arr) => {
                for item in &arr.items {
                    match item {
                        Value::Range(r) => ranges.push((**r).clone()),
                        Value::Null => {
                            return Err(Error::new(
                                code::NULL_VALUE_NOT_ALLOWED,
                                "multirange values cannot contain null members",
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Value::Null => {
                return Err(Error::new(
                    code::NULL_VALUE_NOT_ALLOWED,
                    "multirange values cannot contain null members",
                ));
            }
            _ => {}
        }
    }
    Ok(rg::multi_value(rg::normalize(&rt, ranges)?))
}

fn multi_of_range(a: &[Value], c: &FnCtx) -> Result<Value> {
    let rt = ret_type(c);
    Ok(rg::multi_value(rg::normalize(&rt, vec![rng(&a[0])])?))
}

// ── accessors ──

fn lower(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(match &a[0] {
        Value::Range(r) if !r.empty => r.lower.clone().unwrap_or(Value::Null),
        Value::Multirange(m) => m
            .first()
            .and_then(|r| r.lower.clone())
            .unwrap_or(Value::Null),
        _ => Value::Null,
    })
}

fn upper(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(match &a[0] {
        Value::Range(r) if !r.empty => r.upper.clone().unwrap_or(Value::Null),
        Value::Multirange(m) => m
            .last()
            .and_then(|r| r.upper.clone())
            .unwrap_or(Value::Null),
        _ => Value::Null,
    })
}

fn first_last(v: &Value) -> Option<(Range, Range)> {
    match v {
        Value::Range(r) if !r.empty => Some(((**r).clone(), (**r).clone())),
        Value::Multirange(m) if !m.is_empty() => Some((m[0].clone(), m[m.len() - 1].clone())),
        _ => None,
    }
}

fn isempty(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(match &a[0] {
        Value::Range(r) => r.empty,
        Value::Multirange(m) => m.is_empty(),
        _ => true,
    })
}

fn lower_inc(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(first_last(&a[0]).is_some_and(|(f, _)| f.lower_inc))
}

fn upper_inc(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(first_last(&a[0]).is_some_and(|(_, l)| l.upper_inc))
}

fn lower_inf(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(first_last(&a[0]).is_some_and(|(f, _)| f.lower.is_none()))
}

fn upper_inf(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(first_last(&a[0]).is_some_and(|(_, l)| l.upper.is_none()))
}

// ── range operators ──

fn range_eq(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(a[0].cmp_total(&a[1]) == Ordering::Equal)
}

fn range_ne(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(a[0].cmp_total(&a[1]) != Ordering::Equal)
}

fn range_lt(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(a[0].cmp_total(&a[1]) == Ordering::Less)
}

fn range_le(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(a[0].cmp_total(&a[1]) != Ordering::Greater)
}

fn range_gt(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(a[0].cmp_total(&a[1]) == Ordering::Greater)
}

fn range_ge(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(a[0].cmp_total(&a[1]) != Ordering::Less)
}

fn range_cmp(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(match a[0].cmp_total(&a[1]) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }))
}

fn range_contains(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(rg::contains(&rng(&a[0]), &rng(&a[1])))
}

fn range_contained_by(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(rg::contains(&rng(&a[1]), &rng(&a[0])))
}

fn range_contains_elem(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(rg::contains_elem(&rng(&a[0]), &a[1]))
}

fn elem_contained_by_range(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(rg::contains_elem(&rng(&a[1]), &a[0]))
}

fn range_overlaps(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(rg::overlaps(&rng(&a[0]), &rng(&a[1])))
}

fn range_before(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(rg::before(&rng(&a[0]), &rng(&a[1])))
}

fn range_after(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(rg::after(&rng(&a[0]), &rng(&a[1])))
}

fn range_overleft(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(rg::overleft(&rng(&a[0]), &rng(&a[1])))
}

fn range_overright(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(rg::overright(&rng(&a[0]), &rng(&a[1])))
}

fn range_adjacent(a: &[Value], c: &FnCtx) -> Result<Value> {
    b(rg::adjacent(&rtype(c, 0), &rng(&a[0]), &rng(&a[1])))
}

fn range_union(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(rg::value(rg::union(
        &rtype(c, 0),
        &rng(&a[0]),
        &rng(&a[1]),
        true,
    )?))
}

fn range_merge(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(rg::value(rg::union(
        &rtype(c, 0),
        &rng(&a[0]),
        &rng(&a[1]),
        false,
    )?))
}

fn range_intersect(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(rg::value(rg::intersect(
        &rtype(c, 0),
        &rng(&a[0]),
        &rng(&a[1]),
    )?))
}

fn range_minus(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(rg::value(rg::minus(
        &rtype(c, 0),
        &rng(&a[0]),
        &rng(&a[1]),
    )?))
}

/// `int4range_subdiff` and friends: upper - lower as float8.
fn subdiff(a: &[Value], _: &FnCtx) -> Result<Value> {
    let f = |v: &Value| -> f64 {
        match v {
            Value::Int4(i) => *i as f64,
            Value::Int8(i) => *i as f64,
            Value::Date(d) => *d as f64,
            Value::Numeric(n) => n.to_f64(),
            Value::Timestamp(t) | Value::Timestamptz(t) => *t as f64 / 1_000_000.0,
            v => v.as_f64().unwrap_or(0.0),
        }
    };
    Ok(Value::Float8(f(&a[0]) - f(&a[1])))
}

// ── multirange operators ──

fn mr_contains_elem(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(multi(&a[0]).iter().any(|r| rg::contains_elem(r, &a[1])))
}

fn elem_contained_by_mr(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(multi(&a[1]).iter().any(|r| rg::contains_elem(r, &a[0])))
}

fn mr_contains_range(m: &[Range], r: &Range) -> bool {
    r.empty || m.iter().any(|x| rg::contains(x, r))
}

fn range_contains_mr(r: &Range, m: &[Range]) -> bool {
    match (m.first(), m.last()) {
        (Some(f), Some(l)) => {
            !r.empty
                && rg::cmp_bounds(&r.lower_bound(), &f.lower_bound()) != Ordering::Greater
                && rg::cmp_bounds(&r.upper_bound(), &l.upper_bound()) != Ordering::Less
        }
        _ => true,
    }
}

fn mr_contains(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (&a[0], &a[1]);
    b(match (x, y) {
        (Value::Multirange(m1), Value::Multirange(m2)) => {
            m2.iter().all(|r| mr_contains_range(m1, r))
        }
        (Value::Multirange(m), Value::Range(r)) => mr_contains_range(m, r),
        (Value::Range(r), Value::Multirange(m)) => range_contains_mr(r, m),
        _ => false,
    })
}

fn mr_contained_by(a: &[Value], c: &FnCtx) -> Result<Value> {
    mr_contains(&[a[1].clone(), a[0].clone()], c)
}

fn mr_overlaps(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (m1, m2) = (multi(&a[0]), multi(&a[1]));
    b(m1.iter().any(|x| m2.iter().any(|y| rg::overlaps(x, y))))
}

/// Both sides' first and last ranges, unless either side is empty.
fn ends(a: &[Value]) -> Option<((Range, Range), (Range, Range))> {
    Some((first_last(&a[0])?, first_last(&a[1])?))
}

fn mr_before(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(ends(a).is_some_and(|((_, l1), (f2, _))| {
        rg::cmp_bounds(&l1.upper_bound(), &f2.lower_bound()) == Ordering::Less
    }))
}

fn mr_after(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(ends(a).is_some_and(|((f1, _), (_, l2))| {
        rg::cmp_bounds(&f1.lower_bound(), &l2.upper_bound()) == Ordering::Greater
    }))
}

fn mr_overleft(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(ends(a).is_some_and(|((_, l1), (_, l2))| {
        rg::cmp_bounds(&l1.upper_bound(), &l2.upper_bound()) != Ordering::Greater
    }))
}

fn mr_overright(a: &[Value], _: &FnCtx) -> Result<Value> {
    b(ends(a).is_some_and(|((f1, _), (f2, _))| {
        rg::cmp_bounds(&f1.lower_bound(), &f2.lower_bound()) != Ordering::Less
    }))
}

fn mr_adjacent(a: &[Value], c: &FnCtx) -> Result<Value> {
    let rt = rtype(c, 0);
    // (multirange_adjacent_multirange: the left's last range before the right's first, or
    // the right's last before the left's first)
    b(ends(a).is_some_and(|((f1, l1), (f2, l2))| {
        rg::adjacent_after(&rt, &l1, &f2) || rg::adjacent_after(&rt, &l2, &f1)
    }))
}

fn mr_union(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(rg::multi_value(rg::multi_union(
        &rtype(c, 0),
        &multi(&a[0]),
        &multi(&a[1]),
    )?))
}

fn mr_minus(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(rg::multi_value(rg::multi_minus(
        &rtype(c, 0),
        &multi(&a[0]),
        &multi(&a[1]),
    )?))
}

fn mr_intersect(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(rg::multi_value(rg::multi_intersect(
        &rtype(c, 0),
        &multi(&a[0]),
        &multi(&a[1]),
    )?))
}

fn mr_merge(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(rg::value(rg::span(&rtype(c, 0), &multi(&a[0]))?))
}

fn mr_unnest(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    let items: Vec<Result<Vec<Value>>> = multi(&a[0])
        .into_iter()
        .map(|r| Ok(vec![rg::value(r)]))
        .collect();
    Ok(Box::new(items.into_iter()))
}

/// A range type's constructor functions and its multirange's, with their pg_proc oids when
/// the catalog has them (`oids`, in this order).
fn constructors(
    r: &mut Registry,
    rt: &Arc<RangeType>,
    (name, multi_name): (&str, &str),
    oids: &[u32],
) {
    let (name, multi_name): (&'static str, &'static str) = (
        Box::leak(name.to_string().into_boxed_str()),
        Box::leak(multi_name.to_string().into_boxed_str()),
    );
    let st = rt.subtype.clone();
    let range = T::Range(rt.clone());
    let mr = T::Multirange(rt.clone());
    let defs = [
        r.func(name, vec![st.clone(), st.clone()], range.clone(), construct)
            .non_strict()
            .oid,
        r.func(
            name,
            vec![st.clone(), st.clone(), T::Text],
            range.clone(),
            construct,
        )
        .non_strict()
        .oid,
        r.func(multi_name, vec![], mr.clone(), multi_construct).oid,
        r.func(multi_name, vec![range.clone()], mr.clone(), multi_of_range)
            .oid,
        r.func(multi_name, vec![T::array_of(range)], mr, multi_construct)
            .variadic()
            .oid,
    ];
    for (old, new) in defs
        .into_iter()
        .zip(oids.iter().copied().filter(|o| *o != 0))
    {
        for d in r.funcs.values_mut().flatten() {
            if d.oid == old
                && let Some(d) = Arc::get_mut(d)
            {
                d.oid = new;
            }
        }
    }
}

/// The constructors named `name` of a user-defined range type (CREATE TYPE ... AS RANGE),
/// whose range and multirange constructors are named `ctors` and have pg_proc oids `oids`;
/// built once per type.
pub fn user_constructors(
    rt: &Arc<RangeType>,
    ctors: (&str, &str),
    oids: &[u32],
    name: &str,
) -> Vec<Arc<super::FuncDef>> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    // (By the whole definition and the constructors' oids: a type's oid and names repeat
    // in other databases of the process, of other engines or stores, as other types.)
    type Key = (String, String, String);
    static CACHE: Mutex<Option<HashMap<Key, Registry>>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let key = (format!("{:?} {oids:?}", **rt), ctors.0.to_string(), ctors.1.to_string());
    let reg = cache
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert_with(|| {
            let mut r = Registry {
                next_oid: rt.oid.wrapping_mul(8),
                ..Default::default()
            };
            constructors(&mut r, rt, ctors, oids);
            r
        });
    reg.funcs.get(name).cloned().unwrap_or_default()
}

pub fn register(r: &mut Registry) {
    use T::{AnyElement as E, AnyMultirange as M, AnyRange as R, Bool};
    for rt in rg::builtins() {
        constructors(r, rt, (&rt.name, &rt.multi_name), &[]);
        let st = rt.subtype.clone();
        let range = T::Range(rt.clone());
        if let Some(cn) = &rt.canonical {
            let cn: &'static str = Box::leak(cn.to_string().into_boxed_str());
            r.func(cn, vec![range.clone()], range.clone(), canonical);
        }
        if let Some(sd) = &rt.subdiff {
            let sd: &'static str = Box::leak(sd.to_string().into_boxed_str());
            r.func(sd, vec![st.clone(), st.clone()], T::Float8, subdiff);
        }
    }
    for (n, f) in [("lower", lower as super::FnImpl), ("upper", upper)] {
        r.func(n, vec![R], E, f);
        r.func(n, vec![M], E, f);
    }
    for (n, f) in [
        ("isempty", isempty as super::FnImpl),
        ("lower_inc", lower_inc),
        ("upper_inc", upper_inc),
        ("lower_inf", lower_inf),
        ("upper_inf", upper_inf),
    ] {
        r.func(n, vec![R], Bool, f);
        r.func(n, vec![M], Bool, f);
    }
    r.func("range_merge", vec![R, R], R, range_merge);
    r.func("range_merge", vec![M], R, mr_merge);
    r.func("multirange", vec![R], M, multi_of_range);
    r.srf_ret("unnest", vec![M], Ret::Fixed(R), mr_unnest)
        .out_name("unnest");
    r.func("range_cmp", vec![R, R], T::Int4, range_cmp);
    r.func("multirange_cmp", vec![M, M], T::Int4, range_cmp);
    // hash_range / hash_multirange: with the other hash support functions (btcmp.rs).
    for (name, f) in [
        ("range_eq", range_eq as super::FnImpl),
        ("range_ne", range_ne),
        ("range_lt", range_lt),
        ("range_le", range_le),
        ("range_gt", range_gt),
        ("range_ge", range_ge),
        ("range_contains", range_contains),
        ("range_contained_by", range_contained_by),
        ("range_overlaps", range_overlaps),
        ("range_before", range_before),
        ("range_after", range_after),
        ("range_overleft", range_overleft),
        ("range_overright", range_overright),
        ("range_adjacent", range_adjacent),
    ] {
        r.func(name, vec![R, R], Bool, f);
    }
    r.func("range_union", vec![R, R], R, range_union);
    r.func("range_intersect", vec![R, R], R, range_intersect);
    r.func("range_minus", vec![R, R], R, range_minus);
    r.func("range_contains_elem", vec![R, E], Bool, range_contains_elem);
    r.func(
        "elem_contained_by_range",
        vec![E, R],
        Bool,
        elem_contained_by_range,
    );
    for (op, f) in [
        ("=", range_eq as super::FnImpl),
        ("<>", range_ne),
        ("<", range_lt),
        ("<=", range_le),
        (">", range_gt),
        (">=", range_ge),
    ] {
        r.binop(op, R, R, Bool, f);
        r.binop(op, M, M, Bool, f);
    }
    r.binop("@>", R, R, Bool, range_contains);
    r.binop("@>", R, E, Bool, range_contains_elem);
    r.binop("<@", R, R, Bool, range_contained_by);
    r.binop("<@", E, R, Bool, elem_contained_by_range);
    r.binop("&&", R, R, Bool, range_overlaps);
    r.binop("<<", R, R, Bool, range_before);
    r.binop(">>", R, R, Bool, range_after);
    r.binop("&<", R, R, Bool, range_overleft);
    r.binop("&>", R, R, Bool, range_overright);
    r.binop("-|-", R, R, Bool, range_adjacent);
    r.binop("+", R, R, R, range_union);
    r.binop("*", R, R, R, range_intersect);
    r.binop("-", R, R, R, range_minus);
    // Multiranges, alone and mixed with ranges.
    r.binop("@>", M, E, Bool, mr_contains_elem);
    r.binop("<@", E, M, Bool, elem_contained_by_mr);
    for (l, rr) in [(M, M), (M, R), (R, M)] {
        r.binop("@>", l.clone(), rr.clone(), Bool, mr_contains);
        r.binop("<@", l.clone(), rr.clone(), Bool, mr_contained_by);
        r.binop("&&", l.clone(), rr.clone(), Bool, mr_overlaps);
        r.binop("<<", l.clone(), rr.clone(), Bool, mr_before);
        r.binop(">>", l.clone(), rr.clone(), Bool, mr_after);
        r.binop("&<", l.clone(), rr.clone(), Bool, mr_overleft);
        r.binop("&>", l.clone(), rr.clone(), Bool, mr_overright);
        r.binop("-|-", l, rr, Bool, mr_adjacent);
    }
    r.binop("+", M, M, M, mr_union);
    r.binop("-", M, M, M, mr_minus);
    r.binop("*", M, M, M, mr_intersect);
}
