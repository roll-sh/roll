//! hstore's operators and functions (hstore_op.c).

use super::build::{array_arg, bytes_text, hs_bytes, hs_value, text_bytes};
use super::{Hs, Pair, encode, from_pairs, hstore_cmp, key_order};
use crate::Result;
use crate::ext::native::Native;
use crate::sql::func::{FnCtx, SrfIter};
use crate::types::array::ArrayValue;
use crate::types::{DataType, Value};
use std::cmp::Ordering;
use std::sync::Arc;

/// `hstoreArrayToPairs`: a text[]'s non-NULL elements, sorted and unique.
fn array_keys(v: &Value) -> Vec<Vec<u8>> {
    let a = array_arg(v);
    let pairs: Vec<Pair> = a
        .items
        .iter()
        .filter(|k| !k.is_null())
        .map(|k| Pair {
            key: text_bytes(k),
            val: None,
            needfree: false,
        })
        .collect();
    let b = from_pairs(pairs);
    Hs::new(&b).pairs().map(|(k, _)| k.to_vec()).collect()
}

fn text_array(items: Vec<Value>, dims: Vec<(usize, i32)>) -> Value {
    Value::Array(Arc::new(if items.is_empty() {
        ArrayValue::empty(DataType::Text)
    } else {
        ArrayValue {
            elem: DataType::Text,
            dims,
            items,
        }
    }))
}

fn val_text(v: Option<&[u8]>) -> Value {
    v.map_or(Value::Null, bytes_text)
}

fn fetchval(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h = Hs::new(hs_bytes(&a[0]));
    Ok(match h.find(None, &text_bytes(&a[1])) {
        Some(i) => val_text(h.val(i)),
        None => Value::Null,
    })
}

fn exists(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h = Hs::new(hs_bytes(&a[0]));
    Ok(Value::Bool(h.find(None, &text_bytes(&a[1])).is_some()))
}

fn defined(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h = Hs::new(hs_bytes(&a[0]));
    Ok(Value::Bool(h.find(None, &text_bytes(&a[1])).is_some_and(|i| h.val(i).is_some())))
}

fn exists_any(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h = Hs::new(hs_bytes(&a[0]));
    let mut low = 0;
    Ok(Value::Bool(array_keys(&a[1]).iter().any(|k| h.find(Some(&mut low), k).is_some())))
}

fn exists_all(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h = Hs::new(hs_bytes(&a[0]));
    let mut low = 0;
    Ok(Value::Bool(array_keys(&a[1]).iter().all(|k| h.find(Some(&mut low), k).is_some())))
}

/// The pairs of `h` for which `keep` holds.
fn filtered(h: Hs<'_>, mut keep: impl FnMut(&[u8], Option<&[u8]>) -> bool) -> Value {
    hs_value(encode(h.pairs().filter(|(k, v)| keep(k, *v))))
}

fn delete(a: &[Value], _: &FnCtx) -> Result<Value> {
    let key = text_bytes(&a[1]);
    Ok(filtered(Hs::new(hs_bytes(&a[0])), |k, _| k != key.as_slice()))
}

fn delete_array(a: &[Value], _: &FnCtx) -> Result<Value> {
    let keys = array_keys(&a[1]);
    Ok(filtered(Hs::new(hs_bytes(&a[0])), |k, _| {
        keys.binary_search_by(|x| key_order(x, k)).is_err()
    }))
}

/// `hstore_delete_hstore`: pairs whose key and value are both in the second.
fn delete_hstore(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h2 = Hs::new(hs_bytes(&a[1]));
    Ok(filtered(Hs::new(hs_bytes(&a[0])), |k, v| match h2.find(None, k) {
        Some(j) => h2.val(j) != v,
        None => true,
    }))
}

/// `hstore_concat`: the second's pairs win.
fn concat(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (h1, h2) = (Hs::new(hs_bytes(&a[0])), Hs::new(hs_bytes(&a[1])));
    let mut out: Vec<(&[u8], Option<&[u8]>)> = Vec::with_capacity(h1.count() + h2.count());
    let (mut i, mut j) = (0, 0);
    while i < h1.count() || j < h2.count() {
        let d = if i >= h1.count() {
            Ordering::Greater
        } else if j >= h2.count() {
            Ordering::Less
        } else {
            key_order(h1.key(i), h2.key(j))
        };
        if d.is_ge() {
            out.push((h2.key(j), h2.val(j)));
            j += 1;
            if d.is_eq() {
                i += 1;
            }
        } else {
            out.push((h1.key(i), h1.val(i)));
            i += 1;
        }
    }
    Ok(hs_value(encode(out)))
}

/// `hstore_contains`: every pair of the template is in the value.
fn contains_hs(val: &[u8], tmpl: &[u8]) -> bool {
    let (v, t) = (Hs::new(val), Hs::new(tmpl));
    let mut low = 0;
    t.pairs().all(|(k, tv)| match v.find(Some(&mut low), k) {
        Some(i) => v.val(i) == tv,
        None => false,
    })
}

fn contains(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(contains_hs(hs_bytes(&a[0]), hs_bytes(&a[1]))))
}

fn contained(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(contains_hs(hs_bytes(&a[1]), hs_bytes(&a[0]))))
}

/// `hstore_slice_to_array`: the values of the keys, shaped as the key array.
fn slice_to_array(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h = Hs::new(hs_bytes(&a[0]));
    let keys = array_arg(&a[1]);
    let items = keys
        .items
        .iter()
        .map(|k| match k {
            Value::Null => Value::Null,
            k => h.find(None, &text_bytes(k)).map_or(Value::Null, |i| val_text(h.val(i))),
        })
        .collect();
    Ok(text_array(items, keys.dims.clone()))
}

fn slice_to_hstore(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h = Hs::new(hs_bytes(&a[0]));
    let keys = array_keys(&a[1]);
    let mut low = 0;
    let found: Vec<(&[u8], Option<&[u8]>)> = keys
        .iter()
        .filter_map(|k| h.find(Some(&mut low), k).map(|i| (h.key(i), h.val(i))))
        .collect();
    Ok(hs_value(encode(found)))
}

fn akeys(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h = Hs::new(hs_bytes(&a[0]));
    let items: Vec<Value> = h.pairs().map(|(k, _)| bytes_text(k)).collect();
    let n = items.len();
    Ok(text_array(items, vec![(n, 1)]))
}

fn avals(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h = Hs::new(hs_bytes(&a[0]));
    let items: Vec<Value> = h.pairs().map(|(_, v)| val_text(v)).collect();
    let n = items.len();
    Ok(text_array(items, vec![(n, 1)]))
}

/// `hstore_to_array_internal`: keys and values alternating, in one or two dimensions.
fn to_array_n(a: &[Value], ndims: usize) -> Value {
    let h = Hs::new(hs_bytes(&a[0]));
    let items: Vec<Value> = h.pairs().flat_map(|(k, v)| [bytes_text(k), val_text(v)]).collect();
    let n = h.count();
    let dims = if ndims == 1 { vec![(n * 2, 1)] } else { vec![(n, 1), (2, 1)] };
    text_array(items, dims)
}

fn to_array(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(to_array_n(a, 1))
}

fn to_matrix(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(to_array_n(a, 2))
}

fn rows(h: &[u8], f: fn(&[u8], Option<&[u8]>) -> Vec<Value>) -> Result<SrfIter> {
    let rows: Vec<Vec<Value>> = Hs::new(h).pairs().map(|(k, v)| f(k, v)).collect();
    Ok(Box::new(rows.into_iter().map(Ok)))
}

fn skeys(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(hs_bytes(&a[0]), |k, _| vec![bytes_text(k)])
}

fn svals(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(hs_bytes(&a[0]), |_, v| vec![val_text(v)])
}

fn each(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(hs_bytes(&a[0]), |k, v| vec![bytes_text(k), val_text(v)])
}

fn ord(a: &[Value]) -> Ordering {
    hstore_cmp(hs_bytes(&a[0]), hs_bytes(&a[1]))
}

/// `hstore_hash`: hash_any over the value's bytes.
fn hash(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(crate::types::hash::hash_bytes_extended(hs_bytes(&a[0]), 0) as u32 as i32))
}

fn hash_extended(a: &[Value], _: &FnCtx) -> Result<Value> {
    let seed = a[1].as_i64().unwrap_or(0) as u64;
    Ok(Value::Int8(crate::types::hash::hash_bytes_extended(hs_bytes(&a[0]), seed) as i64))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    if let Some(srf) = match sym {
        "hstore_skeys" => Some(skeys as crate::sql::func::SrfImpl),
        "hstore_svals" => Some(svals as crate::sql::func::SrfImpl),
        "hstore_each" => Some(each as crate::sql::func::SrfImpl),
        _ => None,
    } {
        return Some(Native::set(srf));
    }
    Some(Native::scalar(match sym {
        "hstore_fetchval" => fetchval,
        "hstore_slice_to_array" => slice_to_array,
        "hstore_slice_to_hstore" => slice_to_hstore,
        "hstore_exists" => exists,
        "hstore_exists_any" => exists_any,
        "hstore_exists_all" => exists_all,
        "hstore_defined" => defined,
        "hstore_delete" => delete,
        "hstore_delete_array" => delete_array,
        "hstore_delete_hstore" => delete_hstore,
        "hstore_concat" => concat,
        "hstore_contains" => contains,
        "hstore_contained" => contained,
        "hstore_akeys" => akeys,
        "hstore_avals" => avals,
        "hstore_to_array" => to_array,
        "hstore_to_matrix" => to_matrix,
        "hstore_eq" => |a, _| Ok(Value::Bool(ord(a).is_eq())),
        "hstore_ne" => |a, _| Ok(Value::Bool(ord(a).is_ne())),
        "hstore_gt" => |a, _| Ok(Value::Bool(ord(a).is_gt())),
        "hstore_ge" => |a, _| Ok(Value::Bool(ord(a).is_ge())),
        "hstore_lt" => |a, _| Ok(Value::Bool(ord(a).is_lt())),
        "hstore_le" => |a, _| Ok(Value::Bool(ord(a).is_le())),
        "hstore_cmp" => |a, _| Ok(Value::Int4(ord(a) as i32)),
        "hstore_hash" => hash,
        "hstore_hash_extended" => hash_extended,
        _ => return None,
    }))
}
