//! Bit-string functions and operators (Postgres's varbit.c).

use super::{FnCtx, Registry};
use crate::bail;
use crate::error::Result;
use crate::types::text::{bits_of, bits_value};
use crate::types::{DataType as T, Value};

fn bits(v: &Value) -> Vec<bool> {
    match v {
        Value::Bits(len, bytes) => bits_of(*len, bytes),
        _ => Vec::new(),
    }
}

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

/// `bitand` / `bitor` / `bitxor`: equal lengths only.
pub(crate) fn bitwise(a: &[Value], what: &str, f: fn(bool, bool) -> bool) -> Result<Value> {
    let (l, r) = (bits(&a[0]), bits(&a[1]));
    if l.len() != r.len() {
        bail!(
            STRING_DATA_LENGTH_MISMATCH,
            "cannot {what} bit strings of different sizes"
        );
    }
    Ok(bits_value(
        &l.iter().zip(&r).map(|(x, y)| f(*x, *y)).collect::<Vec<_>>(),
    ))
}

/// `bitshiftleft` (negative: right), zero-filled, the length kept.
fn shift(v: &[bool], n: i64) -> Value {
    let len = v.len() as i64;
    let out: Vec<bool> = (0..len)
        .map(|i| {
            let src = i + n;
            (0..len).contains(&src) && v[src as usize]
        })
        .collect();
    bits_value(&out)
}

/// `bitsubstr`: SQL substring semantics over bits; `len` None: to the end.
fn substr(v: &[bool], start: i64, len: Option<i64>) -> Result<Vec<bool>> {
    let end = match len {
        Some(l) if l < 0 => bail!(SUBSTRING_ERROR, "negative substring length not allowed"),
        Some(l) => start.saturating_add(l),
        None => i64::MAX,
    };
    let (s, e) = (start.max(1), end.min(v.len() as i64 + 1));
    if e <= s {
        return Ok(Vec::new());
    }
    Ok(v[(s - 1) as usize..(e - 1) as usize].to_vec())
}

fn bit_index(v: &[bool], n: i64) -> Result<usize> {
    if n < 0 || n >= v.len() as i64 {
        bail!(
            ARRAY_SUBSCRIPT_ERROR,
            "bit index {n} out of valid range (0..{})",
            v.len() as i64 - 1
        );
    }
    Ok(n as usize)
}

fn bitcat(a: &[Value], _: &FnCtx) -> Result<Value> {
    let mut v = bits(&a[0]);
    v.extend(bits(&a[1]));
    Ok(bits_value(&v))
}

fn bitand(a: &[Value], _: &FnCtx) -> Result<Value> {
    bitwise(a, "AND", |x, y| x & y)
}

fn bitor(a: &[Value], _: &FnCtx) -> Result<Value> {
    bitwise(a, "OR", |x, y| x | y)
}

fn bitxor(a: &[Value], _: &FnCtx) -> Result<Value> {
    bitwise(a, "XOR", |x, y| x ^ y)
}

fn bitnot(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(bits_value(
        &bits(&a[0]).iter().map(|b| !b).collect::<Vec<_>>(),
    ))
}

fn bitshiftleft(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(shift(&bits(&a[0]), int(&a[1])))
}

fn bitshiftright(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(shift(&bits(&a[0]), -int(&a[1])))
}

fn bitlength(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(bits(&a[0]).len() as i32))
}

fn bitoctetlength(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(bits(&a[0]).len().div_ceil(8) as i32))
}

fn bit_count(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int8(
        bits(&a[0]).iter().filter(|b| **b).count() as i64
    ))
}

/// `position(sub IN str)`, called as `position(str, sub)`.
fn bitposition(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (s, sub) = (bits(&a[0]), bits(&a[1]));
    // As varbit.c: an empty string (or a longer substring) is 0, an empty substring 1.
    if s.is_empty() || sub.len() > s.len() {
        return Ok(Value::Int4(0));
    }
    if sub.is_empty() {
        return Ok(Value::Int4(1));
    }
    Ok(Value::Int4(
        s.windows(sub.len())
            .position(|w| w == sub.as_slice())
            .map_or(0, |i| i as i32 + 1),
    ))
}

fn bitsubstr(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(bits_value(&substr(
        &bits(&a[0]),
        int(&a[1]),
        Some(int(&a[2])),
    )?))
}

fn bitsubstr_no_len(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(bits_value(&substr(&bits(&a[0]), int(&a[1]), None)?))
}

/// `overlay(t1 PLACING t2 FROM s [FOR l])`.
fn overlay(t1: &[bool], t2: &[bool], s: i64, l: i64) -> Result<Value> {
    if s <= 0 {
        bail!(SUBSTRING_ERROR, "negative substring length not allowed");
    }
    let mut out = substr(t1, 1, Some(s - 1))?;
    out.extend_from_slice(t2);
    out.extend(substr(t1, s.saturating_add(l), None)?);
    Ok(bits_value(&out))
}

fn bitoverlay(a: &[Value], _: &FnCtx) -> Result<Value> {
    overlay(&bits(&a[0]), &bits(&a[1]), int(&a[2]), int(&a[3]))
}

fn bitoverlay_no_len(a: &[Value], _: &FnCtx) -> Result<Value> {
    let t2 = bits(&a[1]);
    overlay(&bits(&a[0]), &t2, int(&a[2]), t2.len() as i64)
}

fn bitgetbit(a: &[Value], _: &FnCtx) -> Result<Value> {
    let v = bits(&a[0]);
    Ok(Value::Int4(i32::from(v[bit_index(&v, int(&a[1]))?])))
}

fn bitsetbit(a: &[Value], _: &FnCtx) -> Result<Value> {
    let mut v = bits(&a[0]);
    let i = bit_index(&v, int(&a[1]))?;
    v[i] = match int(&a[2]) {
        0 => false,
        1 => true,
        _ => bail!(INVALID_PARAMETER_VALUE, "new bit must be 0 or 1"),
    };
    Ok(bits_value(&v))
}

pub fn register(r: &mut Registry) {
    use T::{Int4, Int8};
    let (bit, varbit) = (T::Bit(-1), T::Varbit(-1));
    r.func(
        "bitcat",
        vec![varbit.clone(), varbit.clone()],
        varbit.clone(),
        bitcat,
    );
    r.binop("||", varbit.clone(), varbit.clone(), varbit.clone(), bitcat);
    for (name, op, f) in [
        ("bitand", "&", bitand as super::FnImpl),
        ("bitor", "|", bitor),
        ("bitxor", "#", bitxor),
    ] {
        r.func(name, vec![bit.clone(), bit.clone()], bit.clone(), f);
        r.binop(op, bit.clone(), bit.clone(), bit.clone(), f);
    }
    r.func("bitnot", vec![bit.clone()], bit.clone(), bitnot);
    r.prefix_op("~", bit.clone(), bit.clone(), bitnot);
    r.func(
        "bitshiftleft",
        vec![bit.clone(), Int4],
        bit.clone(),
        bitshiftleft,
    );
    r.binop("<<", bit.clone(), Int4, bit.clone(), bitshiftleft);
    r.func(
        "bitshiftright",
        vec![bit.clone(), Int4],
        bit.clone(),
        bitshiftright,
    );
    r.binop(">>", bit.clone(), Int4, bit.clone(), bitshiftright);
    r.func("length", vec![bit.clone()], Int4, bitlength);
    r.func("bit_length", vec![bit.clone()], Int4, bitlength);
    r.func("octet_length", vec![bit.clone()], Int4, bitoctetlength);
    r.func("bit_count", vec![bit.clone()], Int8, bit_count);
    r.func(
        "position",
        vec![bit.clone(), bit.clone()],
        Int4,
        bitposition,
    );
    r.func(
        "substring",
        vec![bit.clone(), Int4, Int4],
        bit.clone(),
        bitsubstr,
    );
    r.func(
        "substring",
        vec![bit.clone(), Int4],
        bit.clone(),
        bitsubstr_no_len,
    );
    r.func(
        "overlay",
        vec![bit.clone(), bit.clone(), Int4, Int4],
        bit.clone(),
        bitoverlay,
    );
    r.func(
        "overlay",
        vec![bit.clone(), bit.clone(), Int4],
        bit.clone(),
        bitoverlay_no_len,
    );
    r.func("get_bit", vec![bit.clone(), Int4], Int4, bitgetbit);
    r.func("set_bit", vec![bit.clone(), Int4, Int4], bit, bitsetbit);
}
