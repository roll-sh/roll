//! hstore's constructors and conversions (hstore_io.c): from text, arrays and records,
//! populate_record, the JSON conversions.

use super::io::{check_key_len, check_val_len};
use super::{Hs, Pair, encode, from_pairs};
use crate::Result;
use crate::error::{Error, code};
use crate::ext::native::Native;
use crate::ext::types::HSTORE;
use crate::sql::func::{FnCtx, text_arg};
use crate::types::array::ArrayValue;
use crate::types::{DataType, Value};
use std::sync::Arc;

/// An hstore argument's bytes.
pub(crate) fn hs_bytes(v: &Value) -> &[u8] {
    match v {
        Value::Ext(_, b) => b,
        _ => &[],
    }
}

pub(crate) fn hs_value(b: Vec<u8>) -> Value {
    Value::Ext(HSTORE, b.into())
}

pub(crate) fn text_bytes(v: &Value) -> Vec<u8> {
    text_arg(v).as_bytes().to_vec()
}

pub(crate) fn bytes_text(b: &[u8]) -> Value {
    Value::Text(String::from_utf8_lossy(b).into())
}

pub(crate) fn subscript_error(msg: &str) -> Error {
    Error::new(code::ARRAY_SUBSCRIPT_ERROR, msg.to_string())
}

/// A text[] argument.
pub(crate) fn array_arg(v: &Value) -> Arc<ArrayValue> {
    match v {
        Value::Array(a) => a.clone(),
        _ => Arc::new(ArrayValue::empty(DataType::Text)),
    }
}

fn null_key() -> Error {
    Error::new(code::NULL_VALUE_NOT_ALLOWED, "null value not allowed for hstore key")
}

/// `hstore_in` called as a function.
fn hstore_in(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(hs_value(super::io::input(text_arg(&a[0]))?))
}

fn hstore_out(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Text(super::io::output(hs_bytes(&a[0])).into()))
}

/// `hstore_send`: the pair count, then each key and value (-1 for NULL) with its length.
fn hstore_send(a: &[Value], _: &FnCtx) -> Result<Value> {
    let h = Hs::new(hs_bytes(&a[0]));
    let mut out = Vec::new();
    out.extend_from_slice(&(h.count() as i32).to_be_bytes());
    for (k, v) in h.pairs() {
        out.extend_from_slice(&(k.len() as i32).to_be_bytes());
        out.extend_from_slice(k);
        match v {
            Some(v) => {
                out.extend_from_slice(&(v.len() as i32).to_be_bytes());
                out.extend_from_slice(v);
            }
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
        }
    }
    Ok(Value::Bytea(out.into()))
}

/// Functions taking `internal` can't be called from SQL.
fn internal_only(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot call a function taking type internal"))
}

/// `hstore_version_diag`: 0 old-format, 1 new (what every value here is), 2 both.
fn version_diag(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(1))
}

/// `hstore_from_text`: one pair (not strict: a NULL value is a NULL value).
fn from_text(a: &[Value], _: &FnCtx) -> Result<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let key = text_bytes(&a[0]);
    check_key_len(key.len())?;
    let val = (!a[1].is_null()).then(|| text_bytes(&a[1]));
    if let Some(v) = &val {
        check_val_len(v.len())?;
    }
    Ok(hs_value(encode([(key.as_slice(), val.as_deref())])))
}

/// `hstore_from_arrays`: keys and values (NULL: all values NULL).
fn from_arrays(a: &[Value], _: &FnCtx) -> Result<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let keys = array_arg(&a[0]);
    if keys.ndims() > 1 {
        return Err(subscript_error("wrong number of array subscripts"));
    }
    let vals = (!a[1].is_null()).then(|| array_arg(&a[1]));
    if let Some(v) = &vals {
        if v.ndims() > 1 {
            return Err(subscript_error("wrong number of array subscripts"));
        }
        if (keys.ndims() > 0 || v.ndims() > 0)
            && (keys.ndims() != v.ndims() || keys.dims[0] != v.dims[0])
        {
            return Err(subscript_error("arrays must have same bounds"));
        }
    }
    let mut pairs = Vec::with_capacity(keys.items.len());
    for (i, k) in keys.items.iter().enumerate() {
        if k.is_null() {
            return Err(null_key());
        }
        let key = text_bytes(k);
        check_key_len(key.len())?;
        let val = match vals.as_ref().map(|v| &v.items[i]) {
            Some(v) if !v.is_null() => {
                let v = text_bytes(v);
                check_val_len(v.len())?;
                Some(v)
            }
            _ => None,
        };
        pairs.push(Pair { key, val, needfree: false });
    }
    Ok(hs_value(from_pairs(pairs)))
}

/// `hstore_from_array`: alternating keys and values, or rows of two.
fn from_array(a: &[Value], _: &FnCtx) -> Result<Value> {
    let arr = array_arg(&a[0]);
    match arr.ndims() {
        0 => return Ok(hs_value(encode([]))),
        1 if !arr.dims[0].0.is_multiple_of(2) => {
            return Err(subscript_error("array must have even number of elements"));
        }
        1 => {}
        2 if arr.dims[1].0 != 2 => return Err(subscript_error("array must have two columns")),
        2 => {}
        _ => return Err(subscript_error("wrong number of array subscripts")),
    }
    let mut pairs = Vec::with_capacity(arr.items.len() / 2);
    for kv in arr.items.chunks(2) {
        if kv[0].is_null() {
            return Err(null_key());
        }
        let key = text_bytes(&kv[0]);
        check_key_len(key.len())?;
        let val = if kv[1].is_null() {
            None
        } else {
            let v = text_bytes(&kv[1]);
            check_val_len(v.len())?;
            Some(v)
        };
        pairs.push(Pair { key, val, needfree: false });
    }
    Ok(hs_value(from_pairs(pairs)))
}

/// A record argument's fields: names, types and values (all NULL for a NULL record).
fn record_fields(v: &Value, ty: &DataType) -> Result<Vec<(String, DataType, Value)>> {
    let DataType::Row(rt) = ty else {
        return Err(Error::new(
            code::WRONG_OBJECT_TYPE,
            format!("type {} is not composite", ty.display()),
        ));
    };
    let vals: Vec<Value> = match v {
        Value::Record(r) => (**r).clone(),
        _ => vec![Value::Null; rt.fields.len()],
    };
    Ok(rt
        .fields
        .iter()
        .zip(vals)
        .filter(|((n, _), _)| !n.starts_with("........pg.dropped."))
        .map(|((n, t), v)| (n.clone(), t.clone(), v))
        .collect())
}

/// `hstore_from_record`: each column's name and output text.
fn from_record(a: &[Value], c: &FnCtx) -> Result<Value> {
    let ty = c.arg_type(0);
    let fields = record_fields(&a[0], &ty)?;
    let mut pairs = Vec::with_capacity(fields.len());
    for (name, t, v) in fields {
        let key = name.into_bytes();
        check_key_len(key.len())?;
        let val = if v.is_null() {
            None
        } else {
            let s = crate::types::text::format_value(&v, &t, c.exec.fmt());
            check_val_len(s.len())?;
            Some(s.into_bytes())
        };
        pairs.push(Pair { key, val, needfree: false });
    }
    Ok(hs_value(from_pairs(pairs)))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(Native::scalar(match sym {
        "hstore_in" => hstore_in,
        "hstore_out" => hstore_out,
        "hstore_send" => hstore_send,
        "hstore_recv" => internal_only,
        "hstore_version_diag" => version_diag,
        "hstore_from_text" => from_text,
        "hstore_from_arrays" => from_arrays,
        "hstore_from_array" => from_array,
        "hstore_from_record" => from_record,
        "hstore_populate_record" => super::convert::populate_record,
        "hstore_to_json" => super::convert::to_json,
        "hstore_to_json_loose" => super::convert::to_json_loose,
        "hstore_to_jsonb" => super::convert::to_jsonb,
        "hstore_to_jsonb_loose" => super::convert::to_jsonb_loose,
        _ => return None,
    }))
}
