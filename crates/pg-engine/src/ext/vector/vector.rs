//! The vector type (`vector.c`): up to 16,000 finite float4 elements. A value's bytes are
//! the floats, little-endian.

use super::common::*;
use super::dist;
use super::sparsevec::Sparse;
use crate::Result;
use crate::error::Error;
use crate::ext::types::{ExtType, VECTOR};
use crate::types::Value;
use bytes::{BufMut, BytesMut};
use std::borrow::Cow;
use std::cmp::Ordering;

const MAX_DIM: i64 = 16000;
const NAME: &str = "vector";

pub(crate) const TYPE: ExtType = ExtType {
    kind: VECTOR,
    extension: "vector",
    name: "vector",
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
    b.len() / 4
}

fn floats(b: &[u8]) -> impl Iterator<Item = f32> + '_ {
    b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c))
}

fn value(x: impl IntoIterator<Item = f32>) -> Value {
    Value::Ext(VECTOR, to_bytes(x).into())
}

fn to_bytes(x: impl IntoIterator<Item = f32>) -> Vec<u8> {
    x.into_iter().flat_map(f32::to_le_bytes).collect()
}

fn check_dim(dim: i64) -> Result<()> {
    super::common::check_dim(dim, NAME, MAX_DIM)
}

/// `CheckDims`.
fn check_dims(a: &[u8], b: &[u8]) -> Result<()> {
    if dim(a) != dim(b) {
        return Err(data_exception(format!("different vector dimensions {} and {}", dim(a), dim(b))));
    }
    Ok(())
}

/// `vector_in`'s parse of `lit`, its dimensions checked against `typmod` (-1: none).
pub(super) fn parse(lit: &str, typmod: i32) -> Result<Vec<u8>> {
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
        return Err(data_exception("vector must have at least 1 dimension"));
    }
    let mut x = Vec::new();
    loop {
        if x.len() as i64 == MAX_DIM {
            return Err(limit_exceeded(format!("vector cannot have more than {MAX_DIM} dimensions")));
        }
        while is_space(at(pt)) {
            pt += 1;
        }
        // Check for empty string like float4in
        if pt >= s.len() {
            return Err(bad(None));
        }
        let Some(f) = strtof(&s[pt..]) else { return Err(bad(None)) };
        // Check for range error like float4in
        if f.erange && f.value.is_infinite() {
            return Err(out_of_range(&s[pt..pt + f.len], NAME));
        }
        check_element(f.value, NAME)?;
        x.push(f.value);
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

/// `"<text>" is out of range for type <name>`.
pub(super) fn out_of_range(text: &[u8], name: &str) -> Error {
    Error::new(
        crate::error::code::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("\"{}\" is out of range for type {name}", String::from_utf8_lossy(text)),
    )
}

/// `vector_out`.
fn output(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + dim(b) * 12);
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

/// `vector_cmp_internal`: element by element, then the dimensions.
pub(super) fn cmp(a: &[u8], b: &[u8]) -> Ordering {
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

/// Equal vectors in one form: the zeros positive.
fn canon(b: &[u8]) -> Cow<'_, [u8]> {
    if floats(b).any(|v| v == 0.0 && v.is_sign_negative()) {
        Cow::Owned(to_bytes(floats(b).map(|v| if v == 0.0 { 0.0 } else { v })))
    } else {
        Cow::Borrowed(b)
    }
}

/// The elements' keys in order, then an end shorter vectors sort first by.
fn key(b: &[u8], out: &mut Vec<u8>) {
    let k: Vec<u8> = floats(b).flat_map(f32_key).collect();
    crate::ext::types::key_bytes(&k, out);
}

pub(super) fn typmod_in(mods: &[i64]) -> Result<i32> {
    super::common::typmod_in(mods, NAME, MAX_DIM)
}

/// The length coercion (`vector(vector, integer, boolean)`).
pub(super) fn check_typmod(b: &[u8], typmod: i32) -> Result<()> {
    check_expected_dim(typmod, dim(b) as i32)
}

/// `vector_send`: the dimensions (int16), an unused int16, then the floats.
pub(super) fn send(b: &[u8], out: &mut BytesMut) -> Result<()> {
    out.put_i16(dim(b) as i16);
    out.put_i16(0);
    for v in floats(b) {
        out.put_f32(v);
    }
    Ok(())
}

/// `vector_recv`.
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
        let v = m.f32()?;
        check_element(v, NAME)?;
        x.push(v);
    }
    Ok(to_bytes(x))
}

// ── the C functions ──

natives! {
    "vector_in" => vector_in,
    "vector_out" => vector_out,
    "vector_typmod_in" => vector_typmod_in,
    "vector_recv" => vector_recv,
    "vector_send" => vector_send,
    "vector" => vector,
    "array_to_vector" => array_to_vector,
    "vector_to_float4" => vector_to_float4,
    "halfvec_to_vector" => halfvec_to_vector,
    "sparsevec_to_vector" => sparsevec_to_vector,
    "l2_distance" => l2_distance,
    "vector_l2_squared_distance" => vector_l2_squared_distance,
    "inner_product" => inner_product,
    "vector_negative_inner_product" => vector_negative_inner_product,
    "cosine_distance" => cosine_distance,
    "vector_spherical_distance" => vector_spherical_distance,
    "l1_distance" => l1_distance,
    "vector_dims" => vector_dims,
    "vector_norm" => vector_norm,
    "l2_normalize" => l2_normalize,
    "vector_add" => vector_add,
    "vector_sub" => vector_sub,
    "vector_mul" => vector_mul,
    "vector_concat" => vector_concat,
    "binary_quantize" => binary_quantize,
    "subvector" => subvector,
    "vector_lt" => vector_lt,
    "vector_le" => vector_le,
    "vector_eq" => vector_eq,
    "vector_ne" => vector_ne,
    "vector_ge" => vector_ge,
    "vector_gt" => vector_gt,
    "vector_cmp" => vector_cmp,
    "vector_accum" => vector_accum,
    "vector_combine" => vector_combine,
    "vector_avg" => vector_avg,
}

/// `vector_in(cstring, oid, typmod)`.
fn vector_in(a: &[Value]) -> Result<Value> {
    Ok(Value::Ext(VECTOR, parse(text(&a[0]), int(&a[2]))?.into()))
}

fn vector_out(a: &[Value]) -> Result<Value> {
    Ok(Value::Text(output(bytes(&a[0])).into()))
}

fn vector_typmod_in(a: &[Value]) -> Result<Value> {
    typmod_in_sql(&a[0], NAME, MAX_DIM)
}

fn vector_recv(_: &[Value]) -> Result<Value> {
    Err(crate::error::internal("vector_recv takes the binary input of a value"))
}

fn vector_send(a: &[Value]) -> Result<Value> {
    let mut out = BytesMut::new();
    send(bytes(&a[0]), &mut out)?;
    Ok(Value::Bytea(out.to_vec().into()))
}

/// `vector(vector, integer, boolean)`: the type modifier checked.
fn vector(a: &[Value]) -> Result<Value> {
    check_typmod(bytes(&a[0]), int(&a[1]))?;
    Ok(a[0].clone())
}

/// `array_to_vector` from int4[], float8[], float4[] or numeric[].
fn array_to_vector(a: &[Value]) -> Result<Value> {
    let arr = array_items(&a[0])?;
    let n = arr.items.len();
    check_dim(n as i64)?;
    check_expected_dim(int(&a[1]), n as i32)?;
    let x = arr.items.iter().map(|v| element_f32(&arr.elem, v)).collect::<Result<Vec<f32>>>()?;
    for &v in &x {
        check_element(v, NAME)?;
    }
    Ok(value(x))
}

/// `vector_to_float4`: float4[].
fn vector_to_float4(a: &[Value]) -> Result<Value> {
    Ok(float4_array(floats(bytes(&a[0]))))
}

/// `halfvec_to_vector`.
fn halfvec_to_vector(a: &[Value]) -> Result<Value> {
    let b = bytes(&a[0]);
    let n = b.len() / 2;
    check_dim(n as i64)?;
    check_expected_dim(int(&a[1]), n as i32)?;
    Ok(value((0..n).map(|i| half_to_f32(u16_at(b, i)))))
}

/// `sparsevec_to_vector`.
fn sparsevec_to_vector(a: &[Value]) -> Result<Value> {
    let s = Sparse::new(bytes(&a[0]));
    check_dim(s.dim as i64)?;
    check_expected_dim(int(&a[1]), s.dim)?;
    let mut x = vec![0.0f32; s.dim as usize];
    for i in 0..s.nnz {
        let index = s.idx(i);
        // Safety check
        if index < 0 || index >= s.dim {
            return Err(crate::error::internal("index out of bounds"));
        }
        x[index as usize] = s.val(i);
    }
    Ok(value(x))
}

/// The distance functions: the dimensions checked, then the kernel's float8.
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
    l2_distance => dist::vector_l2,
    vector_l2_squared_distance => dist::vector_l2_squared,
    inner_product => dist::vector_ip,
    vector_negative_inner_product => dist::vector_negative_ip,
    cosine_distance => dist::vector_cosine,
    vector_spherical_distance => dist::vector_spherical,
    l1_distance => dist::vector_l1,
}

fn vector_dims(a: &[Value]) -> Result<Value> {
    Ok(Value::Int4(dim(bytes(&a[0])) as i32))
}

fn vector_norm(a: &[Value]) -> Result<Value> {
    Ok(Value::Float8(dist::vector_norm(bytes(&a[0]))))
}

/// `l2_normalize`: each element over the norm (in double), zeros for a zero norm.
pub(super) fn normalize(b: &[u8]) -> Result<Value> {
    let norm = dist::norm_squared(b).sqrt();
    // Return zero vector for zero norm
    if norm.is_nan() || norm <= 0.0 {
        return Ok(value(std::iter::repeat_n(0.0, dim(b))));
    }
    let x: Vec<f32> = floats(b).map(|v| (v as f64 / norm) as f32).collect();
    if x.iter().any(|v| v.is_infinite()) {
        return Err(overflow());
    }
    Ok(value(x))
}

fn l2_normalize(a: &[Value]) -> Result<Value> {
    normalize(bytes(&a[0]))
}

/// An element-wise operation, then the overflow check.
fn elementwise(a: &[Value], op: fn(f32, f32) -> f32) -> Result<(Vec<f32>, &[u8], &[u8])> {
    let (x, y) = (bytes(&a[0]), bytes(&a[1]));
    check_dims(x, y)?;
    let r: Vec<f32> = floats(x).zip(floats(y)).map(|(p, q)| op(p, q)).collect();
    Ok((r, x, y))
}

fn vector_add(a: &[Value]) -> Result<Value> {
    let (r, _, _) = elementwise(a, |p, q| p + q)?;
    if r.iter().any(|v| v.is_infinite()) {
        return Err(overflow());
    }
    Ok(value(r))
}

fn vector_sub(a: &[Value]) -> Result<Value> {
    let (r, _, _) = elementwise(a, |p, q| p - q)?;
    if r.iter().any(|v| v.is_infinite()) {
        return Err(overflow());
    }
    Ok(value(r))
}

/// `vector_mul`: the overflow and underflow checks element by element.
fn vector_mul(a: &[Value]) -> Result<Value> {
    let (r, x, y) = elementwise(a, |p, q| p * q)?;
    for ((&v, p), q) in r.iter().zip(floats(x)).zip(floats(y)) {
        if v.is_infinite() {
            return Err(overflow());
        }
        if v == 0.0 && !(p == 0.0 || q == 0.0) {
            return Err(underflow());
        }
    }
    Ok(value(r))
}

fn vector_concat(a: &[Value]) -> Result<Value> {
    let (x, y) = (bytes(&a[0]), bytes(&a[1]));
    check_dim((dim(x) + dim(y)) as i64)?;
    Ok(Value::Ext(VECTOR, [x, y].concat().into()))
}

/// `binary_quantize`: a bit per element, set for a positive one.
fn binary_quantize(a: &[Value]) -> Result<Value> {
    let b = bytes(&a[0]);
    let mut out = vec![0u8; dim(b).div_ceil(8)];
    for (i, v) in floats(b).enumerate() {
        if v > 0.0 {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
    Ok(Value::Bits(dim(b) as u32, out.into()))
}

/// `subvector(v, start, count)`: elements from 1-based `start`, as substring takes them.
pub(super) fn sub_range(d: usize, start: i32, count: i32, name: &str) -> Result<(usize, usize)> {
    let none = || data_exception(format!("{name} must have at least 1 dimension"));
    let (d, mut start, count) = (d as i64, start as i64, count as i64);
    if count < 1 {
        return Err(none());
    }
    let end = if start > d - count { d + 1 } else { start + count };
    // Indexing starts at 1, like substring
    if start < 1 {
        start = 1;
    } else if start > d {
        return Err(none());
    }
    // (CheckDim: none left.)
    let n = end - start;
    if n < 1 {
        return Err(none());
    }
    Ok((start as usize - 1, n as usize))
}

fn subvector(a: &[Value]) -> Result<Value> {
    let b = bytes(&a[0]);
    let (from, n) = sub_range(dim(b), int(&a[1]), int(&a[2]), NAME)?;
    Ok(Value::Ext(VECTOR, b[4 * from..4 * (from + n)].to_vec().into()))
}

/// The comparison operators over `vector_cmp_internal`.
macro_rules! comparisons {
    ($($name:ident => $test:ident),* $(,)?) => {
        $(fn $name(a: &[Value]) -> Result<Value> {
            Ok(Value::Bool(cmp(bytes(&a[0]), bytes(&a[1])).$test()))
        })*
    };
}
pub(super) use comparisons;

comparisons! {
    vector_lt => is_lt,
    vector_le => is_le,
    vector_eq => is_eq,
    vector_ne => is_ne,
    vector_ge => is_ge,
    vector_gt => is_gt,
}

fn vector_cmp(a: &[Value]) -> Result<Value> {
    Ok(Value::Int4(cmp(bytes(&a[0]), bytes(&a[1])) as i32))
}

/// `vector_accum`: the count and the sums in float8 (the first value starts them).
fn vector_accum(a: &[Value]) -> Result<Value> {
    accum(a, "vector_accum", |b| floats(b).collect())
}

/// `vector_accum` / `halfvec_accum` over the new value's elements as floats.
pub(super) fn accum(a: &[Value], caller: &str, elems: fn(&[u8]) -> Vec<f32>) -> Result<Value> {
    let s = state(&a[0], caller)?;
    let x = elems(bytes(&a[1]));
    let newarr = s.len() == 1;
    if !newarr {
        check_expected_dim(s.len() as i32 - 1, x.len() as i32)?;
    }
    let mut out = Vec::with_capacity(x.len() + 1);
    out.push(s[0] + 1.0);
    for (i, v) in x.into_iter().enumerate() {
        if newarr {
            out.push(v as f64);
        } else {
            let r = s[i + 1] + v as f64;
            // Check for overflow
            if r.is_infinite() {
                return Err(overflow());
            }
            out.push(r);
        }
    }
    Ok(float8_array(out))
}

/// `vector_combine` (also `halfvec_combine`): two states' counts and sums added.
fn vector_combine(a: &[Value]) -> Result<Value> {
    let (s1, s2) = (state(&a[0], "vector_combine")?, state(&a[1], "vector_combine")?);
    let (d1, d2) = (s1.len() - 1, s2.len() - 1);
    let mut out = vec![s1[0] + s2[0]];
    if d1 == 0 && d2 != 0 {
        check_dim(d2 as i64)?;
        out.extend_from_slice(&s2[1..]);
    } else if d2 == 0 && d1 != 0 {
        check_dim(d1 as i64)?;
        out.extend_from_slice(&s1[1..]);
    } else if d1 != 0 {
        check_dim(d1 as i64)?;
        check_expected_dim(d1 as i32, d2 as i32)?;
        for (p, q) in s1[1..].iter().zip(&s2[1..]) {
            let v = p + q;
            // Check for overflow
            if v.is_infinite() {
                return Err(overflow());
            }
            out.push(v);
        }
    }
    Ok(float8_array(out))
}

/// `vector_avg`: the sums over the count, as floats (NULL for no values).
fn vector_avg(a: &[Value]) -> Result<Value> {
    let s = state(&a[0], "vector_avg")?;
    let n = s[0];
    // SQL defines AVG of no values to be NULL
    if n == 0.0 {
        return Ok(Value::Null);
    }
    check_dim(s.len() as i64 - 1)?;
    let mut x = Vec::with_capacity(s.len() - 1);
    for v in &s[1..] {
        let r = (v / n) as f32;
        check_element(r, NAME)?;
        x.push(r);
    }
    Ok(value(x))
}
