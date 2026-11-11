//! The bit type's distance functions (`bitvec.c`, `bitutils.c`); `bit` is the engine's
//! own `Value::Bits`.

use super::common::{data_exception, natives};
use super::dist;
use crate::Result;
use crate::types::Value;

natives! {
    "hamming_distance" => hamming_distance,
    "jaccard_distance" => jaccard_distance,
}

/// `CheckDims`.
fn check_dims(a: &Value, b: &Value) -> Result<()> {
    let len = |v: &Value| match v {
        Value::Bits(n, _) => *n,
        _ => 0,
    };
    if len(a) != len(b) {
        return Err(data_exception(format!("different bit lengths {} and {}", len(a), len(b))));
    }
    Ok(())
}

fn hamming_distance(a: &[Value]) -> Result<Value> {
    check_dims(&a[0], &a[1])?;
    Ok(Value::Float8(dist::hamming(&a[0], &a[1])))
}

fn jaccard_distance(a: &[Value]) -> Result<Value> {
    check_dims(&a[0], &a[1])?;
    Ok(Value::Float8(dist::jaccard(&a[0], &a[1])))
}
