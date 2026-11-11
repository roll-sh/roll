//! The halfvec type (`halfvec.c`): up to 16,000 finite IEEE binary16 elements. A value's
//! bytes are the halves' bits, little-endian. Arithmetic is in float, each result rounded
//! back to a half (the build has no `_Float16`).

use super::common::*;
use super::dist;
use super::sparsevec::Sparse;
use super::vector::{accum, comparisons, out_of_range, sub_range};
use crate::Result;
use crate::ext::types::{ExtType, HALFVEC};
use crate::types::Value;
use bytes::{BufMut, BytesMut};
use std::borrow::Cow;
use std::cmp::Ordering;

const MAX_DIM: i64 = 16000;
const NAME: &str = "halfvec";

pub(crate) const TYPE: ExtType = ExtType {
    kind: HALFVEC,
    extension: "vector",
    name: "halfvec",
    input: |s| parse(s, -1),
    output: |b, _| output(b),
    cmp,
    canon,
    key,
    to_builtin: None,
    from_builtin: None,
    hash: false,
};

fn dim(b: &[u8]) -> usize {
    b.len() / 2
}

fn halves(b: &[u8]) -> impl Iterator<Item = u16> + '_ {
    b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c))
}

fn floats(b: &[u8]) -> impl Iterator<Item = f32> + '_ {
    halves(b).map(half_to_f32)
}

fn value(x: impl IntoIterator<Item = u16>) -> Value {
    Value::Ext(HALFVEC, to_bytes(x).into())
}

fn to_bytes(x: impl IntoIterator<Item = u16>) -> Vec<u8> {
    x.into_iter().flat_map(u16::to_le_bytes).collect()
}

fn check_dim(dim: i64) -> Result<()> {
    super::common::check_dim(dim, NAME, MAX_DIM)
}

/// `CheckDims`.
fn check_dims(a: &[u8], b: &[u8]) -> Result<()> {
    if dim(a) != dim(b) {
        return Err(data_exception(format!("different halfvec dimensions {} and {}", dim(a), dim(b))));
    }
    Ok(())
}

/// `CheckElement`.
fn check_element(h: u16) -> Result<()> {
    if half_is_nan(h) {
        return Err(data_exception("NaN not allowed in halfvec"));
    }
    if half_is_inf(h) {
        return Err(data_exception("infinite value not allowed in halfvec"));
    }
    Ok(())
}

/// `halfvec_in`'s parse of `lit`: each element read as a float (strtof), then rounded to
/// a half.
fn parse(lit: &str, typmod: i32) -> Result<Vec<u8>> {
    let s = lit.as_bytes();
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let bad = |detail| bad_input(NAME, lit, detail);
    let mut pt = 0;
    while is_space(at(pt)) {
        pt += 1;
    }
    if at(pt) != b'[' {
        return Err(bad(Some("Vector contents must start with \"[\".")));
    }
    pt += 1;
    while is_space(at(pt)) {
        pt += 1;
    }
    if at(pt) == b']' {
        return Err(data_exception("halfvec must have at least 1 dimension"));
    }
    let mut x = Vec::new();
    loop {
        if x.len() as i64 == MAX_DIM {
            return Err(limit_exceeded(format!("halfvec cannot have more than {MAX_DIM} dimensions")));
        }
        while is_space(at(pt)) {
            pt += 1;
        }
        // Check for empty string like float4in
        if pt >= s.len() {
            return Err(bad(None));
        }
        let Some(f) = strtof(&s[pt..]) else { return Err(bad(None)) };
        let h = f32_to_half(f.value);
        // Check for range error like float4in
        if (f.erange && f.value.is_infinite()) || (half_is_inf(h) && !f.value.is_infinite()) {
            return Err(out_of_range(&s[pt..pt + f.len], NAME));
        }
        check_element(h)?;
        x.push(h);
        pt += f.len;
        while is_space(at(pt)) {
            pt += 1;
        }
        match at(pt) {
            b',' => pt += 1,
            b']' => {
                pt += 1;
                break;
            }
            _ => return Err(bad(None)),
        }
    }
    // Only whitespace is allowed after the closing brace
    while is_space(at(pt)) {
        pt += 1;
    }
    if pt < s.len() {
        return Err(bad(Some("Junk after closing right brace.")));
    }
    check_dim(x.len() as i64)?;
    check_expected_dim(typmod, x.len() as i32)?;
    Ok(to_bytes(x))
}

/// `halfvec_out`: each half as the shortest text of its float.
fn output(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + dim(b) * 8);
    s.push('[');
    for (i, v) in floats(b).enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&fmt_f32(v));
    }
    s.push(']');
    s
}

/// `halfvec_cmp_internal`.
fn cmp(a: &[u8], b: &[u8]) -> Ordering {
    for (x, y) in floats(a).zip(floats(b)) {
        if x < y {
            return Ordering::Less;
        }
        if x > y {
            return Ordering::Greater;
        }
    }
    dim(a).cmp(&dim(b))
}

/// Equal halfvecs in one form: the zeros positive.
fn canon(b: &[u8]) -> Cow<'_, [u8]> {
    if halves(b).any(|h| h == 0x8000) {
        Cow::Owned(to_bytes(halves(b).map(|h| if h == 0x8000 { 0 } else { h })))
    } else {
        Cow::Borrowed(b)
    }
}

fn key(b: &[u8], out: &mut Vec<u8>) {
    let k: Vec<u8> = floats(b).flat_map(f32_key).collect();
    crate::ext::types::key_bytes(&k, out);
}

pub(super) fn typmod_in(mods: &[i64]) -> Result<i32> {
    super::common::typmod_in(mods, NAME, MAX_DIM)
}

pub(super) fn check_typmod(b: &[u8], typmod: i32) -> Result<()> {
    check_expected_dim(typmod, dim(b) as i32)
}

/// `halfvec_send`: the dimensions (int16), an unused int16, then the halves.
pub(super) fn send(b: &[u8], out: &mut BytesMut) -> Result<()> {
    out.put_i16(dim(b) as i16);
    out.put_i16(0);
    for h in halves(b) {
        out.put_u16(h);
    }
    Ok(())
}

/// `halfvec_recv`.
pub(super) fn recv(buf: &[u8], typmod: i32) -> Result<Vec<u8>> {
    let mut m = Msg(buf);
    let dim = m.i16()?;
    let unused = m.i16()?;
    check_dim(dim as i64)?;
    check_expected_dim(typmod, dim as i32)?;
    if unused != 0 {
        return Err(data_exception(format!("expected unused to be 0, not {unused}")));
    }
    let mut x = Vec::with_capacity(dim as usize);
    for _ in 0..dim {
        let h = m.u16()?;
        check_element(h)?;
        x.push(h);
    }
    Ok(to_bytes(x))
}

// ── the C functions ──

natives! {
    "halfvec_in" => halfvec_in,
    "halfvec_out" => halfvec_out,
    "halfvec_typmod_in" => halfvec_typmod_in,
    "halfvec_recv" => halfvec_recv,
    "halfvec_send" => halfvec_send,
    "halfvec" => halfvec,
    "array_to_halfvec" => array_to_halfvec,
    "halfvec_to_float4" => halfvec_to_float4,
    "vector_to_halfvec" => vector_to_halfvec,
    "sparsevec_to_halfvec" => sparsevec_to_halfvec,
    "halfvec_l2_distance" => halfvec_l2_distance,
    "halfvec_l2_squared_distance" => halfvec_l2_squared_distance,
    "halfvec_inner_product" => halfvec_inner_product,
    "halfvec_negative_inner_product" => halfvec_negative_inner_product,
    "halfvec_cosine_distance" => halfvec_cosine_distance,
    "halfvec_spherical_distance" => halfvec_spherical_distance,
    "halfvec_l1_distance" => halfvec_l1_distance,
    "halfvec_vector_dims" => halfvec_vector_dims,
    "halfvec_l2_norm" => halfvec_l2_norm,
    "halfvec_l2_normalize" => halfvec_l2_normalize,
    "halfvec_add" => halfvec_add,
    "halfvec_sub" => halfvec_sub,
    "halfvec_mul" => halfvec_mul,
    "halfvec_concat" => halfvec_concat,
    "halfvec_binary_quantize" => halfvec_binary_quantize,
    "halfvec_subvector" => halfvec_subvector,
    "halfvec_lt" => halfvec_lt,
    "halfvec_le" => halfvec_le,
    "halfvec_eq" => halfvec_eq,
    "halfvec_ne" => halfvec_ne,
    "halfvec_ge" => halfvec_ge,
    "halfvec_gt" => halfvec_gt,
    "halfvec_cmp" => halfvec_cmp,
    "halfvec_accum" => halfvec_accum,
    "halfvec_avg" => halfvec_avg,
}

fn halfvec_in(a: &[Value]) -> Result<Value> {
    Ok(Value::Ext(HALFVEC, parse(text(&a[0]), int(&a[2]))?.into()))
}

fn halfvec_out(a: &[Value]) -> Result<Value> {
    Ok(Value::Text(output(bytes(&a[0])).into()))
}

fn halfvec_typmod_in(a: &[Value]) -> Result<Value> {
    typmod_in_sql(&a[0], NAME, MAX_DIM)
}

fn halfvec_recv(_: &[Value]) -> Result<Value> {
    Err(crate::error::internal("halfvec_recv takes the binary input of a value"))
}

fn halfvec_send(a: &[Value]) -> Result<Value> {
    let mut out = BytesMut::new();
    send(bytes(&a[0]), &mut out)?;
    Ok(Value::Bytea(out.to_vec().into()))
}

/// `halfvec(halfvec, integer, boolean)`: the type modifier checked.
fn halfvec(a: &[Value]) -> Result<Value> {
    check_typmod(bytes(&a[0]), int(&a[1]))?;
    Ok(a[0].clone())
}

/// `array_to_halfvec`: each element as a float, then a half (`Float4ToHalf`).
fn array_to_halfvec(a: &[Value]) -> Result<Value> {
    let arr = array_items(&a[0])?;
    let n = arr.items.len();
    check_dim(n as i64)?;
    check_expected_dim(int(&a[1]), n as i32)?;
    let x = arr
        .items
        .iter()
        .map(|v| f32_to_half_checked(element_f32(&arr.elem, v)?))
        .collect::<Result<Vec<u16>>>()?;
    for &h in &x {
        check_element(h)?;
    }
    Ok(value(x))
}

fn halfvec_to_float4(a: &[Value]) -> Result<Value> {
    Ok(float4_array(floats(bytes(&a[0]))))
}

fn vector_to_halfvec(a: &[Value]) -> Result<Value> {
    let b = bytes(&a[0]);
    let n = b.len() / 4;
    check_dim(n as i64)?;
    check_expected_dim(int(&a[1]), n as i32)?;
    let x = (0..n).map(|i| f32_to_half_checked(f32_at(b, i))).collect::<Result<Vec<u16>>>()?;
    Ok(value(x))
}

fn sparsevec_to_halfvec(a: &[Value]) -> Result<Value> {
    let s = Sparse::new(bytes(&a[0]));
    check_dim(s.dim as i64)?;
    check_expected_dim(int(&a[1]), s.dim)?;
    let mut x = vec![0u16; s.dim as usize];
    for i in 0..s.nnz {
        let index = s.idx(i);
        // Safety check
        if index < 0 || index >= s.dim {
            return Err(crate::error::internal("index out of bounds"));
        }
        x[index as usize] = f32_to_half_checked(s.val(i))?;
    }
    Ok(value(x))
}

macro_rules! distances {
    ($($name:ident => $f:path),* $(,)?) => {
        $(fn $name(a: &[Value]) -> Result<Value> {
            let (x, y) = (bytes(&a[0]), bytes(&a[1]));
            check_dims(x, y)?;
            Ok(Value::Float8($f(x, y)))
        })*
    };
}

distances! {
    halfvec_l2_distance => dist::halfvec_l2,
    halfvec_l2_squared_distance => dist::halfvec_l2_squared,
    halfvec_inner_product => dist::halfvec_ip,
    halfvec_negative_inner_product => dist::halfvec_negative_ip,
    halfvec_cosine_distance => dist::halfvec_cosine,
    halfvec_spherical_distance => dist::halfvec_spherical,
    halfvec_l1_distance => dist::halfvec_l1,
}

fn halfvec_vector_dims(a: &[Value]) -> Result<Value> {
    Ok(Value::Int4(dim(bytes(&a[0])) as i32))
}

fn halfvec_l2_norm(a: &[Value]) -> Result<Value> {
    Ok(Value::Float8(dist::halfvec_norm(bytes(&a[0]))))
}

/// `halfvec_l2_normalize`: each element over the norm in double, then a float, then a
/// half; zeros for a zero norm.
pub(super) fn normalize(b: &[u8]) -> Result<Value> {
    let norm = dist::half_norm_squared(b).sqrt();
    // Return zero vector for zero norm
    if norm.is_nan() || norm <= 0.0 {
        return Ok(value(std::iter::repeat_n(0, dim(b))));
    }
    let x: Vec<u16> = floats(b).map(|v| f32_to_half((v as f64 / norm) as f32)).collect();
    if x.iter().any(|&h| half_is_inf(h)) {
        return Err(overflow());
    }
    Ok(value(x))
}

fn halfvec_l2_normalize(a: &[Value]) -> Result<Value> {
    normalize(bytes(&a[0]))
}

/// An element-wise operation in float, each result rounded to a half.
fn elementwise(a: &[Value], op: fn(f32, f32) -> f32) -> Result<(Vec<u16>, &[u8], &[u8])> {
    let (x, y) = (bytes(&a[0]), bytes(&a[1]));
    check_dims(x, y)?;
    let r = floats(x).zip(floats(y)).map(|(p, q)| f32_to_half(op(p, q))).collect();
    Ok((r, x, y))
}

fn halfvec_add(a: &[Value]) -> Result<Value> {
    let (r, _, _) = elementwise(a, |p, q| p + q)?;
    if r.iter().any(|&h| half_is_inf(h)) {
        return Err(overflow());
    }
    Ok(value(r))
}

fn halfvec_sub(a: &[Value]) -> Result<Value> {
    let (r, _, _) = elementwise(a, |p, q| p - q)?;
    if r.iter().any(|&h| half_is_inf(h)) {
        return Err(overflow());
    }
    Ok(value(r))
}

fn halfvec_mul(a: &[Value]) -> Result<Value> {
    let (r, x, y) = elementwise(a, |p, q| p * q)?;
    for ((&h, p), q) in r.iter().zip(halves(x)).zip(halves(y)) {
        if half_is_inf(h) {
            return Err(overflow());
        }
        if half_is_zero(h) && !(half_is_zero(p) || half_is_zero(q)) {
            return Err(underflow());
        }
    }
    Ok(value(r))
}

fn halfvec_concat(a: &[Value]) -> Result<Value> {
    let (x, y) = (bytes(&a[0]), bytes(&a[1]));
    check_dim((dim(x) + dim(y)) as i64)?;
    Ok(Value::Ext(HALFVEC, [x, y].concat().into()))
}

fn halfvec_binary_quantize(a: &[Value]) -> Result<Value> {
    let b = bytes(&a[0]);
    let mut out = vec![0u8; dim(b).div_ceil(8)];
    for (i, v) in floats(b).enumerate() {
        if v > 0.0 {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
    Ok(Value::Bits(dim(b) as u32, out.into()))
}

fn halfvec_subvector(a: &[Value]) -> Result<Value> {
    let b = bytes(&a[0]);
    let (from, n) = sub_range(dim(b), int(&a[1]), int(&a[2]), NAME)?;
    Ok(Value::Ext(HALFVEC, b[2 * from..2 * (from + n)].to_vec().into()))
}

comparisons! {
    halfvec_lt => is_lt,
    halfvec_le => is_le,
    halfvec_eq => is_eq,
    halfvec_ne => is_ne,
    halfvec_ge => is_ge,
    halfvec_gt => is_gt,
}

fn halfvec_cmp(a: &[Value]) -> Result<Value> {
    Ok(Value::Int4(cmp(bytes(&a[0]), bytes(&a[1])) as i32))
}

fn halfvec_accum(a: &[Value]) -> Result<Value> {
    accum(a, "halfvec_accum", |b| floats(b).collect())
}

/// `halfvec_avg`: the sums over the count, as floats, then halves.
fn halfvec_avg(a: &[Value]) -> Result<Value> {
    let s = state(&a[0], "halfvec_avg")?;
    let n = s[0];
    // SQL defines AVG of no values to be NULL
    if n == 0.0 {
        return Ok(Value::Null);
    }
    check_dim(s.len() as i64 - 1)?;
    let mut x = Vec::with_capacity(s.len() - 1);
    for v in &s[1..] {
        let h = f32_to_half_checked((v / n) as f32)?;
        check_element(h)?;
        x.push(h);
    }
    Ok(value(x))
}
