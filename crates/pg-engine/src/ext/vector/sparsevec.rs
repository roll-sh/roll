//! The sparsevec type (`sparsevec.c`): up to 1,000,000,000 dimensions, up to 16,000 of
//! them nonzero. A value's bytes are the dimensions, the count of nonzero elements, their
//! 0-based indices in ascending order and their values (int32s and float4s, little-endian).

use super::common::*;
use super::dist;
use super::vector::{comparisons, out_of_range};
use crate::Result;
use crate::ext::types::{ExtType, SPARSEVEC};
use crate::types::Value;
use bytes::{BufMut, BytesMut};
use std::borrow::Cow;
use std::cmp::Ordering;

const MAX_DIM: i64 = 1_000_000_000;
const MAX_NNZ: i64 = 16000;
const NAME: &str = "sparsevec";

pub(crate) const TYPE: ExtType = ExtType {
    kind: SPARSEVEC,
    extension: "vector",
    name: "sparsevec",
    input: |s| parse(s, -1),
    output: |b, _| output(b),
    cmp,
    // (No zeros are stored: equal values have equal bytes.)
    canon: |b| Cow::Borrowed(b),
    key,
    to_builtin: None,
    from_builtin: None,
    hash: false,
};

/// A sparsevec value read in place.
pub(super) struct Sparse<'a> {
    pub dim: i32,
    pub nnz: usize,
    /// The indices' and the values' bytes.
    pub idxs: &'a [u8],
    pub vals: &'a [u8],
}

impl<'a> Sparse<'a> {
    pub fn new(b: &'a [u8]) -> Sparse<'a> {
        if b.len() < 8 {
            return Sparse { dim: 0, nnz: 0, idxs: &[], vals: &[] };
        }
        let nnz = (i32_at(b, 1).max(0) as usize).min((b.len() - 8) / 8);
        Sparse {
            dim: i32_at(b, 0),
            nnz,
            idxs: &b[8..8 + 4 * nnz],
            vals: &b[8 + 4 * nnz..8 + 8 * nnz],
        }
    }

    pub fn idx(&self, i: usize) -> i32 {
        i32_at(self.idxs, i)
    }

    pub fn val(&self, i: usize) -> f32 {
        f32_at(self.vals, i)
    }
}

/// A sparsevec's bytes: `dim`, then the (0-based index, value) elements.
fn to_bytes(dim: i32, elems: &[(i32, f32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 8 * elems.len());
    out.extend_from_slice(&dim.to_le_bytes());
    out.extend_from_slice(&(elems.len() as i32).to_le_bytes());
    for (i, _) in elems {
        out.extend_from_slice(&i.to_le_bytes());
    }
    for (_, v) in elems {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn value(dim: i32, elems: &[(i32, f32)]) -> Value {
    Value::Ext(SPARSEVEC, to_bytes(dim, elems).into())
}

fn check_dim(dim: i64) -> Result<()> {
    super::common::check_dim(dim, NAME, MAX_DIM)
}

/// `CheckDims`.
fn check_dims(a: &Sparse, b: &Sparse) -> Result<()> {
    if a.dim != b.dim {
        return Err(data_exception(format!("different sparsevec dimensions {} and {}", a.dim, b.dim)));
    }
    Ok(())
}

/// `CheckNnz`.
fn check_nnz(nnz: i64, dim: i64) -> Result<()> {
    if nnz < 0 {
        return Err(data_exception("sparsevec cannot have negative number of elements"));
    }
    if nnz > MAX_NNZ {
        return Err(limit_exceeded(format!("sparsevec cannot have more than {MAX_NNZ} non-zero elements")));
    }
    if nnz > dim {
        return Err(limit_exceeded("sparsevec cannot have more elements than dimensions"));
    }
    Ok(())
}

/// `CheckIndex` of the `i`th of `indices`.
fn check_index(indices: &[i32], i: usize, dim: i32) -> Result<()> {
    let index = indices[i];
    if index < 0 || index >= dim {
        return Err(data_exception("sparsevec index out of bounds"));
    }
    if i > 0 {
        if index < indices[i - 1] {
            return Err(data_exception("sparsevec indices must be in ascending order"));
        }
        if index == indices[i - 1] {
            return Err(data_exception("sparsevec indices must not contain duplicates"));
        }
    }
    Ok(())
}

/// `sparsevec_in`'s parse of `lit` (`{1:1.5,3:2}/5`).
fn parse(lit: &str, typmod: i32) -> Result<Vec<u8>> {
    let s = lit.as_bytes();
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let bad = |detail| bad_input(NAME, lit, detail);
    let skip = |pt: &mut usize| {
        while is_space(at(*pt)) {
            *pt += 1;
        }
    };
    let max_nnz = 1 + s.iter().filter(|&&c| c == b',').count() as i64;
    if max_nnz > MAX_NNZ {
        return Err(limit_exceeded(format!("sparsevec cannot have more than {MAX_NNZ} non-zero elements")));
    }
    let mut pt = 0;
    skip(&mut pt);
    if at(pt) != b'{' {
        return Err(bad(Some("Vector contents must start with \"{\".")));
    }
    pt += 1;
    skip(&mut pt);
    let mut elems: Vec<(i32, f32)> = Vec::new();
    if at(pt) == b'}' {
        pt += 1;
    } else {
        loop {
            skip(&mut pt);
            // Check for empty string like float4in
            if pt >= s.len() {
                return Err(bad(None));
            }
            // Use similar logic as int2vectorin
            let Some((index, len)) = strtol(&s[pt..]) else { return Err(bad(None)) };
            // Keep in int range for correct error message later
            let index = index.clamp(i32::MIN as i64 + 1, i32::MAX as i64) as i32;
            pt += len;
            skip(&mut pt);
            if at(pt) != b':' {
                return Err(bad(None));
            }
            pt += 1;
            skip(&mut pt);
            let Some(f) = strtof(&s[pt..]) else { return Err(bad(None)) };
            // Check for range error like float4in
            if f.erange && (f.value == 0.0 || f.value.is_infinite()) {
                return Err(out_of_range(&s[pt..pt + f.len], NAME));
            }
            check_element(f.value, NAME)?;
            // Do not store zero values
            if f.value != 0.0 {
                // Convert 1-based numbering (SQL) to 0-based (C)
                elems.push((index - 1, f.value));
            }
            pt += f.len;
            skip(&mut pt);
            match at(pt) {
                b',' => pt += 1,
                b'}' => {
                    pt += 1;
                    break;
                }
                _ => return Err(bad(None)),
            }
        }
    }
    skip(&mut pt);
    if at(pt) != b'/' {
        return Err(bad(Some("Unexpected end of input.")));
    }
    pt += 1;
    skip(&mut pt);
    let Some((dim, len)) = strtol(&s[pt..]) else { return Err(bad(None)) };
    let dim = dim.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    pt += len;
    // Only whitespace is allowed after the closing brace
    skip(&mut pt);
    if pt < s.len() {
        return Err(bad(Some("Junk after closing.")));
    }
    check_dim(dim as i64)?;
    check_expected_dim(typmod, dim)?;
    elems.sort_by_key(|e| e.0);
    let indices: Vec<i32> = elems.iter().map(|e| e.0).collect();
    for i in 0..indices.len() {
        check_index(&indices, i, dim)?;
    }
    Ok(to_bytes(dim, &elems))
}

/// `sparsevec_out`.
fn output(b: &[u8]) -> String {
    let s = Sparse::new(b);
    let mut out = String::with_capacity(8 + s.nnz * 16);
    out.push('{');
    for i in 0..s.nnz {
        if i > 0 {
            out.push(',');
        }
        // Convert 0-based numbering (C) to 1-based (SQL)
        out.push_str(&(s.idx(i) as i64 + 1).to_string());
        out.push(':');
        out.push_str(&fmt_f32(s.val(i)));
    }
    out.push_str("}/");
    out.push_str(&s.dim.to_string());
    out
}

/// `sparsevec_cmp_internal`: as the dense vectors compare (element by element up to the
/// shorter's dimensions, then the dimensions).
fn cmp(a: &[u8], b: &[u8]) -> Ordering {
    use Ordering::*;
    let (a, b) = (Sparse::new(a), Sparse::new(b));
    let sign = |neg: bool| if neg { Less } else { Greater };
    let nnz = a.nnz.min(b.nnz);
    // Check values before dimensions to be consistent with Postgres arrays
    for i in 0..nnz {
        if a.idx(i) < b.idx(i) {
            return sign(a.val(i) < 0.0);
        }
        if a.idx(i) > b.idx(i) {
            return sign(b.val(i) < 0.0).reverse();
        }
        if a.val(i) < b.val(i) {
            return Less;
        }
        if a.val(i) > b.val(i) {
            return Greater;
        }
    }
    if a.nnz < b.nnz && b.idx(nnz) < a.dim {
        return sign(b.val(nnz) < 0.0).reverse();
    }
    if a.nnz > b.nnz && a.idx(nnz) < b.dim {
        return sign(a.val(nnz) < 0.0);
    }
    a.dim.cmp(&b.dim)
}

/// An index key ordered as `cmp`, which is the order of the dense vectors as sequences:
/// at the first position where they differ, a nonzero element against a zero one decides
/// by its sign, and the end of the shorter sorts first. A positive element at index i is
/// (2, !i, value), a negative one (0, i, 1, value), and the end at dimensions d is
/// (0, d, 0): below positive elements, and below negative ones from d on.
fn key(b: &[u8], out: &mut Vec<u8>) {
    let s = Sparse::new(b);
    let mut k = Vec::with_capacity(10 * s.nnz + 6);
    for i in 0..s.nnz {
        let (idx, v) = (s.idx(i) as u32, s.val(i));
        if v > 0.0 {
            k.push(2);
            k.extend_from_slice(&(!idx).to_be_bytes());
        } else {
            k.push(0);
            k.extend_from_slice(&idx.to_be_bytes());
            k.push(1);
        }
        k.extend_from_slice(&f32_key(v));
    }
    k.push(0);
    k.extend_from_slice(&(s.dim as u32).to_be_bytes());
    k.push(0);
    crate::ext::types::key_bytes(&k, out);
}

pub(super) fn typmod_in(mods: &[i64]) -> Result<i32> {
    super::common::typmod_in(mods, NAME, MAX_DIM)
}

pub(super) fn check_typmod(b: &[u8], typmod: i32) -> Result<()> {
    check_expected_dim(typmod, Sparse::new(b).dim)
}

/// `sparsevec_send`: the dimensions, the count, an unused int32, the 0-based indices, then
/// the values.
pub(super) fn send(b: &[u8], out: &mut BytesMut) -> Result<()> {
    let s = Sparse::new(b);
    out.put_i32(s.dim);
    out.put_i32(s.nnz as i32);
    out.put_i32(0);
    for i in 0..s.nnz {
        out.put_i32(s.idx(i));
    }
    for i in 0..s.nnz {
        out.put_f32(s.val(i));
    }
    Ok(())
}

/// `sparsevec_recv`.
pub(super) fn recv(buf: &[u8], typmod: i32) -> Result<Vec<u8>> {
    let mut m = Msg(buf);
    let dim = m.i32()?;
    let nnz = m.i32()?;
    let unused = m.i32()?;
    check_dim(dim as i64)?;
    check_nnz(nnz as i64, dim as i64)?;
    check_expected_dim(typmod, dim)?;
    if unused != 0 {
        return Err(data_exception(format!("expected unused to be 0, not {unused}")));
    }
    // Binary representation uses zero-based numbering for indices
    let mut indices = Vec::with_capacity(nnz as usize);
    for i in 0..nnz as usize {
        indices.push(m.i32()?);
        check_index(&indices, i, dim)?;
    }
    let mut elems = Vec::with_capacity(nnz as usize);
    for &index in &indices {
        let v = m.f32()?;
        check_element(v, NAME)?;
        if v == 0.0 {
            return Err(data_exception("binary representation of sparsevec cannot contain zero values"));
        }
        elems.push((index, v));
    }
    Ok(to_bytes(dim, &elems))
}

// ── the C functions ──

natives! {
    "sparsevec_in" => sparsevec_in,
    "sparsevec_out" => sparsevec_out,
    "sparsevec_typmod_in" => sparsevec_typmod_in,
    "sparsevec_recv" => sparsevec_recv,
    "sparsevec_send" => sparsevec_send,
    "sparsevec" => sparsevec,
    "vector_to_sparsevec" => vector_to_sparsevec,
    "halfvec_to_sparsevec" => halfvec_to_sparsevec,
    "array_to_sparsevec" => array_to_sparsevec,
    "sparsevec_l2_distance" => sparsevec_l2_distance,
    "sparsevec_l2_squared_distance" => sparsevec_l2_squared_distance,
    "sparsevec_inner_product" => sparsevec_inner_product,
    "sparsevec_negative_inner_product" => sparsevec_negative_inner_product,
    "sparsevec_cosine_distance" => sparsevec_cosine_distance,
    "sparsevec_l1_distance" => sparsevec_l1_distance,
    "sparsevec_vector_dims" => sparsevec_vector_dims,
    "sparsevec_l2_norm" => sparsevec_l2_norm,
    "sparsevec_l2_normalize" => sparsevec_l2_normalize,
    "sparsevec_lt" => sparsevec_lt,
    "sparsevec_le" => sparsevec_le,
    "sparsevec_eq" => sparsevec_eq,
    "sparsevec_ne" => sparsevec_ne,
    "sparsevec_ge" => sparsevec_ge,
    "sparsevec_gt" => sparsevec_gt,
    "sparsevec_cmp" => sparsevec_cmp,
}

fn sparsevec_in(a: &[Value]) -> Result<Value> {
    Ok(Value::Ext(SPARSEVEC, parse(text(&a[0]), int(&a[2]))?.into()))
}

fn sparsevec_out(a: &[Value]) -> Result<Value> {
    Ok(Value::Text(output(bytes(&a[0])).into()))
}

fn sparsevec_typmod_in(a: &[Value]) -> Result<Value> {
    typmod_in_sql(&a[0], NAME, MAX_DIM)
}

fn sparsevec_recv(_: &[Value]) -> Result<Value> {
    Err(crate::error::internal("sparsevec_recv takes the binary input of a value"))
}

fn sparsevec_send(a: &[Value]) -> Result<Value> {
    let mut out = BytesMut::new();
    send(bytes(&a[0]), &mut out)?;
    Ok(Value::Bytea(out.to_vec().into()))
}

/// `sparsevec(sparsevec, integer, boolean)`: the type modifier checked.
fn sparsevec(a: &[Value]) -> Result<Value> {
    check_typmod(bytes(&a[0]), int(&a[1]))?;
    Ok(a[0].clone())
}

/// A dense vector's nonzero elements (`vector_to_sparsevec`, `halfvec_to_sparsevec`).
fn from_dense(dim: usize, typmod: i32, x: impl Iterator<Item = f32>) -> Result<Value> {
    check_dim(dim as i64)?;
    check_expected_dim(typmod, dim as i32)?;
    let elems: Vec<(i32, f32)> = x.enumerate().filter(|e| e.1 != 0.0).map(|(i, v)| (i as i32, v)).collect();
    check_nnz(elems.len() as i64, dim as i64)?;
    Ok(value(dim as i32, &elems))
}

fn vector_to_sparsevec(a: &[Value]) -> Result<Value> {
    let b = bytes(&a[0]);
    from_dense(b.len() / 4, int(&a[1]), (0..b.len() / 4).map(|i| f32_at(b, i)))
}

fn halfvec_to_sparsevec(a: &[Value]) -> Result<Value> {
    let b = bytes(&a[0]);
    from_dense(b.len() / 2, int(&a[1]), (0..b.len() / 2).map(|i| half_to_f32(u16_at(b, i))))
}

/// `array_to_sparsevec`: the nonzero elements as floats.
fn array_to_sparsevec(a: &[Value]) -> Result<Value> {
    let arr = array_items(&a[0])?;
    let n = arr.items.len();
    check_dim(n as i64)?;
    check_expected_dim(int(&a[1]), n as i32)?;
    let x = arr.items.iter().map(|v| element_f32(&arr.elem, v)).collect::<Result<Vec<f32>>>()?;
    let elems: Vec<(i32, f32)> = x.into_iter().enumerate().filter(|e| e.1 != 0.0).map(|(i, v)| (i as i32, v)).collect();
    check_nnz(elems.len() as i64, n as i64)?;
    for e in &elems {
        check_element(e.1, NAME)?;
    }
    Ok(value(n as i32, &elems))
}

macro_rules! distances {
    ($($name:ident => $f:path),* $(,)?) => {
        $(fn $name(a: &[Value]) -> Result<Value> {
            let (x, y) = (bytes(&a[0]), bytes(&a[1]));
            check_dims(&Sparse::new(x), &Sparse::new(y))?;
            Ok(Value::Float8($f(x, y)))
        })*
    };
}

distances! {
    sparsevec_l2_distance => dist::sparsevec_l2,
    sparsevec_l2_squared_distance => dist::sparsevec_l2_squared,
    sparsevec_inner_product => dist::sparsevec_ip,
    sparsevec_negative_inner_product => dist::sparsevec_negative_ip,
    sparsevec_cosine_distance => dist::sparsevec_cosine,
    sparsevec_l1_distance => dist::sparsevec_l1,
}

/// (In the library, not yet in the install script.)
fn sparsevec_vector_dims(a: &[Value]) -> Result<Value> {
    Ok(Value::Int4(Sparse::new(bytes(&a[0])).dim))
}

fn sparsevec_l2_norm(a: &[Value]) -> Result<Value> {
    Ok(Value::Float8(dist::sparsevec_norm(bytes(&a[0]))))
}

/// `sparsevec_l2_normalize`: each value over the norm in double, then a float; values
/// that become zero dropped.
pub(super) fn normalize(b: &[u8]) -> Result<Value> {
    let s = Sparse::new(b);
    let norm = dist::norm_squared(s.vals).sqrt();
    // Return zero vector for zero norm
    if norm.is_nan() || norm <= 0.0 {
        return Ok(value(s.dim, &vec![(0, 0.0); s.nnz]));
    }
    let mut elems = Vec::with_capacity(s.nnz);
    for i in 0..s.nnz {
        let v = (s.val(i) as f64 / norm) as f32;
        if v.is_infinite() {
            return Err(overflow());
        }
        elems.push((s.idx(i), v));
    }
    elems.retain(|e| e.1 != 0.0);
    Ok(value(s.dim, &elems))
}

fn sparsevec_l2_normalize(a: &[Value]) -> Result<Value> {
    normalize(bytes(&a[0]))
}

comparisons! {
    sparsevec_lt => is_lt,
    sparsevec_le => is_le,
    sparsevec_eq => is_eq,
    sparsevec_ne => is_ne,
    sparsevec_ge => is_ge,
    sparsevec_gt => is_gt,
}

fn sparsevec_cmp(a: &[Value]) -> Result<Value> {
    Ok(Value::Int4(cmp(bytes(&a[0]), bytes(&a[1])) as i32))
}
