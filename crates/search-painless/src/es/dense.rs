//! `dense_vector` fields in scripts: the `DenseVector` objects behind `field('v').get()` and
//! `doc['v']`, and the vector score functions (`dotProduct`, `cosineSimilarity`, `l1norm`,
//! `l2norm`, `hamming`) of `VectorScoreScriptUtils`, for every element type.
//!
//! Elasticsearch has one `DenseVector` implementation per element type and storage — `float`
//! (and `bfloat16`) vectors indexed for kNN (`KnnDenseVector`) or only in doc values
//! (`BinaryDenseVector`), `byte` vectors (`ByteKnnDenseVector`, `ByteBinaryDenseVector`) and
//! `bit` vectors (`BitKnnDenseVector`, `BitBinaryDenseVector`) — and they differ in more than
//! arithmetic: which overloads exist, which loops run over the document's length and which
//! over the query's (so a hex query of the wrong length fails differently), the Lucene checks
//! that fire, the stack frames of every failure. [`Impl`] names the implementation and every
//! operation follows the corresponding Java method, frames included.
//!
//! The score functions are class bindings: the query vector is parsed and checked once per
//! call site, against the first document, like the Java constructors; each document is then
//! read into a buffer the instance reuses, so scoring allocates nothing per document.

use std::cell::Cell;
use std::rc::Rc;

use super::{binding, es, es_error, es_value, server_frames, EsObj, SERVER};
use crate::host::{DocKind, DocValues, VectorElement};
use crate::types::cls;
use crate::value::{ArrayData, JArray, Obj, Value};
use crate::vm::{Throw, Thrown, Vm, VR};

/// The Java class implementing a document's `DenseVector`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Impl {
    Knn,
    Binary,
    ByteKnn,
    ByteBinary,
    BitKnn,
    BitBinary,
}

impl Impl {
    fn of(element: VectorElement, binary: bool) -> Impl {
        match (element, binary) {
            (VectorElement::Float | VectorElement::BFloat16, false) => Impl::Knn,
            (VectorElement::Float | VectorElement::BFloat16, true) => Impl::Binary,
            (VectorElement::Byte, false) => Impl::ByteKnn,
            (VectorElement::Byte, true) => Impl::ByteBinary,
            (VectorElement::Bit, false) => Impl::BitKnn,
            (VectorElement::Bit, true) => Impl::BitBinary,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Impl::Knn => "KnnDenseVector",
            Impl::Binary => "BinaryDenseVector",
            Impl::ByteKnn => "ByteKnnDenseVector",
            Impl::ByteBinary => "ByteBinaryDenseVector",
            Impl::BitKnn => "BitKnnDenseVector",
            Impl::BitBinary => "BitBinaryDenseVector",
        }
    }
    pub fn class(self) -> crate::types::ClassId {
        match self {
            Impl::Knn => cls::KNN_DENSE_VECTOR,
            Impl::Binary => cls::BINARY_DENSE_VECTOR,
            Impl::ByteKnn => cls::BYTE_KNN_DENSE_VECTOR,
            Impl::ByteBinary => cls::BYTE_BINARY_DENSE_VECTOR,
            Impl::BitKnn => cls::BIT_KNN_DENSE_VECTOR,
            Impl::BitBinary => cls::BIT_BINARY_DENSE_VECTOR,
        }
    }
    fn is_float(self) -> bool {
        matches!(self, Impl::Knn | Impl::Binary)
    }
    fn is_bit(self) -> bool {
        matches!(self, Impl::BitKnn | Impl::BitBinary)
    }
    /// The byte implementation a bit vector inherits from (`BitKnnDenseVector extends
    /// ByteKnnDenseVector`).
    fn byte_base(self) -> Impl {
        match self {
            Impl::BitKnn => Impl::ByteKnn,
            Impl::BitBinary => Impl::ByteBinary,
            other => other,
        }
    }
    /// A frame in this implementation's source file.
    fn frame(self, method: &str, line: u32) -> String {
        let n = self.name();
        format!("{SERVER}org.elasticsearch.script.field.vectors.{n}.{method}({n}.java:{line})")
    }
}

/// A document's dense vector.
#[derive(Clone)]
pub struct Vector {
    /// The values as floats (byte and bit vectors widened).
    pub v: Rc<[f32]>,
    /// The values of a `byte` or `bit` vector (a bit vector packs eight dimensions per byte).
    pub bytes: Rc<[i8]>,
    pub imp: Impl,
    /// The magnitude: stored with doc-values-only vectors, computed on first use otherwise
    /// (NaN until then).
    magnitude: Cell<f32>,
}

impl Vector {
    /// A float vector indexed for kNN (`magnitude` stored when known).
    pub fn new(v: Vec<f32>, magnitude: Option<f32>) -> Vector {
        Vector { v: Rc::from(v), bytes: Rc::from(Vec::new()), imp: if magnitude.is_some() { Impl::Binary } else { Impl::Knn }, magnitude: Cell::new(magnitude.unwrap_or(f32::NAN)) }
    }

    /// The vector of a document's doc values (`None` when the document has none).
    pub fn from_doc(dv: &DocValues) -> Option<Vector> {
        if dv.kind != DocKind::DenseVector {
            return None;
        }
        let imp = Impl::of(dv.element, dv.vector_binary);
        let (v, bytes): (Rc<[f32]>, Rc<[i8]>) = if imp.is_float() {
            if dv.vector.is_empty() {
                return None;
            }
            (Rc::from(dv.vector.as_slice()), Rc::from(Vec::new()))
        } else {
            if dv.vector_bytes.is_empty() {
                return None;
            }
            (dv.vector_bytes.iter().map(|&b| b as f32).collect(), Rc::from(dv.vector_bytes.as_slice()))
        };
        let magnitude = dv.magnitude;
        Some(Vector { v, bytes, imp, magnitude: Cell::new(magnitude.unwrap_or(f32::NAN)) })
    }

    fn view(&self) -> DocVec<'_> {
        DocVec { imp: self.imp, f: &self.v, b: &self.bytes, magnitude: &self.magnitude }
    }

    pub fn magnitude(&self) -> f32 {
        self.view().magnitude()
    }

    /// `getDims()`: a bit vector has eight dimensions per byte.
    pub fn dims(&self) -> usize {
        self.view().dims()
    }

    /// `getVector()` / `vectorValue`.
    pub fn to_array(&self) -> Value {
        Value::obj(Obj::Array(JArray::from_data(crate::types::Ty::FLOAT, ArrayData::Float(self.v.to_vec()))))
    }
}

/// A document vector being scored: the buffers of the current document, or a [`Vector`].
struct DocVec<'a> {
    imp: Impl,
    f: &'a [f32],
    b: &'a [i8],
    magnitude: &'a Cell<f32>,
}

/// A query vector after the overload Java picked: `float[]`, `byte[]` or `List<Number>`.
#[derive(Clone)]
enum Query {
    Floats(Vec<f32>),
    Bytes(Vec<i8>),
    /// The list's numbers (`floatValue()` and `intValue()` of each).
    List(Vec<f32>, Vec<i32>),
}

impl Query {
    fn len(&self) -> usize {
        match self {
            Query::Floats(v) => v.len(),
            Query::Bytes(v) => v.len(),
            Query::List(v, _) => v.len(),
        }
    }
}

fn srv(f: &str) -> String {
    format!("{SERVER}org.elasticsearch.{f}")
}
fn lucene(f: &str) -> String {
    format!("org.apache.lucene.core@10.5.1/org.apache.lucene.util.{f}")
}
fn simdvec(f: &str) -> String {
    format!("org.elasticsearch.simdvec@9.5.5/org.elasticsearch.simdvec.{f}")
}

fn throw(class: crate::types::ClassId, msg: impl Into<String>, frames: Vec<String>) -> Thrown {
    let mut t = Throw::new(class, Some(msg.into()));
    t.frames = frames;
    t
}
fn uoe(msg: &str, frames: Vec<String>) -> Thrown {
    throw(cls::UNSUPPORTED_OPERATION_EXCEPTION, msg, frames)
}
fn iae(msg: impl Into<String>, frames: Vec<String>) -> Thrown {
    throw(cls::ILLEGAL_ARGUMENT_EXCEPTION, msg, frames)
}

/// `frames` followed by `outer`.
fn chain(mut frames: Vec<String>, outer: &[String]) -> Vec<String> {
    frames.extend_from_slice(outer);
    frames
}

// ------------------------------------------------------------------------------ kernels

fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    super::vecmath::dot(a, b)
}

fn square_distance_f32(a: &[f32], b: &[f32]) -> f32 {
    super::vecmath::square_distance(a, b)
}

/// `ESVectorUtil.dotProduct(byte[], byte[])`.
fn dot_bytes(a: &[i8], b: &[i8], outer: impl FnOnce() -> Vec<String>) -> Result<f32, Thrown> {
    if a.len() != b.len() {
        return Err(iae(format!("vector dimensions incompatible: {}!= {}", a.len(), b.len()), chain(vec![simdvec("ESVectorUtil.dotProduct(ESVectorUtil.java:168)")], &outer())));
    }
    let mut r: i32 = 0;
    for (x, y) in a.iter().zip(b) {
        r += *x as i32 * *y as i32;
    }
    Ok(r as f32)
}

/// `ESVectorUtil.ipFloatByte(float[], byte[])`.
fn ip_float_byte(q: &[f32], d: &[i8], outer: impl FnOnce() -> Vec<String>) -> Result<f32, Thrown> {
    if q.len() != d.len() {
        return Err(iae(format!("vector dimensions incompatible: {}!= {}", q.len(), d.len()), chain(vec![simdvec("ESVectorUtil.ipFloatByte(ESVectorUtil.java:391)")], &outer())));
    }
    let mut r = 0f32;
    for (x, y) in q.iter().zip(d) {
        r += x * *y as f32;
    }
    Ok(r)
}

/// Whether bit `i` of a packed bit vector is set (most significant bit first).
fn bit(d: &[i8], i: usize) -> bool {
    (d[i / 8] as u8) & (0x80 >> (i % 8)) != 0
}

/// `ESVectorUtil.ipByteBit(byte[], byte[])`: the query values where the document has a bit.
fn ip_byte_bit(q: &[i8], d: &[i8], outer: impl FnOnce() -> Vec<String>) -> Result<f32, Thrown> {
    if q.len() != d.len() * 8 {
        return Err(iae(format!("vector dimensions incompatible: {}!= 8 x {}", q.len(), d.len()), chain(vec![simdvec("ESVectorUtil.ipByteBit(ESVectorUtil.java:362)")], &outer())));
    }
    let mut r: i32 = 0;
    for (i, &x) in q.iter().enumerate() {
        if bit(d, i) {
            r += x as i32;
        }
    }
    Ok(r as f32)
}

/// `ESVectorUtil.ipFloatBit(float[], byte[])`.
fn ip_float_bit(q: &[f32], d: &[i8], outer: impl FnOnce() -> Vec<String>) -> Result<f32, Thrown> {
    if q.len() != d.len() * 8 {
        return Err(iae(format!("vector dimensions incompatible: {}!= 8 x {}", q.len(), d.len()), chain(vec![simdvec("ESVectorUtil.ipFloatBit(ESVectorUtil.java:378)")], &outer())));
    }
    let mut r = 0f32;
    for (i, &x) in q.iter().enumerate() {
        if bit(d, i) {
            r += x;
        }
    }
    Ok(r)
}

/// `ESVectorUtil.andBitCount(byte[], byte[])`.
fn and_bit_count(a: &[i8], b: &[i8], outer: impl FnOnce() -> Vec<String>) -> Result<f32, Thrown> {
    if a.len() != b.len() {
        return Err(iae(format!("vector dimensions differ: {}!={}", a.len(), b.len()), chain(vec![simdvec("ESVectorUtil.andBitCount(ESVectorUtil.java:405)")], &outer())));
    }
    Ok(a.iter().zip(b).map(|(x, y)| ((*x & *y) as u8).count_ones()).sum::<u32>() as f32)
}

/// Lucene's `VectorUtil.xorBitCount(byte[], byte[])`.
fn xor_bit_count(a: &[i8], b: &[i8], outer: impl FnOnce() -> Vec<String>) -> Result<i32, Thrown> {
    if a.len() != b.len() {
        return Err(iae(format!("vector dimensions differ: {}!={}", a.len(), b.len()), chain(vec![lucene("VectorUtil.xorBitCount(VectorUtil.java:350)")], &outer())));
    }
    Ok(a.iter().zip(b).map(|(x, y)| ((*x ^ *y) as u8).count_ones()).sum::<u32>() as i32)
}

/// Lucene's `VectorUtil.squareDistance(byte[], byte[])`.
fn square_distance_bytes(a: &[i8], b: &[i8], outer: impl FnOnce() -> Vec<String>) -> Result<i32, Thrown> {
    if a.len() != b.len() {
        return Err(iae(format!("vector dimensions differ: {}!={}", a.len(), b.len()), chain(vec![lucene("VectorUtil.squareDistance(VectorUtil.java:118)")], &outer())));
    }
    let mut r: i32 = 0;
    for (x, y) in a.iter().zip(b) {
        let d = *x as i32 - *y as i32;
        r += d * d;
    }
    Ok(r)
}

fn aioobe(i: usize, len: usize, frames: Vec<String>) -> Thrown {
    throw(cls::ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION, format!("Index {i} out of bounds for length {len}"), frames)
}

/// `DenseVector.getMagnitude(float[])`.
fn magnitude_f32(v: &[f32]) -> f32 {
    (dot_f32(v, v) as f64).sqrt() as f32
}
/// `DenseVector.getMagnitude(List<Number>)`.
fn magnitude_list(v: &[f32]) -> f32 {
    let mut m = 0f64;
    for x in v {
        m += (x * x) as f64;
    }
    m.sqrt() as f32
}
/// `DenseVector.getMagnitude(byte[])`.
fn magnitude_bytes(v: &[i8]) -> f32 {
    let s: i32 = v.iter().map(|&x| x as i32 * x as i32).sum();
    (s as f64).sqrt() as f32
}

// ------------------------------------------------------------------------------ DenseVector

impl DocVec<'_> {
    fn dims(&self) -> usize {
        if self.imp.is_bit() {
            self.b.len() * 8
        } else if self.imp.is_float() {
            self.f.len()
        } else {
            self.b.len()
        }
    }

    fn magnitude(&self) -> f32 {
        let m = self.magnitude.get();
        if !m.is_nan() {
            return m;
        }
        let m = match self.imp {
            Impl::Knn | Impl::Binary => magnitude_f32(self.f),
            Impl::ByteKnn | Impl::ByteBinary => magnitude_bytes(self.b),
            Impl::BitKnn | Impl::BitBinary => (self.b.iter().map(|&x| (x as u8).count_ones()).sum::<u32>() as f64).sqrt() as f32,
        };
        self.magnitude.set(m);
        m
    }

    /// `dotProduct(byte[] | float[] | List)` of the implementation.
    fn dot(&self, q: &Query, outer: &dyn Fn() -> Vec<String>) -> Result<f64, Thrown> {
        let imp = self.imp;
        match (imp, q) {
            (_, Query::Floats(qf)) => self.dot_floats(qf, outer),
            (_, Query::Bytes(qb)) => self.dot_bytes(qb, outer),
            (Impl::Knn | Impl::Binary, Query::List(qf, _)) => {
                // Knn loops over the document, Binary over the query
                let n = if imp == Impl::Knn { self.f.len() } else { qf.len() };
                let mut r = 0f64;
                for i in 0..n {
                    let (Some(d), Some(x)) = (self.f.get(i), qf.get(i)) else {
                        return Err(list_index(i, qf.len(), self.f.len(), true, imp.frame("dotProduct", if imp == Impl::Knn { 61 } else { 59 }), outer));
                    };
                    r += (d * x) as f64;
                }
                Ok(r)
            }
            (Impl::ByteKnn | Impl::ByteBinary, Query::List(_, qi)) => {
                let n = if imp == Impl::ByteKnn { self.b.len() } else { qi.len() };
                let mut r: i32 = 0;
                for i in 0..n {
                    let (Some(&d), Some(&x)) = (self.b.get(i), qi.get(i)) else {
                        return Err(list_index(i, qi.len(), self.b.len(), true, imp.frame("dotProduct", if imp == Impl::ByteKnn { 68 } else { 72 }), outer));
                    };
                    r = r.wrapping_add((d as i32).wrapping_mul(x));
                }
                Ok(r as f64)
            }
            (Impl::BitKnn | Impl::BitBinary, Query::List(..)) => {
                Err(uoe("dotProduct is not supported for bit vectors.", chain(vec![imp.frame("dotProduct", if imp == Impl::BitKnn { 82 } else { 75 })], &outer())))
            }
        }
    }

    /// `dotProduct(float[])`.
    fn dot_floats(&self, qf: &[f32], outer: &dyn Fn() -> Vec<String>) -> Result<f64, Thrown> {
        let imp = self.imp;
        match imp {
            Impl::Knn | Impl::Binary => {
                if qf.len() != self.f.len() {
                    let line = if imp == Impl::Knn { 54 } else { 52 };
                    return Err(iae(
                        format!("vector dimensions incompatible: {}!= {}", self.f.len(), qf.len()),
                        chain(vec![simdvec("ESVectorUtil.dotProduct(ESVectorUtil.java:91)"), imp.frame("dotProduct", line)], &outer()),
                    ));
                }
                Ok(dot_f32(self.f, qf) as f64)
            }
            Impl::ByteKnn | Impl::ByteBinary => Ok(ip_float_byte(qf, self.b, || chain(vec![imp.frame("dotProduct", if imp == Impl::ByteKnn { 60 } else { 65 })], &outer()))? as f64),
            Impl::BitKnn | Impl::BitBinary => Ok(ip_float_bit(qf, self.b, || chain(vec![imp.frame("dotProduct", if imp == Impl::BitKnn { 97 } else { 90 })], &outer()))? as f64),
        }
    }

    /// `dotProduct(byte[])`.
    fn dot_bytes(&self, qb: &[i8], outer: &dyn Fn() -> Vec<String>) -> Result<f64, Thrown> {
        let imp = self.imp;
        match imp {
            Impl::Knn | Impl::Binary => Err(uoe("use [double dotProduct(float[] queryVector)] instead", chain(vec![imp.frame("dotProduct", if imp == Impl::Knn { 49 } else { 47 })], &outer()))),
            Impl::ByteKnn | Impl::ByteBinary => Ok(dot_bytes(qb, self.b, || chain(vec![imp.frame("dotProduct", if imp == Impl::ByteKnn { 55 } else { 60 })], &outer()))? as f64),
            Impl::BitKnn | Impl::BitBinary => {
                let knn = imp == Impl::BitKnn;
                if qb.len() == self.b.len() {
                    Ok(and_bit_count(self.b, qb, || chain(vec![imp.frame("dotProduct", if knn { 70 } else { 63 })], &outer()))? as f64)
                } else {
                    Ok(ip_byte_bit(qb, self.b, || chain(vec![imp.frame("dotProduct", if knn { 72 } else { 65 })], &outer()))? as f64)
                }
            }
        }
    }

    /// `l1Norm(byte[] | float[] | List)`.
    fn l1(&self, q: &Query, outer: &dyn Fn() -> Vec<String>) -> Result<f64, Thrown> {
        let imp = self.imp;
        match (imp, q) {
            (Impl::Knn | Impl::Binary, Query::Bytes(_)) => {
                Err(uoe("use [double l1Norm(float[] queryVector)] instead", chain(vec![imp.frame("l1Norm", if imp == Impl::Knn { 68 } else { 66 })], &outer())))
            }
            (Impl::Knn | Impl::Binary, Query::Floats(qf) | Query::List(qf, _)) => {
                let floats = matches!(q, Query::Floats(_));
                let knn = imp == Impl::Knn;
                let n = if knn { self.f.len() } else { qf.len() };
                let mut r = 0f64;
                for i in 0..n {
                    let (Some(d), Some(x)) = (self.f.get(i), qf.get(i)) else {
                        let line = match (knn, floats) {
                            (true, true) => 75,
                            (true, false) => 84,
                            (false, true) => 73,
                            (false, false) => 82,
                        };
                        return Err(list_index(i, qf.len(), self.f.len(), !floats, imp.frame("l1Norm", line), outer));
                    };
                    r += (d - x).abs() as f64;
                }
                Ok(r)
            }
            (Impl::ByteKnn | Impl::ByteBinary, Query::Floats(_)) => {
                Err(uoe("use [int l1Norm(byte[] queryVector)] instead", chain(vec![imp.frame("l1Norm", if imp == Impl::ByteKnn { 92 } else { 93 })], &outer())))
            }
            (Impl::ByteKnn | Impl::ByteBinary, Query::Bytes(qb)) => {
                let knn = imp == Impl::ByteKnn;
                let n = if knn { self.b.len() } else { qb.len() };
                let mut r: i32 = 0;
                for i in 0..n {
                    let (Some(&d), Some(&x)) = (self.b.get(i), qb.get(i)) else {
                        return Err(list_index(i, qb.len(), self.b.len(), false, imp.frame("l1Norm", if knn { 84 } else { 86 }), outer));
                    };
                    r += (d as i32 - x as i32).abs();
                }
                Ok(r as f64)
            }
            (Impl::ByteKnn | Impl::ByteBinary, Query::List(_, qi)) => {
                let knn = imp == Impl::ByteKnn;
                let n = if knn { self.b.len() } else { qi.len() };
                let mut r: i32 = 0;
                for i in 0..n {
                    let (Some(&d), Some(&x)) = (self.b.get(i), qi.get(i)) else {
                        return Err(list_index(i, qi.len(), self.b.len(), true, imp.frame("l1Norm", 100), outer));
                    };
                    r = r.wrapping_add((d as i32).wrapping_sub(x).wrapping_abs());
                }
                Ok(r as f64)
            }
            (Impl::BitKnn | Impl::BitBinary, Query::Floats(_)) => {
                let base = imp.byte_base();
                Err(uoe("use [int l1Norm(byte[] queryVector)] instead", chain(vec![base.frame("l1Norm", if base == Impl::ByteKnn { 92 } else { 93 })], &outer())))
            }
            (Impl::BitKnn | Impl::BitBinary, _) => {
                let line = match (imp == Impl::BitKnn, matches!(q, Query::Bytes(_))) {
                    (true, true) => 48,
                    (true, false) => 53,
                    (false, true) => 41,
                    (false, false) => 46,
                };
                Ok(self.hamming(q, &|| chain(vec![imp.frame("l1Norm", line)], &outer()))? as f64)
            }
        }
    }

    /// `hamming(byte[] | List)` (byte and bit vectors).
    fn hamming(&self, q: &Query, outer: &dyn Fn() -> Vec<String>) -> Result<i32, Thrown> {
        let base = self.imp.byte_base();
        let knn = base == Impl::ByteKnn;
        match q {
            Query::Bytes(qb) => xor_bit_count(qb, self.b, || chain(vec![base.frame("hamming", if knn { 108 } else { 107 })], &outer())),
            Query::List(_, qi) => {
                let mut r: i32 = 0;
                for (i, &x) in qi.iter().enumerate() {
                    let Some(&d) = self.b.get(i) else {
                        return Err(aioobe(i, self.b.len(), chain(vec![base.frame("hamming", if knn { 115 } else { 114 })], &outer())));
                    };
                    r += ((x ^ d as i32) & 0xFF).count_ones() as i32;
                }
                Ok(r)
            }
            Query::Floats(_) => Err(uoe("hamming distance is only supported for byte or bit vectors", outer())),
        }
    }

    /// `l2Norm(byte[] | float[] | List)`.
    fn l2(&self, q: &Query, outer: &dyn Fn() -> Vec<String>) -> Result<f64, Thrown> {
        let imp = self.imp;
        match (imp, q) {
            (Impl::Knn | Impl::Binary, Query::Bytes(_)) => {
                Err(uoe("use [double l2Norm(float[] queryVector)] instead", chain(vec![imp.frame("l2Norm", if imp == Impl::Knn { 101 } else { 99 })], &outer())))
            }
            (Impl::Knn | Impl::Binary, Query::Floats(qf)) => {
                if qf.len() != self.f.len() {
                    let (a, b) = if imp == Impl::Knn { (self.f.len(), qf.len()) } else { (qf.len(), self.f.len()) };
                    return Err(iae(
                        format!("vector dimensions incompatible: {a}!= {b}"),
                        chain(vec![simdvec("ESVectorUtil.squareDistance(ESVectorUtil.java:146)"), imp.frame("l2Norm", if imp == Impl::Knn { 106 } else { 104 })], &outer()),
                    ));
                }
                Ok((square_distance_f32(self.f, qf) as f64).sqrt())
            }
            (Impl::Knn | Impl::Binary, Query::List(qf, _)) => {
                let knn = imp == Impl::Knn;
                let n = if knn { self.f.len() } else { qf.len() };
                let mut r = 0f64;
                for i in 0..n {
                    let (Some(d), Some(x)) = (self.f.get(i), qf.get(i)) else {
                        return Err(list_index(i, qf.len(), self.f.len(), true, imp.frame("l2Norm", if knn { 113 } else { 111 }), outer));
                    };
                    let diff = (d - x) as f64;
                    r += diff * diff;
                }
                Ok(r.sqrt())
            }
            (Impl::ByteKnn | Impl::ByteBinary, Query::Floats(_)) => {
                Err(uoe("use [double l2Norm(byte[] queryVector)] instead", chain(vec![imp.frame("l2Norm", if imp == Impl::ByteKnn { 127 } else { 126 })], &outer())))
            }
            (Impl::ByteKnn | Impl::ByteBinary, Query::Bytes(qb)) => {
                let knn = imp == Impl::ByteKnn;
                let frames = || chain(vec![imp.frame("l2Norm", if knn { 122 } else { 121 })], &outer());
                let d = if knn { square_distance_bytes(self.b, qb, frames)? } else { square_distance_bytes(qb, self.b, frames)? };
                Ok((d as f64).sqrt())
            }
            (Impl::ByteKnn | Impl::ByteBinary, Query::List(_, qi)) => {
                let knn = imp == Impl::ByteKnn;
                let n = if knn { self.b.len() } else { qi.len() };
                let mut r: i32 = 0;
                for i in 0..n {
                    let (Some(&d), Some(&x)) = (self.b.get(i), qi.get(i)) else {
                        return Err(list_index(i, qi.len(), self.b.len(), true, imp.frame("l2Norm", if knn { 135 } else { 133 }), outer));
                    };
                    let diff = (d as i32).wrapping_sub(x);
                    r = r.wrapping_add(diff.wrapping_mul(diff));
                }
                Ok((r as f64).sqrt())
            }
            (Impl::BitKnn | Impl::BitBinary, Query::Floats(_)) => {
                let base = imp.byte_base();
                Err(uoe("use [double l2Norm(byte[] queryVector)] instead", chain(vec![base.frame("l2Norm", if base == Impl::ByteKnn { 127 } else { 126 })], &outer())))
            }
            (Impl::BitKnn | Impl::BitBinary, _) => {
                let line = match (imp == Impl::BitKnn, matches!(q, Query::Bytes(_))) {
                    (true, true) => 58,
                    (true, false) => 63,
                    (false, true) => 51,
                    (false, false) => 56,
                };
                Ok((self.hamming(q, &|| chain(vec![imp.frame("l2Norm", line)], &outer()))? as f64).sqrt())
            }
        }
    }

    /// `cosineSimilarity(float[], boolean normalize)`.
    fn cos_floats(&self, qf: &[f32], normalize: bool, outer: &dyn Fn() -> Vec<String>) -> Result<f64, Thrown> {
        let imp = self.imp;
        match imp {
            Impl::BitKnn | Impl::BitBinary => Err(uoe("cosineSimilarity is not supported for bit vectors.", chain(vec![imp.frame("cosineSimilarity", if imp == Impl::BitKnn { 77 } else { 70 })], &outer()))),
            _ => {
                let line = match (imp, normalize) {
                    (Impl::Knn, true) => 127,
                    (Impl::Knn, false) => 130,
                    (Impl::Binary, true) => 125,
                    (Impl::Binary, false) => 127,
                    (Impl::ByteKnn, true) => 150,
                    (Impl::ByteKnn, false) => 153,
                    (_, true) => 147,
                    (_, false) => 150,
                };
                let dot = self.dot_floats(qf, &|| chain(vec![imp.frame("cosineSimilarity", line)], &outer()))?;
                if normalize {
                    Ok(dot / (magnitude_f32(qf) * self.magnitude()) as f64)
                } else {
                    Ok(dot / self.magnitude() as f64)
                }
            }
        }
    }

    /// `cosineSimilarity(byte[], float qvMagnitude)`.
    fn cos_bytes(&self, qb: &[i8], qv_magnitude: f32, outer: &dyn Fn() -> Vec<String>) -> Result<f64, Thrown> {
        let imp = self.imp;
        match imp {
            Impl::Knn | Impl::Binary => Err(uoe(
                "use [double cosineSimilarity(float[] queryVector, boolean normalizeQueryVector)] instead",
                chain(vec![imp.frame("cosineSimilarity", if imp == Impl::Knn { 121 } else { 119 })], &outer()),
            )),
            Impl::BitKnn | Impl::BitBinary => Err(uoe("cosineSimilarity is not supported for bit vectors.", chain(vec![imp.frame("cosineSimilarity", if imp == Impl::BitKnn { 87 } else { 80 })], &outer()))),
            _ => {
                let dot = self.dot_bytes(qb, &|| chain(vec![imp.frame("cosineSimilarity", if imp == Impl::ByteKnn { 144 } else { 141 })], &outer()))?;
                Ok(dot / (qv_magnitude * self.magnitude()) as f64)
            }
        }
    }

    /// `cosineSimilarity(List)`.
    fn cos_list(&self, q: &Query, outer: &dyn Fn() -> Vec<String>) -> Result<f64, Thrown> {
        let imp = self.imp;
        let Query::List(qf, _) = q else { unreachable!() };
        match imp {
            Impl::BitKnn | Impl::BitBinary => Err(uoe("cosineSimilarity is not supported for bit vectors.", chain(vec![imp.frame("cosineSimilarity", if imp == Impl::BitKnn { 92 } else { 85 })], &outer()))),
            _ => {
                let line = match imp {
                    Impl::Knn => 135,
                    Impl::Binary => 132,
                    Impl::ByteKnn => 158,
                    _ => 155,
                };
                let dot = self.dot(q, &|| chain(vec![imp.frame("cosineSimilarity", line)], &outer()))?;
                Ok(dot / (magnitude_list(qf) * self.magnitude()) as f64)
            }
        }
    }

    /// `DenseVector.checkDimensions(int)` (bit vectors: their override).
    fn check_dims(&self, qv: usize, outer: &dyn Fn() -> Vec<String>) -> Result<(), Thrown> {
        match self.imp {
            Impl::BitKnn | Impl::BitBinary => {
                if qv != self.b.len() {
                    let line = if self.imp == Impl::BitKnn { 27 } else { 29 };
                    return Err(iae(
                        format!("The query vector has a different number of dimensions [{}] than the document vectors [{}].", qv * 8, self.b.len() * 8),
                        chain(vec![self.imp.frame("checkDimensions", line)], &outer()),
                    ));
                }
                Ok(())
            }
            _ => {
                let dims = self.dims();
                if qv != dims {
                    return Err(iae(
                        format!("The query vector has a different number of dimensions [{qv}] than the document vectors [{dims}]."),
                        chain(vec![srv("script.field.vectors.DenseVector.checkDimensions(DenseVector.java:222)"), srv("script.field.vectors.DenseVector.checkDimensions(DenseVector.java:31)")], &outer()),
                    ));
                }
                Ok(())
            }
        }
    }
}

/// An index past one of the two arrays a Java loop walks (`List.get` past the query's end, or
/// the document's array).
fn list_index(i: usize, qlen: usize, dlen: usize, query_is_list: bool, frame: String, outer: &dyn Fn() -> Vec<String>) -> Thrown {
    if i >= qlen && !query_is_list {
        return aioobe(i, qlen, chain(vec![frame], &outer()));
    }
    if i >= qlen {
        // List.get
        let mut t = throw(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, format!("Index {i} out of bounds for length {qlen}"), Vec::new());
        t.frames = chain(
            vec![
                "java.base/jdk.internal.util.Preconditions.outOfBounds(Preconditions.java:100)".into(),
                "java.base/jdk.internal.util.Preconditions.outOfBoundsCheckIndex(Preconditions.java:106)".into(),
                "java.base/jdk.internal.util.Preconditions.checkIndex(Preconditions.java:302)".into(),
                "java.base/java.util.Objects.checkIndex(Objects.java:385)".into(),
                "java.base/java.util.ArrayList.get(ArrayList.java:427)".into(),
                frame,
            ],
            &outer(),
        );
        return t;
    }
    aioobe(i, dlen, chain(vec![frame], &outer()))
}

// ------------------------------------------------------------------------------ query vectors

/// The numbers of a `List` query (`floatValue()`, `intValue()`).
fn list_numbers(vm: &mut Vm, v: &Value) -> Result<(Vec<f32>, Vec<i32>), Thrown> {
    let items = crate::lib_coll::elements(vm, v)?;
    numbers(items)
}

/// The size of a `List` query (checked before its elements are read as numbers).
fn list_len(vm: &mut Vm, v: &Value) -> Result<usize, Thrown> {
    Ok(crate::lib_coll::elements(vm, v)?.len())
}

fn numbers(items: Vec<Value>) -> Result<(Vec<f32>, Vec<i32>), Thrown> {
    let mut f = Vec::with_capacity(items.len());
    let mut n = Vec::with_capacity(items.len());
    for x in items {
        let (fv, iv) = match x {
            Value::Byte(b) => (b as f32, b as i32),
            Value::Short(b) => (b as f32, b as i32),
            Value::Int(b) => (b as f32, b),
            Value::Long(b) => (b as f32, b as i32),
            Value::Float(b) => (b, b as i32),
            Value::Double(b) => (b as f32, b as i32),
            ref other => match other.as_f64() {
                Some(d) if matches!(other, Value::Obj(_)) => (d as f32, d as i32),
                _ => return Err(crate::vm::cce(crate::cast::checkcast_message(crate::lookup::java_binary_name(other.class()), "java.lang.Number"))),
            },
        };
        f.push(fv);
        n.push(iv);
    }
    Ok((f, n))
}

/// The query of `DenseVector.<op>(Object)`: `float[]`, `List` or `byte[]`.
/// A query of `DenseVector.<op>(Object)`, with a list's elements still unread.
type ObjectQuery = (Query, Option<Vec<Value>>);

/// A list query's elements are read (as numbers) only after its size passed the dimension
/// check.
fn object_query(vm: &mut Vm, v: &Value) -> Result<Option<ObjectQuery>, Thrown> {
    if let Value::Obj(o) = v
        && let Obj::Array(a) = &**o
    {
        match &*a.data.borrow() {
            ArrayData::Float(f) => return Ok(Some((Query::Floats(f.clone()), None))),
            ArrayData::Byte(b) => return Ok(Some((Query::Bytes(b.clone()), None))),
            _ => {}
        }
    }
    if crate::dynamic::is_list(vm.reg, v) {
        let items = crate::lib_coll::elements(vm, v)?;
        let n = items.len();
        return Ok(Some((Query::List(vec![0.0; n], vec![0; n]), Some(items))));
    }
    Ok(None)
}

fn bad_query(v: &Value, method: &str, line: u32) -> Thrown {
    let cname = crate::lookup::java_binary_name(v.class());
    super::es_iae(format!("Cannot use vector [{}] with class [{cname}] as query vector", v.to_java_string()), &[&format!("script.field.vectors.DenseVector.{method}(DenseVector.java:{line})")])
}

/// `HexFormat.of().parseHex(s)` of a byte or bit query.
fn parse_hex(s: &str) -> Result<Vec<i8>, Thrown> {
    Ok(crate::lib_util::parse_hex(s)?.into_iter().map(|b| b as i8).collect())
}

// ------------------------------------------------------------------------------ DenseVector methods

fn vector_of(v: &Value) -> Result<Option<Vector>, Thrown> {
    match es(v) {
        Some(EsObj::Vector(x)) => Ok(x.clone()),
        _ => Err(crate::vm::cce("not a vector")),
    }
}

fn missing(method: &str, line: u32) -> Thrown {
    super::es_iae(
        "Dense vector value missing for a field, use isEmpty() to check for a missing vector value",
        &[&format!("script.field.vectors.DenseVector$1.{method}(DenseVector.java:{line})")],
    )
}

pub fn v_magnitude(_: &mut Vm, a: &mut [Value]) -> VR {
    match vector_of(&a[0])? {
        Some(v) => Ok(Value::Float(v.magnitude())),
        None => Err(missing("getMagnitude", 238)),
    }
}

pub fn v_get_vector(_: &mut Vm, a: &mut [Value]) -> VR {
    match vector_of(&a[0])? {
        Some(v) => Ok(v.to_array()),
        None => Err(missing("getVector", 323)),
    }
}

pub fn v_is_empty(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(vector_of(&a[0])?.is_none()))
}

pub fn v_dims(_: &mut Vm, a: &mut [Value]) -> VR {
    match vector_of(&a[0])? {
        Some(v) => Ok(Value::Int(v.dims() as i32)),
        None => Err(missing("getDims", 333)),
    }
}

pub fn v_size(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(i32::from(vector_of(&a[0])?.is_some())))
}

pub fn v_empty(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(es_value(EsObj::Vector(None)))
}

#[derive(Clone, Copy)]
enum VOp {
    Dot,
    L1,
    L2,
    Cos,
}

/// `DenseVector.dotProduct/l1Norm/l2Norm/cosineSimilarity(Object)`: the overload by the
/// argument's class, after `checkDimensions`.
fn vector_op(vm: &mut Vm, a: &[Value], op: VOp) -> VR {
    let v = vector_of(&a[0])?;
    let (name, floats_line, list_line, bytes_line, bad_line) = match op {
        VOp::Dot => ("dotProduct", 47, 50, 53, 57),
        VOp::L1 => ("l1Norm", 69, 72, 75, 79),
        VOp::L2 => ("l2Norm", 109, 112, 115, 119),
        VOp::Cos => ("cosineSimilarity", 159, 162, 165, 169),
    };
    let Some((mut q, items)) = object_query(vm, &a[1])? else { return Err(bad_query(&a[1], name, bad_line)) };
    let line = match q {
        Query::Floats(_) => floats_line,
        Query::List(..) => list_line,
        Query::Bytes(_) => bytes_line,
    };
    let at = vec![srv(&format!("script.field.vectors.DenseVector.{name}(DenseVector.java:{line})"))];
    let Some(v) = v else {
        // EMPTY.getDims() inside checkDimensions
        return Err(super::es_iae(
            "Dense vector value missing for a field, use isEmpty() to check for a missing vector value",
            &["script.field.vectors.DenseVector$1.getDims(DenseVector.java:333)", "script.field.vectors.DenseVector.checkDimensions(DenseVector.java:31)", &format!("script.field.vectors.DenseVector.{name}(DenseVector.java:{line})")],
        ));
    };
    let dv = v.view();
    dv.check_dims(q.len(), &|| at.clone())?;
    if let Some(items) = items {
        let (f, i) = numbers(items)?;
        q = Query::List(f, i);
    }
    // the typed overload is called on the next line of the dispatch
    let call = vec![srv(&format!("script.field.vectors.DenseVector.{name}(DenseVector.java:{})", line + 1))];
    let r = match op {
        VOp::Dot => dv.dot(&q, &|| call.clone())?,
        VOp::L1 => dv.l1(&q, &|| call.clone())?,
        VOp::L2 => dv.l2(&q, &|| call.clone())?,
        VOp::Cos => match &q {
            // DenseVector.cosineSimilarity(float[]) → cosineSimilarity(q, true)
            Query::Floats(qf) => dv.cos_floats(qf, true, &|| chain(vec![srv("script.field.vectors.DenseVector.cosineSimilarity(DenseVector.java:139)")], &call))?,
            // DenseVector.cosineSimilarity(byte[]) → cosineSimilarity(q, getMagnitude(q))
            Query::Bytes(qb) => {
                let m = (dot_bytes(qb, qb, Vec::new).unwrap_or(0.0) as f64).sqrt() as f32;
                dv.cos_bytes(qb, m, &|| chain(vec![srv("script.field.vectors.DenseVector.cosineSimilarity(DenseVector.java:126)")], &call))?
            }
            Query::List(..) => dv.cos_list(&q, &|| call.clone())?,
        },
    };
    Ok(Value::Double(r))
}

pub fn v_dot(vm: &mut Vm, a: &mut [Value]) -> VR {
    vector_op(vm, a, VOp::Dot)
}
pub fn v_l1(vm: &mut Vm, a: &mut [Value]) -> VR {
    vector_op(vm, a, VOp::L1)
}
pub fn v_l2(vm: &mut Vm, a: &mut [Value]) -> VR {
    vector_op(vm, a, VOp::L2)
}
pub fn v_cos(vm: &mut Vm, a: &mut [Value]) -> VR {
    vector_op(vm, a, VOp::Cos)
}

// ------------------------------------------------------------------------------ score functions

/// A score function as its Java constructor left it.
struct VectorFn {
    field: Rc<str>,
    /// The parsed query (a float query of a byte field is kept as floats).
    query: Query,
    /// `qvMagnitude` of a byte query (cosine).
    qv_magnitude: f32,
    /// The VSSU frames of the per-document call (function class, then the public wrapper).
    call: Vec<String>,
}

fn vssu(f: &str) -> String {
    srv(&format!("script.VectorScoreScriptUtils${f}"))
}

/// (function class, its public method, line of `return function.x()`, wrapper constructor
/// line of the field cast, the score method's name in the inner class)
fn names(op: VFn) -> (&'static str, &'static str, u32, u32) {
    match op {
        VFn::Dot => ("DotProduct", "dotProduct", 491, 461),
        VFn::Cos => ("CosineSimilarity", "cosineSimilarity", 560, 539),
        VFn::L1 => ("L1Norm", "l1norm", 223, 202),
        VFn::L2 => ("L2Norm", "l2norm", 333, 312),
        VFn::Hamming => ("Hamming", "hamming", 269, 254),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VFn {
    Dot,
    Cos,
    L1,
    L2,
    Hamming,
}

/// `Element.checkVectorErrors` of a byte element: `None` when every value is a byte.
pub(super) fn byte_errors(v: &[f32], element: &str) -> Option<String> {
    if let Some(e) = nan_errors(v, element) {
        return Some(e);
    }
    for (i, &x) in v.iter().enumerate() {
        if x % 1.0 != 0.0 {
            return Some(format!("element_type [{element}] vectors only support non-decimal values but found decimal value [{}] at dim [{i}];", crate::java::fmt_float(x)));
        }
        if !(-128.0..=127.0).contains(&x) {
            return Some(format!("element_type [{element}] vectors only support integers between [-128, 127] but found [{}] at dim [{i}];", crate::java::fmt_float(x)));
        }
    }
    None
}

/// `Element.checkNanAndInfinite`.
pub(super) fn nan_errors(v: &[f32], element: &str) -> Option<String> {
    for (i, &x) in v.iter().enumerate() {
        if x.is_nan() {
            return Some(format!("element_type [{element}] vectors do not support NaN values but found [NaN] at dim [{i}];"));
        }
        if x.is_infinite() {
            return Some(format!("element_type [{element}] vectors do not support infinite values but found [{}] at dim [{i}];", crate::java::fmt_float(x)));
        }
    }
    None
}

/// `FloatElement.appendErrorElements`: the first five values.
pub(super) fn preview(mut msg: String, v: &[f32]) -> String {
    msg.push_str(" Preview of invalid vector: [");
    for (i, x) in v.iter().take(5).enumerate() {
        if i > 0 {
            msg.push_str(", ");
        }
        msg.push_str(&crate::java::fmt_float(*x));
    }
    if v.len() >= 5 {
        msg.push_str(", ...");
    }
    msg.push(']');
    msg
}

fn element_name(e: VectorElement) -> &'static str {
    match e {
        VectorElement::Float => "float",
        VectorElement::BFloat16 => "bfloat16",
        VectorElement::Byte => "byte",
        VectorElement::Bit => "bit",
    }
}

/// The field's vector of the document the binding is built on (`field.get()`).
fn first_vector(vm: &mut Vm, field: &str) -> Result<(Option<Vector>, VectorElement), Thrown> {
    let mut dv = DocValues::default();
    if vm.st.has_doc {
        match vm.host.field_values(field, &mut dv) {
            Ok(true) => {}
            Ok(false) => dv.clear(DocKind::DenseVector),
            Err(e) => return Err(super::doc::host_error(e)),
        }
    }
    Ok((Vector::from_doc(&dv), dv.element))
}

fn build(vm: &mut Vm, a: &[Value], op: VFn) -> Result<VectorFn, Thrown> {
    let field_name = a[1].to_java_string();
    let f = super::doc::field_of(vm, &field_name)?;
    let (outer, outer_name, ret_line, cast_line) = names(op);
    if f.class != cls::DENSE_VECTOR_DOC_VALUES_FIELD {
        let from = crate::lookup::java_binary_name(f.class);
        return Err(es_error(
            cls::CLASS_CAST_EXCEPTION,
            format!(
                "class {from} cannot be cast to class org.elasticsearch.script.field.vectors.DenseVectorDocValuesField ({from} and org.elasticsearch.script.field.vectors.DenseVectorDocValuesField are in module org.elasticsearch.server@9.5.5 of loader 'app')"
            ),
            &[&format!("script.VectorScoreScriptUtils${outer}.<init>(VectorScoreScriptUtils.java:{cast_line})")],
        ));
    }
    let (vector, element) = first_vector(vm, &field_name)?;
    let float_field = matches!(element, VectorElement::Float | VectorElement::BFloat16);
    let wrapper = |line: u32| vssu(&format!("{outer}.<init>(VectorScoreScriptUtils.java:{line})"));
    let unsupported = |kind: &str, line: u32| -> Thrown {
        let cname = crate::lookup::java_binary_name(a[0].class());
        iae(format!("Unsupported input object for {kind} vectors: {cname}"), vec![wrapper(line)])
    };
    let is_list = crate::dynamic::is_list(vm.reg, &a[0]);
    let hex = match &a[0] {
        Value::Str(s) if !is_list => Some(s.as_str().to_string()),
        _ => None,
    };
    // (inner constructor, its line, wrapper line) per input kind
    if op == VFn::Hamming && float_field {
        return Err(iae("hamming distance is only supported for byte or bit vectors", vec![wrapper(256)]));
    }
    let missing_dims = |inner: Vec<String>| -> Thrown {
        iae("Dense vector value missing for a field, use isEmpty() to check for a missing vector value", chain(vec![srv("script.field.vectors.DenseVector$1.getDims(DenseVector.java:333)")], &inner))
    };
    let per_doc = |inner: &str, inner_line: u32| -> Vec<String> {
        vec![vssu(&format!("{inner}.{outer_name}(VectorScoreScriptUtils.java:{inner_line})")), vssu(&format!("{outer}.{outer_name}(VectorScoreScriptUtils.java:{ret_line})"))]
    };
    let field: Rc<str> = Rc::from(field_name.as_str());

    // bit vectors: dotProduct has its own function
    if op == VFn::Dot && element == VectorElement::Bit {
        let (inner, wline) = if is_list { (386, 465) } else { (351, 468) };
        let frames = vec![vssu(&format!("BitDotProduct.<init>(VectorScoreScriptUtils.java:{inner})")), wrapper(wline)];
        let query = if is_list {
            let (qf, qi) = list_numbers(vm, &a[0])?;
            if qf.is_empty() {
                // queryVector.isEmpty(): checked in the List constructor before anything else
            }
            let is_float = qf.iter().any(|&x| x % 1.0 != 0.0 || !(-128.0..=127.0).contains(&x));
            let Some(v) = &vector else { return Err(missing_dims(frames)) };
            let dims = v.dims();
            if is_float {
                if dims != qf.len() {
                    return Err(iae(
                        format!("The query vector has an incorrect number of dimensions. Must be [{dims}] for float wise operations: provided [{}].", qf.len()),
                        vec![vssu("BitDotProduct.<init>(VectorScoreScriptUtils.java:391)"), wrapper(wline)],
                    ));
                }
                Query::Floats(qf)
            } else {
                let qb: Vec<i8> = qi.iter().map(|&x| x as i8).collect();
                if dims != qb.len() * 8 && dims != qb.len() {
                    return Err(iae(
                        format!("The query vector has an incorrect number of dimensions. Must be [{}] for bitwise operations, or [{dims}] for byte wise operations: provided [{}].", dims / 8, qb.len()),
                        vec![vssu("BitDotProduct.<init>(VectorScoreScriptUtils.java:403)"), wrapper(wline)],
                    ));
                }
                Query::Bytes(qb)
            }
        } else if let Some(s) = &hex {
            let qb = parse_hex(s)?;
            let Some(v) = &vector else { return Err(missing_dims(frames)) };
            let dims = v.dims();
            if dims != qb.len() * 8 && dims != qb.len() {
                return Err(iae(
                    format!("The query vector has an incorrect number of dimensions. Must be [{}] for bitwise operations, or [{dims}] for byte wise operations: provided [{}].", dims / 8, qb.len()),
                    vec![vssu("BitDotProduct.<init>(VectorScoreScriptUtils.java:353)"), wrapper(wline)],
                ));
            }
            Query::Bytes(qb)
        } else {
            return Err(unsupported("bit", 470));
        };
        return Ok(VectorFn { field, query, qv_magnitude: 0.0, call: per_doc("BitDotProduct", 418) });
    }

    if float_field {
        let (inner, inner_line, wline, bad) = match op {
            VFn::Dot => ("FloatDotProduct", 446, 483, 485),
            VFn::Cos => ("FloatCosineSimilarity", 524, 552, 554),
            VFn::L1 => ("FloatL1Norm", 187, 215, 217),
            _ => ("FloatL2Norm", 297, 325, 327),
        };
        if !is_list {
            return Err(unsupported("float", bad));
        }
        let ctor = vec![vssu("FloatDenseVectorFunction.<init>(VectorScoreScriptUtils.java:143)"), vssu(&format!("{inner}.<init>(VectorScoreScriptUtils.java:{inner_line})")), wrapper(wline)];
        let n = list_len(vm, &a[0])?;
        let Some(v) = &vector else { return Err(missing_dims(ctor[..].to_vec())) };
        if v.dims() != n {
            return Err(iae(
                format!("The query vector has a different number of dimensions [{n}] than the document vectors [{}].", v.dims()),
                chain(vec![srv("script.field.vectors.DenseVector.checkDimensions(DenseVector.java:222)")], &ctor),
            ));
        }
        let (mut qf, _) = list_numbers(vm, &a[0])?;
        let mut qmag = 0f64;
        for x in &qf {
            qmag += (x * x) as f64;
        }
        if let Some(e) = nan_errors(&qf, element_name(element)) {
            let mut frames = vec![srv("index.mapper.vectors.DenseVectorFieldMapper$Element.checkVectorBounds(DenseVectorFieldMapper.java:693)")];
            frames.push(vssu("FloatDenseVectorFunction.<init>(VectorScoreScriptUtils.java:153)"));
            frames.extend_from_slice(&ctor[1..]);
            return Err(iae(preview(e, &qf), frames));
        }
        if op == VFn::Cos {
            let m = qmag.sqrt() as f32;
            for x in qf.iter_mut() {
                *x /= m;
            }
        }
        let call_line = match op {
            VFn::Dot => 451,
            VFn::Cos => 529,
            VFn::L1 => 192,
            _ => 302,
        };
        return Ok(VectorFn { field, query: Query::Floats(qf), qv_magnitude: 0.0, call: per_doc(inner, call_line) });
    }

    // byte and bit vectors
    let (inner, inner_line, list_w, hex_w, bad) = match op {
        VFn::Dot => ("ByteDotProduct", 426, 474, 477, 479),
        VFn::Cos => ("ByteCosineSimilarity", 503, 543, 546, 548),
        VFn::L1 => ("ByteL1Norm", 171, 206, 209, 211),
        VFn::L2 => ("ByteL2Norm", 281, 316, 319, 321),
        VFn::Hamming => ("ByteHammingDistance", 235, 259, 262, 264),
    };
    let call_line = match op {
        VFn::Dot => 438,
        VFn::Cos => 516,
        VFn::L1 => 180,
        VFn::L2 => 290,
        VFn::Hamming => 244,
    };
    if is_list {
        let ctor = vec![vssu("ByteDenseVectorFunction.<init>(VectorScoreScriptUtils.java:70)"), vssu(&format!("{inner}.<init>(VectorScoreScriptUtils.java:{inner_line})")), wrapper(list_w)];
        let n = list_len(vm, &a[0])?;
        let Some(v) = &vector else { return Err(missing_dims(ctor)) };
        // Element.checkDimensions (a bit element counts eight dimensions per query value)
        let (dvd, qvd) = if element == VectorElement::Bit { (v.dims(), n * 8) } else { (v.dims(), n) };
        if dvd != qvd {
            let mut frames = vec![srv("index.mapper.vectors.DenseVectorFieldMapper$Element.checkDimensions(DenseVectorFieldMapper.java:711)")];
            if element == VectorElement::Bit {
                frames.push(srv("index.mapper.vectors.DenseVectorFieldMapper$BitElement.checkDimensions(DenseVectorFieldMapper.java:1439)"));
            }
            return Err(iae(format!("The query vector has a different number of dimensions [{qvd}] than the document vectors [{dvd}]."), chain(frames, &ctor)));
        }
        let (qf, _) = list_numbers(vm, &a[0])?;
        let mut qmag = 0f64;
        for x in &qf {
            qmag += (x * x) as f64;
        }
        let qmag = qmag.sqrt();
        // Element.checkValidVector(values, BYTE[, FLOAT])
        let allow_float = matches!(op, VFn::Dot | VFn::Cos);
        let as_byte = byte_errors(&qf, "byte");
        let query = match as_byte {
            None => Query::Bytes(qf.iter().map(|&x| x as i8).collect()),
            Some(be) => {
                let fe = if allow_float { nan_errors(&qf, "float") } else { Some(String::new()) };
                match fe {
                    None => {
                        let mut f = qf.clone();
                        if op == VFn::Cos {
                            for x in f.iter_mut() {
                                *x /= qmag as f32;
                            }
                        }
                        Query::Floats(f)
                    }
                    Some(fe) => {
                        let mut msg = format!("Vector is not a byte vector: {be}");
                        if allow_float {
                            msg.push_str(&format!(" Vector is not a float vector: {fe}"));
                        }
                        let frames = chain(
                            vec![srv("index.mapper.vectors.DenseVectorFieldMapper$Element.checkValidVector(DenseVectorFieldMapper.java:662)"), vssu("ByteDenseVectorFunction.<init>(VectorScoreScriptUtils.java:80)")],
                            &ctor[1..],
                        );
                        return Err(iae(preview(msg, &qf), frames));
                    }
                }
            }
        };
        let qv_magnitude = qmag as f32;
        let mut call = per_doc(inner, call_line);
        if op == VFn::Cos && matches!(query, Query::Floats(_)) {
            call[0] = vssu("ByteCosineSimilarity.cosineSimilarity(VectorScoreScriptUtils.java:514)");
        }
        if op == VFn::Dot && matches!(query, Query::Floats(_)) {
            call[0] = vssu("ByteDotProduct.dotProduct(VectorScoreScriptUtils.java:436)");
        }
        return Ok(VectorFn { field, query, qv_magnitude, call });
    }
    if let Some(s) = &hex {
        let _ = hex_w;
        let qb = parse_hex(s)?;
        let m = magnitude_bytes(&qb);
        return Ok(VectorFn { field, query: Query::Bytes(qb), qv_magnitude: m, call: per_doc(inner, call_line) });
    }
    Err(unsupported("byte", bad))
}

fn score(vm: &mut Vm, a: &[Value], op: VFn) -> VR {
    let b = binding(vm, |vm| build(vm, a, op))?;
    let (inner_frames, field) = (&b.call, b.field.clone());
    let missing = || {
        let mut f = vec![vssu("DenseVectorFunction.setNextVector(VectorScoreScriptUtils.java:41)")];
        f.extend_from_slice(inner_frames);
        iae("A document doesn't have a value for a vector field!", f)
    };
    if !vm.st.has_doc {
        return Err(missing());
    }
    let run = |imp: Impl, f: &[f32], bytes: &[i8], magnitude: Option<f32>| -> Result<f64, Thrown> {
        // a stored magnitude (doc-values-only vectors, or the host's cosine fields) is used as is
        let mag = Cell::new(magnitude.unwrap_or(f32::NAN));
        let dv = DocVec { imp, f, b: bytes, magnitude: &mag };
        let inner_frames = &|| inner_frames.clone();
        match op {
            VFn::Dot => dv.dot(&b.query, inner_frames),
            VFn::L1 => dv.l1(&b.query, inner_frames),
            VFn::L2 => dv.l2(&b.query, inner_frames),
            VFn::Hamming => dv.hamming(&b.query, inner_frames).map(|x| x as f64),
            VFn::Cos => match &b.query {
                Query::Floats(qf) => dv.cos_floats(qf, false, inner_frames),
                Query::Bytes(qb) => dv.cos_bytes(qb, b.qv_magnitude, inner_frames),
                Query::List(..) => dv.cos_list(&b.query, inner_frames),
            },
        }
    };
    // the host's own storage first: no copy per document
    let mut lent = None;
    match vm.host.with_vector(&field, &mut |v| {
        lent = Some(run(Impl::of(v.element, v.binary), v.floats, v.bytes, v.magnitude));
    }) {
        Ok(Some(true)) => return lent.unwrap_or_else(|| Err(missing())).map(Value::Double),
        Ok(Some(false)) => return Err(missing()),
        Ok(None) => {}
        Err(e) => return Err(super::doc::host_error(e)),
    }
    let mut buf = std::mem::take(&mut vm.st.vec_buf);
    let read = vm.host.doc_values(&field, &mut buf);
    let out = match read {
        Ok(true) if buf.kind == DocKind::DenseVector && !(buf.vector.is_empty() && buf.vector_bytes.is_empty()) => {
            run(Impl::of(buf.element, buf.vector_binary), &buf.vector, &buf.vector_bytes, buf.magnitude)
        }
        Ok(_) => Err(missing()),
        Err(e) => Err(super::doc::host_error(e)),
    };
    vm.st.vec_buf = buf;
    out.map(Value::Double)
}

pub fn s_cosine(vm: &mut Vm, a: &mut [Value]) -> VR {
    score(vm, a, VFn::Cos)
}
pub fn s_dot(vm: &mut Vm, a: &mut [Value]) -> VR {
    score(vm, a, VFn::Dot)
}
pub fn s_l1(vm: &mut Vm, a: &mut [Value]) -> VR {
    score(vm, a, VFn::L1)
}
pub fn s_l2(vm: &mut Vm, a: &mut [Value]) -> VR {
    score(vm, a, VFn::L2)
}
pub fn s_hamming(vm: &mut Vm, a: &mut [Value]) -> VR {
    score(vm, a, VFn::Hamming)
}

/// Frames of a failure inside a server class, for callers outside this module.
#[allow(dead_code)]
pub fn frames(f: &[&str]) -> Vec<String> {
    server_frames(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_kernels_read_bits_most_significant_first() {
        // [0b10000000, 0b00000001]: dimensions 0 and 15
        let d = [0x80u8 as i8, 0x01];
        let q: Vec<i8> = (1..=16).collect();
        assert_eq!(ip_byte_bit(&q, &d, Vec::new).unwrap(), 1.0 + 16.0);
        let qf: Vec<f32> = (1..=16).map(|x| x as f32 / 2.0).collect();
        assert_eq!(ip_float_bit(&qf, &d, Vec::new).unwrap(), 0.5 + 8.0);
        assert_eq!(and_bit_count(&d, &[-1, -1], Vec::new).unwrap(), 2.0);
        assert_eq!(xor_bit_count(&d, &[0, 0], Vec::new).unwrap(), 2);
        assert_eq!(xor_bit_count(&d, &[0], Vec::new).unwrap_err().message.as_deref(), Some("vector dimensions differ: 2!=1"));
    }

    #[test]
    fn byte_validation_messages_follow_elasticsearch() {
        assert_eq!(byte_errors(&[1.0, 0.5], "byte").unwrap(), "element_type [byte] vectors only support non-decimal values but found decimal value [0.5] at dim [1];");
        assert_eq!(byte_errors(&[300.0], "byte").unwrap(), "element_type [byte] vectors only support integers between [-128, 127] but found [300.0] at dim [0];");
        assert_eq!(byte_errors(&[f32::NAN], "byte").unwrap(), "element_type [byte] vectors do not support NaN values but found [NaN] at dim [0];");
        assert!(byte_errors(&[-128.0, 127.0], "byte").is_none());
        assert_eq!(preview("x".into(), &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), "x Preview of invalid vector: [1.0, 2.0, 3.0, 4.0, 5.0, ...]");
    }

    #[test]
    fn magnitudes_by_element_type() {
        let mut dv = DocValues::default();
        dv.clear(DocKind::DenseVector);
        dv.element = VectorElement::Byte;
        dv.vector_bytes.extend_from_slice(&[3, 4]);
        assert_eq!(Vector::from_doc(&dv).unwrap().magnitude(), 5.0);
        dv.element = VectorElement::Bit;
        assert_eq!(Vector::from_doc(&dv).unwrap().magnitude(), (3f64).sqrt() as f32);
        assert_eq!(Vector::from_doc(&dv).unwrap().dims(), 16);
        dv.clear(DocKind::DenseVector);
        assert!(Vector::from_doc(&dv).is_none());
    }
}
