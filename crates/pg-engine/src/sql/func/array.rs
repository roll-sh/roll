//! Array functions and operators, and set-returning generators (unnest, generate_series,
//! generate_subscripts).

use super::{FnCtx, Registry, Ret, SrfIter, int_arg, text, text_arg};
use crate::error::{Error, Result, code};
use crate::types::array::ArrayValue;
use crate::types::datetime;
use crate::types::numeric::Numeric;
use crate::types::text::format_value;
use crate::types::value::Interval;
use crate::types::{DataType as T, Value};
use crate::{bail, err};
use std::sync::Arc;

fn arr(v: &Value) -> Option<&ArrayValue> {
    v.as_array()
}

fn mk(elem: T, items: Vec<Value>) -> Value {
    Value::Array(Arc::new(ArrayValue::from_vec(elem, items)))
}

fn elem_of(c: &FnCtx) -> T {
    c.ret.elem().cloned().unwrap_or(T::Text)
}

fn array_append(a: &[Value], c: &FnCtx) -> Result<Value> {
    let elem = elem_of(c);
    match &a[0] {
        Value::Null => Ok(mk(elem, vec![a[1].clone()])),
        Value::Array(x) => {
            if x.ndims() > 1 {
                bail!(
                    DATA_EXCEPTION,
                    "argument must be empty or one-dimensional array"
                );
            }
            let mut out = (**x).clone();
            out.items.push(a[1].clone());
            out.dims = vec![(out.items.len(), x.lower(0).unwrap_or(1))];
            Ok(Value::Array(Arc::new(out)))
        }
        _ => Ok(Value::Null),
    }
}

fn array_prepend(a: &[Value], c: &FnCtx) -> Result<Value> {
    let elem = elem_of(c);
    match &a[1] {
        Value::Null => Ok(mk(elem, vec![a[0].clone()])),
        Value::Array(x) => {
            if x.ndims() > 1 {
                bail!(
                    DATA_EXCEPTION,
                    "argument must be empty or one-dimensional array"
                );
            }
            let mut items = vec![a[0].clone()];
            items.extend(x.items.iter().cloned());
            // The result keeps the input's lower bound.
            let lb = x.lower(0).unwrap_or(1);
            Ok(Value::Array(Arc::new(ArrayValue {
                elem: x.elem.clone(),
                dims: vec![(items.len(), lb)],
                items,
            })))
        }
        _ => Ok(Value::Null),
    }
}

fn array_cat(a: &[Value], _: &FnCtx) -> Result<Value> {
    match (&a[0], &a[1]) {
        (Value::Null, Value::Null) => Ok(Value::Null),
        (Value::Null, v) | (v, Value::Null) => Ok(v.clone()),
        (Value::Array(x), Value::Array(y)) => {
            if x.is_empty() {
                return Ok(a[1].clone());
            }
            if y.is_empty() {
                return Ok(a[0].clone());
            }
            let (nx, ny) = (x.ndims(), y.ndims());
            if nx == ny {
                if x.dims[1..]
                    .iter()
                    .map(|d| d.0)
                    .ne(y.dims[1..].iter().map(|d| d.0))
                {
                    return Err(Error::new(code::ARRAY_SUBSCRIPT_ERROR, "cannot concatenate incompatible arrays")
                        .detail("Arrays with differing element dimensions are not compatible for concatenation."));
                }
                let mut items = x.items.clone();
                items.extend(y.items.iter().cloned());
                let mut dims = x.dims.clone();
                dims[0].0 += y.dims[0].0;
                Ok(Value::Array(Arc::new(ArrayValue {
                    elem: x.elem.clone(),
                    dims,
                    items,
                })))
            } else if nx + 1 == ny {
                // Prepend an element (sub-array) to a higher-dimensional array.
                if x.dims
                    .iter()
                    .map(|d| d.0)
                    .ne(y.dims[1..].iter().map(|d| d.0))
                {
                    return Err(Error::new(
                        code::ARRAY_SUBSCRIPT_ERROR,
                        "cannot concatenate incompatible arrays",
                    )
                    .detail(
                        "Arrays with differing dimensions are not compatible for concatenation.",
                    ));
                }
                let mut items = x.items.clone();
                items.extend(y.items.iter().cloned());
                // (the outer array's bounds, one more element)
                let mut dims = y.dims.clone();
                dims[0].0 += 1;
                Ok(Value::Array(Arc::new(ArrayValue {
                    elem: y.elem.clone(),
                    dims,
                    items,
                })))
            } else if nx == ny + 1 {
                if y.dims
                    .iter()
                    .map(|d| d.0)
                    .ne(x.dims[1..].iter().map(|d| d.0))
                {
                    return Err(Error::new(
                        code::ARRAY_SUBSCRIPT_ERROR,
                        "cannot concatenate incompatible arrays",
                    )
                    .detail(
                        "Arrays with differing dimensions are not compatible for concatenation.",
                    ));
                }
                let mut items = x.items.clone();
                items.extend(y.items.iter().cloned());
                let mut dims = x.dims.clone();
                dims[0].0 += 1;
                Ok(Value::Array(Arc::new(ArrayValue {
                    elem: x.elem.clone(),
                    dims,
                    items,
                })))
            } else {
                Err(Error::new(
                    code::ARRAY_SUBSCRIPT_ERROR,
                    "cannot concatenate incompatible arrays",
                )
                .detail(format!(
                    "Arrays of {nx} and {ny} dimensions are not compatible for concatenation."
                )))
            }
        }
        _ => Ok(Value::Null),
    }
}

fn array_length(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    let d = int_arg(&a[1]);
    Ok(match x.dims.get((d - 1).max(0) as usize) {
        Some((len, _)) if d >= 1 => Value::Int4(*len as i32),
        _ => Value::Null,
    })
}
fn array_lower(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    let d = int_arg(&a[1]);
    Ok(if d >= 1 {
        x.lower(d as usize - 1)
            .map(Value::Int4)
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    })
}
fn array_upper(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    let d = int_arg(&a[1]);
    Ok(if d >= 1 {
        x.upper(d as usize - 1)
            .map(Value::Int4)
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    })
}
fn array_ndims(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    Ok(if x.ndims() == 0 {
        Value::Null
    } else {
        Value::Int4(x.ndims() as i32)
    })
}
fn array_dims(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    if x.ndims() == 0 {
        // An empty int2vector / oidvector still has its one dimension, from 0.
        let mut e = c.args.first();
        while let Some(crate::sql::expr::Expr::Cast(inner, ..) | crate::sql::expr::Expr::Relabel(inner, _)) = e {
            if matches!(inner.ty(), T::Int2Vector | T::OidVector) {
                return Ok(text("[0:-1]"));
            }
            e = Some(inner);
        }
        return Ok(Value::Null);
    }
    Ok(text(
        x.dims
            .iter()
            .map(|(len, lb)| format!("[{}:{}]", lb, *lb + *len as i32 - 1))
            .collect::<String>(),
    ))
}
fn cardinality(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(arr(&a[0])
        .map(|x| Value::Int4(x.items.len() as i32))
        .unwrap_or(Value::Null))
}

fn eq(x: &Value, y: &Value) -> bool {
    // NULL-aware "IS NOT DISTINCT FROM" for array searching.
    match (x.is_null(), y.is_null()) {
        (true, true) => true,
        (false, false) => x.cmp_total(y) == std::cmp::Ordering::Equal,
        _ => false,
    }
}

fn one_dim(x: &ArrayValue) -> Result<()> {
    if x.ndims() > 1 {
        bail!(
            FEATURE_NOT_SUPPORTED,
            "searching for elements in multidimensional arrays is not supported"
        );
    }
    Ok(())
}

fn array_position(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    one_dim(x)?;
    let lb = x.lower(0).unwrap_or(1) as i64;
    let start = a.get(2).map(|v| int_arg(v) - lb).unwrap_or(0).max(0) as usize;
    Ok(x.items
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, v)| eq(v, &a[1]))
        .map(|(i, _)| Value::Int4((i as i64 + lb) as i32))
        .unwrap_or(Value::Null))
}
fn array_positions(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    one_dim(x)?;
    let lb = x.lower(0).unwrap_or(1) as i64;
    let items = x
        .items
        .iter()
        .enumerate()
        .filter(|(_, v)| eq(v, &a[1]))
        .map(|(i, _)| Value::Int4((i as i64 + lb) as i32))
        .collect();
    Ok(mk(T::Int4, items))
}
fn array_remove(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    if x.ndims() > 1 {
        bail!(
            FEATURE_NOT_SUPPORTED,
            "removing elements from multidimensional arrays is not supported"
        );
    }
    let items = x.items.iter().filter(|v| !eq(v, &a[1])).cloned().collect();
    Ok(mk(x.elem.clone(), items))
}
fn array_replace(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    let mut out = x.clone();
    for v in out.items.iter_mut() {
        if eq(v, &a[1]) {
            *v = a[2].clone();
        }
    }
    Ok(Value::Array(Arc::new(out)))
}
fn array_to_string(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    if a[1].is_null() {
        return Ok(Value::Null);
    }
    let sep = text_arg(&a[1]);
    let null_str = a.get(2).and_then(|v| v.as_str());
    let parts: Vec<String> = x
        .items
        .iter()
        .filter_map(|v| match v {
            Value::Null => null_str.map(|s| s.to_string()),
            v => Some(format_value(v, &x.elem, c.exec.fmt())),
        })
        .collect();
    Ok(text(parts.join(sep)))
}
fn array_fill(a: &[Value], c: &FnCtx) -> Result<Value> {
    // (array_fill / array_fill_with_lower_bounds, then array_fill_internal)
    let with_lbs = a.len() > 2;
    if a[1].is_null() || (with_lbs && a[2].is_null()) {
        bail!(
            NULL_VALUE_NOT_ALLOWED,
            "dimension array or low bound array cannot be null"
        );
    }
    let one_dim = |x: &ArrayValue| -> Result<()> {
        if x.dims.len() > 1 {
            return Err(
                err!(ARRAY_SUBSCRIPT_ERROR, "wrong number of array subscripts")
                    .detail("Dimension array must be one dimensional."),
            );
        }
        if x.items.iter().any(Value::is_null) {
            bail!(NULL_VALUE_NOT_ALLOWED, "dimension values cannot be null");
        }
        Ok(())
    };
    let Some(dims) = arr(&a[1]) else {
        return Ok(Value::Null);
    };
    one_dim(dims)?;
    let ndims = dims.items.len();
    if ndims > 6 {
        bail!(
            PROGRAM_LIMIT_EXCEEDED,
            "number of array dimensions ({ndims}) exceeds the maximum allowed (6)"
        );
    }
    let lbs: Vec<i32> = if with_lbs {
        let Some(l) = arr(&a[2]) else {
            return Ok(Value::Null);
        };
        one_dim(l)?;
        if l.items.len() != ndims {
            return Err(
                err!(ARRAY_SUBSCRIPT_ERROR, "wrong number of array subscripts")
                    .detail("Low bound array has different size than dimensions array."),
            );
        }
        l.items.iter().map(|v| v.as_i64().unwrap_or(1) as i32).collect()
    } else {
        vec![1; ndims]
    };
    let lens: Vec<usize> = dims
        .items
        .iter()
        .map(|v| v.as_i64().unwrap_or(0).max(0) as usize)
        .collect();
    let total: usize = if lens.is_empty() {
        0
    } else {
        lens.iter().product()
    };
    let elem = elem_of(c);
    if total == 0 {
        return Ok(Value::Array(Arc::new(ArrayValue::empty(elem))));
    }
    Ok(Value::Array(Arc::new(ArrayValue {
        elem,
        dims: lens.into_iter().zip(lbs).collect(),
        items: vec![a[0].clone(); total],
    })))
}
/// width_bucket(operand, thresholds): how many of the (sorted) thresholds are <= operand.
fn width_bucket_array(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(t) = arr(&a[1]) else {
        return Ok(Value::Null);
    };
    if t.dims.len() > 1 {
        bail!(
            ARRAY_SUBSCRIPT_ERROR,
            "thresholds must be one-dimensional array"
        );
    }
    if t.items.iter().any(Value::is_null) {
        bail!(
            NULL_VALUE_NOT_ALLOWED,
            "thresholds array must not contain NULLs"
        );
    }
    let cmp = |x: &Value, y: &Value| match c.coll {
        Some(coll) => crate::types::collation::cmp_values(coll, x, y),
        None => x.cmp_total(y),
    };
    let (mut left, mut right) = (0usize, t.items.len());
    while left < right {
        let mid = (left + right) / 2;
        if cmp(&a[0], &t.items[mid]) == std::cmp::Ordering::Less {
            right = mid;
        } else {
            left = mid + 1;
        }
    }
    Ok(Value::Int4(left as i32))
}
fn trim_array(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    let n = int_arg(&a[1]);
    let len = x.dims.first().map(|d| d.0).unwrap_or(0) as i64;
    if n < 0 || n > len {
        bail!(
            ARRAY_SUBSCRIPT_ERROR,
            "number of elements to trim must be between 0 and {len}"
        );
    }
    let keep = (len - n) as usize;
    let stride = x.stride();
    let items = x.items[..keep * stride].to_vec();
    if keep == 0 {
        return Ok(Value::Array(Arc::new(ArrayValue::empty(x.elem.clone()))));
    }
    // (a slice: every lower bound becomes 1)
    let mut dims: Vec<(usize, i32)> = x.dims.iter().map(|d| (d.0, 1)).collect();
    dims[0].0 = keep;
    Ok(Value::Array(Arc::new(ArrayValue {
        elem: x.elem.clone(),
        dims,
        items,
    })))
}
fn array_reverse(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    let stride = x.stride().max(1);
    let mut out = x.clone();
    out.items = x.items.chunks(stride).rev().flatten().cloned().collect();
    Ok(Value::Array(Arc::new(out)))
}
fn array_sort(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    let desc = a.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
    let nulls_first = a.get(2).and_then(|v| v.as_bool()).unwrap_or(desc);
    // Nothing to sort: no comparison needed.
    if x.dims.first().is_none_or(|d| d.0 < 2) {
        return Ok(a[0].clone());
    }
    if matches!(x.elem, T::Xid | T::Cid | T::Json | T::Point) {
        bail!(
            FEATURE_NOT_SUPPORTED,
            "could not identify a comparison function for type {}",
            x.elem.display()
        );
    }
    let stride = x.stride().max(1);
    let mut chunks: Vec<Vec<Value>> = x.items.chunks(stride).map(|c| c.to_vec()).collect();
    // A multidimensional array sorts its sub-arrays, compared as arrays (array_lt).
    let multi = x.dims.len() > 1;
    let cmp_items = |p: &[Value], q: &[Value]| -> std::cmp::Ordering {
        for (a, b) in p.iter().zip(q) {
            let o = match (a.is_null(), b.is_null()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => match c.coll {
                    Some(coll) => crate::types::collation::cmp_values(coll, a, b),
                    None => a.cmp_total(b),
                },
            };
            if o != std::cmp::Ordering::Equal {
                return o;
            }
        }
        p.len().cmp(&q.len())
    };
    if multi {
        chunks.sort_by(|p, q| {
            let o = cmp_items(p, q);
            if desc { o.reverse() } else { o }
        });
        let mut out = x.clone();
        out.items = chunks.into_iter().flatten().collect();
        return Ok(Value::Array(Arc::new(out)));
    }
    chunks.sort_by(|p, q| {
        let (a, b) = (&p[0], &q[0]);
        match (a.is_null(), b.is_null()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => {
                if nulls_first {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            }
            (false, true) => {
                if nulls_first {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                }
            }
            _ => {
                let o = match c.coll {
                    Some(coll) => crate::types::collation::cmp_values(coll, a, b),
                    None => a.cmp_total(b),
                };
                if desc { o.reverse() } else { o }
            }
        }
    });
    let mut out = x.clone();
    out.items = chunks.into_iter().flatten().collect();
    Ok(Value::Array(Arc::new(out)))
}
fn array_shuffle(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    let stride = x.stride().max(1);
    let mut chunks: Vec<Vec<Value>> = x.items.chunks(stride).map(|c| c.to_vec()).collect();
    for i in (1..chunks.len()).rev() {
        let j = (c.exec.random() * (i + 1) as f64) as usize;
        chunks.swap(i, j.min(i));
    }
    let mut out = x.clone();
    out.items = chunks.into_iter().flatten().collect();
    Ok(Value::Array(Arc::new(out)))
}
fn array_sample(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Value::Null);
    };
    let n = int_arg(&a[1]);
    let len = x.dims.first().map(|d| d.0).unwrap_or(0) as i64;
    if n < 0 || n > len {
        bail!(
            INVALID_PARAMETER_VALUE,
            "sample size must be between 0 and {len}"
        );
    }
    let Value::Array(sh) = array_shuffle(&a[..1], c)? else {
        return Ok(Value::Null);
    };
    // (array_shuffle_n: n items; the first dimension's lower bound becomes 1)
    let keep = n as usize;
    if keep == 0 {
        return Ok(Value::Array(Arc::new(ArrayValue::empty(sh.elem.clone()))));
    }
    let stride = sh.stride();
    let mut dims = sh.dims.clone();
    dims[0] = (keep, 1);
    Ok(Value::Array(Arc::new(ArrayValue {
        elem: sh.elem.clone(),
        dims,
        items: sh.items[..keep * stride].to_vec(),
    })))
}

fn contains_all(x: &ArrayValue, y: &ArrayValue) -> bool {
    y.items.iter().all(|v| {
        !v.is_null()
            && x.items
                .iter()
                .any(|w| !w.is_null() && w.cmp_total(v) == std::cmp::Ordering::Equal)
    })
}
fn op_contains(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (Some(x), Some(y)) = (arr(&a[0]), arr(&a[1])) else {
        return Ok(Value::Null);
    };
    Ok(Value::Bool(contains_all(x, y)))
}
fn op_contained(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (Some(x), Some(y)) = (arr(&a[0]), arr(&a[1])) else {
        return Ok(Value::Null);
    };
    Ok(Value::Bool(contains_all(y, x)))
}
fn op_overlap(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (Some(x), Some(y)) = (arr(&a[0]), arr(&a[1])) else {
        return Ok(Value::Null);
    };
    Ok(Value::Bool(x.items.iter().any(|v| {
        !v.is_null()
            && y.items
                .iter()
                .any(|w| !w.is_null() && w.cmp_total(v) == std::cmp::Ordering::Equal)
    })))
}

// ── set-returning ──

fn unnest(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    if a.len() == 1 {
        let items = arr(&a[0]).map(|x| x.items.clone()).unwrap_or_default();
        return Ok(Box::new(items.into_iter().map(|v| Ok(vec![v]))));
    }
    // unnest(a, b, …): zip, padding with NULLs.
    let lists: Vec<Vec<Value>> = a
        .iter()
        .map(|v| arr(v).map(|x| x.items.clone()).unwrap_or_default())
        .collect();
    let n = lists.iter().map(|l| l.len()).max().unwrap_or(0);
    Ok(Box::new((0..n).map(move |i| {
        Ok(lists
            .iter()
            .map(|l| l.get(i).cloned().unwrap_or(Value::Null))
            .collect())
    })))
}

fn generate_subscripts(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    let Some(x) = arr(&a[0]) else {
        return Ok(Box::new(std::iter::empty()));
    };
    let d = int_arg(&a[1]);
    let rev = a.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
    let Some((len, lb)) = (if d >= 1 {
        x.dims.get(d as usize - 1).copied()
    } else {
        None
    }) else {
        return Ok(Box::new(std::iter::empty()));
    };
    let range: Vec<i32> = (lb..lb + len as i32).collect();
    let it: Box<dyn Iterator<Item = i32> + Send> = if rev {
        Box::new(range.into_iter().rev())
    } else {
        Box::new(range.into_iter())
    };
    Ok(Box::new(it.map(|i| Ok(vec![Value::Int4(i)]))))
}

fn series_int(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let start = int_arg(&a[0]);
    let stop = int_arg(&a[1]);
    let step = a.get(2).map(int_arg).unwrap_or(1);
    if step == 0 {
        bail!(INVALID_PARAMETER_VALUE, "step size cannot equal zero");
    }
    let is4 = matches!(c.ret, T::Int4);
    let mut cur = Some(start);
    Ok(Box::new(std::iter::from_fn(move || {
        let v = cur?;
        if (step > 0 && v > stop) || (step < 0 && v < stop) {
            return None;
        }
        cur = v.checked_add(step);
        if is4 && cur.is_some_and(|n| n > i32::MAX as i64 || n < i32::MIN as i64) {
            cur = None;
        }
        Some(Ok(vec![if is4 {
            Value::Int4(v as i32)
        } else {
            Value::Int8(v)
        }]))
    })))
}

fn num(v: &Value) -> Numeric {
    match v {
        Value::Numeric(n) => (**n).clone(),
        v => Numeric::from_i64(v.as_i64().unwrap_or(0)),
    }
}

fn series_numeric(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    let start = num(&a[0]);
    let stop = num(&a[1]);
    let step = a.get(2).map(num).unwrap_or(Numeric::from_i64(1));
    for (n, what) in [
        (&start, "start value"),
        (&stop, "stop value"),
        (&step, "step size"),
    ] {
        if n.is_nan() {
            bail!(INVALID_PARAMETER_VALUE, "{what} cannot be NaN");
        }
        if !n.is_finite() {
            bail!(INVALID_PARAMETER_VALUE, "{what} cannot be infinity");
        }
    }
    if step.is_zero() {
        bail!(INVALID_PARAMETER_VALUE, "step size cannot equal zero");
    }
    let up = !step.is_negative();
    let mut cur = start;
    Ok(Box::new(std::iter::from_fn(move || {
        if (up && cur > stop) || (!up && cur < stop) {
            return None;
        }
        let v = cur.clone();
        cur = cur.add(&step);
        Some(Ok(vec![Value::numeric(v)]))
    })))
}

fn series_ts(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let tz = matches!(a[0], Value::Timestamptz(_));
    let tzv = match a.get(3) {
        // lookup_timezone (DecodeTimezoneNameToTz).
        Some(z) => super::datetime::zone(text_arg(z), c)?,
        None => c.exec.fmt().tz.clone(),
    };
    let start = match &a[0] {
        Value::Timestamp(t) | Value::Timestamptz(t) => *t,
        _ => 0,
    };
    let stop = match &a[1] {
        Value::Timestamp(t) | Value::Timestamptz(t) => *t,
        _ => 0,
    };
    let Value::Interval(step) = a[2] else {
        return Ok(Box::new(std::iter::empty()));
    };
    if step.is_infinite() {
        bail!(INVALID_PARAMETER_VALUE, "step size cannot be infinite");
    }
    let zero = Interval::new(0, 0, 0);
    if step == zero {
        bail!(INVALID_PARAMETER_VALUE, "step size cannot equal zero");
    }
    let up = step > zero;
    let mut cur = Some(start);
    Ok(Box::new(std::iter::from_fn(move || {
        let v = cur?;
        if (up && v > stop) || (!up && v < stop) {
            return None;
        }
        let next = if tz {
            datetime::tstz_add_interval(v, &step, &tzv)
        } else {
            datetime::ts_add_interval(v, &step)
        };
        cur = next.ok();
        Some(Ok(vec![if tz {
            Value::Timestamptz(v)
        } else {
            Value::Timestamp(v)
        }]))
    })))
}

fn series_date(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let conv = |v: &Value| match v {
        Value::Date(d) => Value::Timestamp(*d as i64 * crate::types::value::USECS_PER_DAY),
        v => v.clone(),
    };
    let b = vec![conv(&a[0]), conv(&a[1]), a[2].clone()];
    series_ts(&b, c)
}

pub fn register(r: &mut Registry) {
    use T::*;
    r.func_ret(
        "array_append",
        vec![AnyCompatibleArray, AnyCompatible],
        Ret::PolyArray,
        array_append,
    )
    .non_strict();
    r.func_ret(
        "array_prepend",
        vec![AnyCompatible, AnyCompatibleArray],
        Ret::PolyArray,
        array_prepend,
    )
    .non_strict();
    r.func_ret(
        "array_cat",
        vec![AnyCompatibleArray, AnyCompatibleArray],
        Ret::PolyArray,
        array_cat,
    )
    .non_strict();
    r.binop_ret(
        "||",
        AnyCompatibleArray,
        AnyCompatibleArray,
        Ret::PolyArray,
        array_cat,
    );
    r.binop_ret(
        "||",
        AnyCompatibleArray,
        AnyCompatible,
        Ret::PolyArray,
        array_append,
    );
    r.binop_ret(
        "||",
        AnyCompatible,
        AnyCompatibleArray,
        Ret::PolyArray,
        array_prepend,
    );
    // array_cat / array_append / array_prepend are not strict: NULL || array = array.
    for d in r.binops.get_mut("||").into_iter().flatten() {
        if d.args.iter().any(|t| matches!(t, AnyCompatibleArray))
            && let Some(d) = Arc::get_mut(d)
        {
            d.strict = false;
        }
    }
    r.func("array_length", vec![AnyArray, Int4], Int4, array_length);
    r.func("array_lower", vec![AnyArray, Int4], Int4, array_lower);
    r.func("array_upper", vec![AnyArray, Int4], Int4, array_upper);
    r.func("array_ndims", vec![AnyArray], Int4, array_ndims);
    r.func("array_dims", vec![AnyArray], Text, array_dims);
    r.func("cardinality", vec![AnyArray], Int4, cardinality);
    r.func(
        "array_position",
        vec![AnyCompatibleArray, AnyCompatible],
        Int4,
        array_position,
    )
    .non_strict();
    r.func(
        "array_position",
        vec![AnyCompatibleArray, AnyCompatible, Int4],
        Int4,
        array_position,
    )
    .non_strict();
    r.func(
        "array_positions",
        vec![AnyCompatibleArray, AnyCompatible],
        T::array_of(Int4),
        array_positions,
    )
    .non_strict();
    r.func_ret(
        "array_remove",
        vec![AnyCompatibleArray, AnyCompatible],
        Ret::PolyArray,
        array_remove,
    )
    .non_strict();
    r.func_ret(
        "array_replace",
        vec![AnyCompatibleArray, AnyCompatible, AnyCompatible],
        Ret::PolyArray,
        array_replace,
    )
    .non_strict();
    r.func(
        "array_to_string",
        vec![AnyArray, Text],
        Text,
        array_to_string,
    )
    .stable();
    // The input functions of the polymorphic pseudo-types, callable as functions.
    r.func("array_in", vec![T::Cstring, T::Oid, Int4], AnyArray, array_in)
        .stable();
    r.func("anyrange_in", vec![T::Cstring, T::Oid, Int4], T::AnyRange, anyrange_in)
        .stable();
    r.func(
        "array_to_string",
        vec![AnyArray, Text, Text],
        Text,
        array_to_string,
    )
    .non_strict()
    .stable();
    r.func_ret(
        "array_fill",
        vec![AnyElement, T::array_of(Int4)],
        Ret::PolyArray,
        array_fill,
    )
    .non_strict();
    r.func_ret(
        "array_fill",
        vec![AnyElement, T::array_of(Int4), T::array_of(Int4)],
        Ret::PolyArray,
        array_fill,
    )
    .non_strict();
    r.func_ret("trim_array", vec![AnyArray, Int4], Ret::Arg(0), trim_array);
    r.func(
        "width_bucket",
        vec![AnyCompatible, AnyCompatibleArray],
        Int4,
        width_bucket_array,
    );
    r.func_ret("array_reverse", vec![AnyArray], Ret::Arg(0), array_reverse);
    r.func_ret("array_sort", vec![AnyArray], Ret::Arg(0), array_sort);
    r.func_ret("array_sort", vec![AnyArray, Bool], Ret::Arg(0), array_sort);
    r.func_ret(
        "array_sort",
        vec![AnyArray, Bool, Bool],
        Ret::Arg(0),
        array_sort,
    );
    r.func_ret("array_shuffle", vec![AnyArray], Ret::Arg(0), array_shuffle)
        .volatile();
    r.func_ret(
        "array_sample",
        vec![AnyArray, Int4],
        Ret::Arg(0),
        array_sample,
    )
    .volatile();
    r.binop("@>", AnyArray, AnyArray, Bool, op_contains);
    r.binop("<@", AnyArray, AnyArray, Bool, op_contained);
    r.binop("&&", AnyArray, AnyArray, Bool, op_overlap);
    r.func("arraycontains", vec![AnyArray, AnyArray], Bool, op_contains);
    r.func(
        "arraycontained",
        vec![AnyArray, AnyArray],
        Bool,
        op_contained,
    );
    r.func("arrayoverlap", vec![AnyArray, AnyArray], Bool, op_overlap);
    r.srf_ret("unnest", vec![AnyArray], Ret::Poly, unnest)
        .non_strict();
    r.srf(
        "generate_subscripts",
        vec![AnyArray, Int4],
        Int4,
        generate_subscripts,
    );
    r.srf(
        "generate_subscripts",
        vec![AnyArray, Int4, Bool],
        Int4,
        generate_subscripts,
    );
    r.srf("generate_series", vec![Int4, Int4], Int4, series_int);
    r.srf("generate_series", vec![Int4, Int4, Int4], Int4, series_int);
    r.srf("generate_series", vec![Int8, Int8], Int8, series_int);
    r.srf("generate_series", vec![Int8, Int8, Int8], Int8, series_int);
    r.srf(
        "generate_series",
        vec![Numeric(-1), Numeric(-1)],
        Numeric(-1),
        series_numeric,
    );
    r.srf(
        "generate_series",
        vec![Numeric(-1), Numeric(-1), Numeric(-1)],
        Numeric(-1),
        series_numeric,
    );
    r.srf(
        "generate_series",
        vec![Timestamp(-1), Timestamp(-1), Interval(-1)],
        Timestamp(-1),
        series_ts,
    );
    r.srf(
        "generate_series",
        vec![Timestamptz(-1), Timestamptz(-1), Interval(-1)],
        Timestamptz(-1),
        series_ts,
    )
    .stable();
    r.srf(
        "generate_series",
        vec![Timestamptz(-1), Timestamptz(-1), Interval(-1), Text],
        Timestamptz(-1),
        series_ts,
    );
    let _ = series_date;
}

/// array_in(text, element type, typmod): the text as an array of that element type.
fn array_in(a: &[Value], c: &FnCtx) -> Result<Value> {
    let elem_oid = int_arg(&a[1]) as u32;
    let elem = match T::from_oid(elem_oid) {
        Some(t) => t,
        None => c.exec.catalog().user_data_type(elem_oid).ok_or_else(|| {
            Error::new(
                code::UNDEFINED_OBJECT,
                format!("cache lookup failed for type {elem_oid}"),
            )
        })?,
    };
    crate::types::text::parse_value(text_arg(&a[0]), &T::array_of(elem), c.exec.fmt())
}

/// anyrange_in: no value is of type anyrange.
fn anyrange_in(_: &[Value], _: &FnCtx) -> Result<Value> {
    bail!(FEATURE_NOT_SUPPORTED, "cannot accept a value of type anyrange")
}
