//! `rank_vectors` fields (multi-vector late interaction) in scripts: the `RankVectors` objects
//! of `field('v').get()` and `doc['v']`, and the score functions `maxSimDotProduct` and
//! `maxSimInvHamming` (`RankVectorsScoreScriptUtils`).
//!
//! A document's vectors arrive flattened in [`DocValues::vector`] (`float`, `bfloat16`) or
//! [`DocValues::vector_bytes`] (`byte`, `bit`: packed bits) with [`DocValues::vector_count`]
//! vectors of equal length. The score functions are class bindings: the query vectors are
//! parsed and checked once per call site against the first document, then each document is
//! read into the instance's reusable buffer.
//!
//! MaxSim sums, over the query vectors, the best similarity of each against the document's
//! vectors: the dot product (`float` and `byte` elements; for `bit` elements the bits mask a
//! byte or float query, or AND with a packed-bit query of the same length), or the inverse
//! Hamming distance `(bits - xorBitCount) / bits` (`byte` and `bit`).

use std::rc::Rc;

use super::{binding, es, es_error, es_iae, es_value, EsObj};
use crate::host::{DocKind, DocValues, VectorElement};
use crate::types::cls;
use crate::value::{ArrayData, JArray, Obj, Value};
use crate::vm::{Thrown, Vm, VR};

/// A document's vectors.
pub struct RankVec {
    pub element: VectorElement,
    /// The values as floats (byte and bit vectors widened), vector after vector.
    pub f: Rc<[f32]>,
    /// The values of `byte` and `bit` vectors.
    pub b: Rc<[i8]>,
    pub count: usize,
    pub magnitudes: Rc<[f32]>,
}

impl RankVec {
    pub fn from_doc(dv: &DocValues) -> Option<Rc<RankVec>> {
        if dv.kind != DocKind::RankVectors || dv.vector_count == 0 {
            return None;
        }
        let bytes = matches!(dv.element, VectorElement::Byte | VectorElement::Bit);
        let (f, b): (Rc<[f32]>, Rc<[i8]>) =
            if bytes { (dv.vector_bytes.iter().map(|&x| x as f32).collect(), Rc::from(dv.vector_bytes.as_slice())) } else { (Rc::from(dv.vector.as_slice()), Rc::from(Vec::new())) };
        let count = dv.vector_count;
        let magnitudes: Rc<[f32]> = if dv.magnitudes.len() == count { Rc::from(dv.magnitudes.as_slice()) } else { magnitudes(dv.element, &f, &b, count).into() };
        Some(Rc::new(RankVec { element: dv.element, f, b, count, magnitudes }))
    }

    /// Length of one stored vector (bytes for `bit` vectors).
    fn width(&self) -> usize {
        let n = if matches!(self.element, VectorElement::Byte | VectorElement::Bit) { self.b.len() } else { self.f.len() };
        n / self.count.max(1)
    }

    /// `getDims()`: eight per byte for `bit` vectors.
    pub fn dims(&self) -> usize {
        if self.element == VectorElement::Bit { self.width() * 8 } else { self.width() }
    }

    pub fn class(&self) -> crate::types::ClassId {
        match self.element {
            VectorElement::Float => cls::FLOAT_RANK_VECTORS,
            VectorElement::BFloat16 => cls::BFLOAT16_RANK_VECTORS,
            VectorElement::Byte => cls::BYTE_RANK_VECTORS,
            VectorElement::Bit => cls::BIT_RANK_VECTORS,
        }
    }

    /// The vectors as `float[]`s (`getVectors()` / `getVectorValues()`).
    fn vectors(&self) -> Vec<Value> {
        let w = self.width();
        (0..self.count)
            .map(|i| Value::obj(Obj::Array(JArray::from_data(crate::types::Ty::FLOAT, ArrayData::Float(self.f[i * w..(i + 1) * w].to_vec())))))
            .collect()
    }
}

/// The magnitudes Elasticsearch stores with each vector.
fn magnitudes(element: VectorElement, f: &[f32], b: &[i8], count: usize) -> Vec<f32> {
    if count == 0 {
        return Vec::new();
    }
    match element {
        VectorElement::Byte => b.chunks(b.len() / count).map(|v| (v.iter().map(|&x| x as i32 * x as i32).sum::<i32>() as f64).sqrt() as f32).collect(),
        VectorElement::Bit => b.chunks(b.len() / count).map(|v| (v.iter().map(|&x| (x as u8).count_ones()).sum::<u32>() as f64).sqrt() as f32).collect(),
        _ => f.chunks(f.len() / count).map(|v| (super::vecmath::dot(v, v) as f64).sqrt() as f32).collect(),
    }
}

fn missing() -> Thrown {
    es_iae("rank-vectors value missing for a field, use isEmpty() to check for a missing value", &[])
}

fn rank_of(v: &Value) -> Result<Option<Rc<RankVec>>, Thrown> {
    match es(v) {
        Some(EsObj::RankVectors(x)) => Ok(x.clone()),
        _ => Err(crate::vm::cce("not rank vectors")),
    }
}

fn iterator(items: Vec<Value>) -> Value {
    Value::obj(Obj::Iterator(std::cell::RefCell::new(crate::iter::JIter::of_values(cls::GENERIC_ITERATOR, items))))
}

fn float_array(v: &[f32]) -> Value {
    Value::obj(Obj::Array(JArray::from_data(crate::types::Ty::FLOAT, ArrayData::Float(v.to_vec()))))
}

// ------------------------------------------------------------------------------ RankVectors

pub fn r_empty(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(es_value(EsObj::RankVectors(None)))
}
pub fn r_magnitudes(_: &mut Vm, a: &mut [Value]) -> VR {
    rank_of(&a[0])?.map(|r| float_array(&r.magnitudes)).ok_or_else(missing)
}
pub fn r_vectors(_: &mut Vm, a: &mut [Value]) -> VR {
    rank_of(&a[0])?.map(|r| iterator(r.vectors())).ok_or_else(missing)
}
pub fn r_is_empty(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(rank_of(&a[0])?.is_none()))
}
pub fn r_dims(_: &mut Vm, a: &mut [Value]) -> VR {
    rank_of(&a[0])?.map(|r| Value::Int(r.dims() as i32)).ok_or_else(missing)
}
pub fn r_size(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(rank_of(&a[0])?.map_or(0, |r| r.count as i32)))
}

/// `RankVectorsScriptDocValues.getVectorValues()` / `getMagnitudes()`.
pub fn sdv_vector_values(vm: &mut Vm, a: &mut [Value]) -> VR {
    let _ = vm;
    match es(&a[0]) {
        Some(EsObj::Values(s)) => s.rank.as_ref().map(|r| iterator(r.vectors())).ok_or_else(sdv_missing),
        _ => Err(crate::vm::cce("not doc values")),
    }
}
pub fn sdv_magnitudes(vm: &mut Vm, a: &mut [Value]) -> VR {
    let _ = vm;
    match es(&a[0]) {
        Some(EsObj::Values(s)) => s.rank.as_ref().map(|r| float_array(&r.magnitudes)).ok_or_else(sdv_missing),
        _ => Err(crate::vm::cce("not doc values")),
    }
}

fn sdv_missing() -> Thrown {
    es_iae("A document doesn't have a value for a rank-vectors field!", &[])
}

/// `RankVectorsScriptDocValues.get(i)`.
pub fn get_error() -> Thrown {
    es_error(
        cls::UNSUPPORTED_OPERATION_EXCEPTION,
        "accessing a rank-vectors field's value through 'get' or 'value' is not supported, use 'vectorValues' or 'magnitudes' instead.",
        &[],
    )
}

// ------------------------------------------------------------------------------ MaxSim

/// The parsed query vectors.
enum Query {
    Floats(Vec<Vec<f32>>),
    Bytes(Vec<Vec<i8>>),
}

struct MaxSimFn {
    field: Rc<str>,
    query: Query,
    hamming: bool,
}

/// `parseBytes`: a list of number lists, or of hex strings.
enum ByteInput {
    Lists(Vec<Vec<Value>>),
    Hex(Vec<Vec<i8>>),
}

fn class_name(v: &Value) -> String {
    crate::lookup::java_binary_name(v.class()).to_string()
}

fn lists(vm: &mut Vm, v: &Value, kind: &str) -> Result<Vec<Vec<Value>>, Thrown> {
    if !crate::dynamic::is_list(vm.reg, v) {
        return Err(es_iae(format!("Unsupported input object for {kind} vectors: {}", class_name(v)), &[]));
    }
    let outer = crate::lib_coll::elements(vm, v)?;
    let mut out = Vec::with_capacity(outer.len());
    for x in outer {
        if !crate::dynamic::is_list(vm.reg, &x) {
            return Err(crate::vm::cce(crate::cast::checkcast_message(crate::lookup::java_binary_name(x.class()), "java.util.List")));
        }
        out.push(crate::lib_coll::elements(vm, &x)?);
    }
    Ok(out)
}

fn parse_bytes(vm: &mut Vm, v: &Value) -> Result<ByteInput, Thrown> {
    let unsupported = || es_iae(format!("Unsupported input object for byte vectors: {}", class_name(v)), &[]);
    if !crate::dynamic::is_list(vm.reg, v) {
        return Err(unsupported());
    }
    let outer = crate::lib_coll::elements(vm, v)?;
    let Some(first) = outer.first() else {
        // `get(0)` of an empty list
        return Err(crate::vm::Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some("Index 0 out of bounds for length 0".into())));
    };
    if crate::dynamic::is_list(vm.reg, first) {
        return Ok(ByteInput::Lists(lists(vm, v, "byte")?));
    }
    if matches!(first, Value::Str(_)) {
        let mut out: Vec<Vec<i8>> = Vec::with_capacity(outer.len());
        for x in &outer {
            let Value::Str(s) = x else { return Err(crate::vm::cce(crate::cast::checkcast_message(crate::lookup::java_binary_name(x.class()), "java.lang.String"))) };
            let b: Vec<i8> = crate::lib_util::parse_hex(s.as_str())?.into_iter().map(|b| b as i8).collect();
            if let Some(prev) = out.last()
                && prev.len() != b.len()
            {
                return Err(inconsistent());
            }
            out.push(b);
        }
        return Ok(ByteInput::Hex(out));
    }
    Err(unsupported())
}

fn inconsistent() -> Thrown {
    es_iae("The query vector contains inner vectors which have inconsistent number of dimensions.", &[])
}

fn number(x: &Value) -> Result<(f32, i32), Thrown> {
    Ok(match *x {
        Value::Byte(b) => (b as f32, b as i32),
        Value::Short(b) => (b as f32, b as i32),
        Value::Int(b) => (b as f32, b),
        Value::Long(b) => (b as f32, b as i32),
        Value::Float(b) => (b, b as i32),
        Value::Double(b) => (b as f32, b as i32),
        _ => return Err(crate::vm::cce(crate::cast::checkcast_message(crate::lookup::java_binary_name(x.class()), "java.lang.Number"))),
    })
}

/// `RankVectors.checkDimensions` (a `bit` element counts eight dimensions per query value).
fn check_dims(element: VectorElement, dims: usize, qv: usize) -> Result<(), Thrown> {
    let qv = if element == VectorElement::Bit { qv * 8 } else { qv };
    if dims != qv {
        return Err(es_iae(format!("The query vector has a different number of dimensions [{qv}] than the document vectors [{dims}]."), &[]));
    }
    Ok(())
}

/// `ByteRankVectorsFunction(List)`: each inner vector checked against the element's bounds.
fn byte_lists(element: VectorElement, dims: usize, q: &[Vec<Value>]) -> Result<Vec<Vec<i8>>, Thrown> {
    if q.is_empty() {
        return Err(es_iae("The query vector is empty.", &[]));
    }
    check_dims(element, dims, q[0].len())?;
    let mut out = Vec::with_capacity(q.len());
    for inner in q {
        if inner.len() != q[0].len() {
            return Err(inconsistent());
        }
        let mut b = Vec::with_capacity(inner.len());
        let mut f = Vec::with_capacity(inner.len());
        for x in inner {
            let (fv, iv) = number(x)?;
            b.push(iv as i8);
            f.push(fv);
        }
        if let Some(e) = super::dense::byte_errors(&f, "byte") {
            return Err(es_iae(super::dense::preview(e, &f), &[]));
        }
        out.push(b);
    }
    Ok(out)
}

fn build(vm: &mut Vm, a: &[Value], hamming: bool) -> Result<MaxSimFn, Thrown> {
    let field_name = a[1].to_java_string();
    let f = super::doc::field_of(vm, &field_name)?;
    if f.class != cls::RANK_VECTORS_DOC_VALUES_FIELD {
        let from = crate::lookup::java_binary_name(f.class);
        return Err(es_error(
            cls::CLASS_CAST_EXCEPTION,
            format!(
                "class {from} cannot be cast to class org.elasticsearch.script.field.vectors.RankVectorsDocValuesField ({from} and org.elasticsearch.script.field.vectors.RankVectorsDocValuesField are in module org.elasticsearch.server@9.5.5 of loader 'app')"
            ),
            &[],
        ));
    }
    let element = f.element;
    let float_field = matches!(element, VectorElement::Float | VectorElement::BFloat16);
    if hamming && float_field {
        return Err(es_iae("hamming distance is only supported for byte or bit vectors", &[]));
    }
    // `field.get().getDims()` of the first document
    let dims = || f.rank.as_ref().map(|r| r.dims()).ok_or_else(missing);
    let query = if float_field {
        let q = lists(vm, &a[0], "float")?;
        if q.is_empty() {
            return Err(es_iae("The query vector is empty.", &[]));
        }
        let d = dims()?;
        if d != q[0].len() {
            return Err(es_iae(format!("The query vector has a different number of dimensions [{}] than the document vectors [{d}].", q[0].len()), &[]));
        }
        let mut out = Vec::with_capacity(q.len());
        for inner in &q {
            if inner.len() != q[0].len() {
                return Err(inconsistent());
            }
            let v = inner.iter().map(|x| number(x).map(|n| n.0)).collect::<Result<Vec<f32>, _>>()?;
            if let Some(e) = super::dense::nan_errors(&v, if element == VectorElement::BFloat16 { "bfloat16" } else { "float" }) {
                return Err(es_iae(super::dense::preview(e, &v), &[]));
            }
            out.push(v);
        }
        Query::Floats(out)
    } else {
        match parse_bytes(vm, &a[0])? {
            ByteInput::Hex(b) => {
                if element == VectorElement::Bit && !hamming {
                    let d = dims()?;
                    let n = b[0].len();
                    if d != n * 8 && d != n {
                        return Err(es_iae(
                            format!("The query vector has an incorrect number of dimensions. Must be [{}] for bitwise operations, or [{d}] for byte wise operations: provided [{n}].", d / 8),
                            &[],
                        ));
                    }
                }
                Query::Bytes(b)
            }
            ByteInput::Lists(q) if element == VectorElement::Bit && !hamming => {
                // `MaxSimBitDotProduct(List)`: a float query unless every value is a byte
                if q.is_empty() {
                    return Err(es_iae("The query vector is empty.", &[]));
                }
                let mut floats = Vec::with_capacity(q.len());
                let mut bytes = Vec::with_capacity(q.len());
                let mut is_float = false;
                for inner in &q {
                    if inner.len() != q[0].len() {
                        return Err(inconsistent());
                    }
                    let mut f = Vec::with_capacity(inner.len());
                    let mut b = Vec::with_capacity(inner.len());
                    for x in inner {
                        let (fv, iv) = number(x)?;
                        f.push(fv);
                        b.push(iv as i8);
                        is_float |= fv % 1.0 != 0.0 || !(-128.0..=127.0).contains(&fv);
                    }
                    floats.push(f);
                    bytes.push(b);
                }
                let d = dims()?;
                if is_float {
                    if d != floats[0].len() {
                        return Err(es_iae(
                            format!("The query vector contains inner vectors which have incorrect number of dimensions. Must be [{d}] for float wise operations: provided [{}].", floats[0].len()),
                            &[],
                        ));
                    }
                    Query::Floats(floats)
                } else {
                    let n = bytes[0].len();
                    if d != n * 8 && d != n {
                        return Err(es_iae(
                            format!(
                                "The query vector contains inner vectors which have incorrect number of dimensions. Must be [{}] for bitwise operations, or [{d}] for byte wise operations: provided [{n}].",
                                d / 8
                            ),
                            &[],
                        ));
                    }
                    Query::Bytes(bytes)
                }
            }
            ByteInput::Lists(q) => Query::Bytes(byte_lists(element, dims()?, &q)?),
        }
    };
    Ok(MaxSimFn { field: Rc::from(field_name.as_str()), query, hamming })
}

/// `Math.max` over floats (NaN wins, as in Java).
fn fmax(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() { f32::NAN } else { a.max(b) }
}

fn ip_float_bit(q: &[f32], d: &[i8]) -> f32 {
    let mut r = 0f32;
    for (i, &x) in q.iter().enumerate() {
        if i / 8 < d.len() && (d[i / 8] as u8) & (0x80 >> (i % 8)) != 0 {
            r += x;
        }
    }
    r
}

fn ip_byte_bit(q: &[i8], d: &[i8]) -> f32 {
    let mut r: i32 = 0;
    for (i, &x) in q.iter().enumerate() {
        if i / 8 < d.len() && (d[i / 8] as u8) & (0x80 >> (i % 8)) != 0 {
            r += x as i32;
        }
    }
    r as f32
}

fn dims_differ(a: usize, b: usize) -> Thrown {
    es_iae(format!("vector dimensions differ: {a}!={b}"), &[])
}

/// One document's MaxSim.
fn score_doc(f: &MaxSimFn, dv: &DocValues) -> Result<f64, Thrown> {
    let count = dv.vector_count;
    let element = dv.element;
    let bytes_el = matches!(element, VectorElement::Byte | VectorElement::Bit);
    let data_len = if bytes_el { dv.vector_bytes.len() } else { dv.vector.len() };
    let w = data_len / count.max(1);
    let mut sum = 0f32;
    match (&f.query, element) {
        (Query::Floats(q), VectorElement::Float | VectorElement::BFloat16) => {
            for qi in q {
                let mut m = f32::NEG_INFINITY;
                for d in dv.vector.chunks(w.max(1)).take(count) {
                    m = fmax(m, super::vecmath::dot(qi, d));
                }
                sum += m;
            }
        }
        (Query::Floats(q), VectorElement::Bit) => {
            for qi in q {
                let mut m = f32::NEG_INFINITY;
                for d in dv.vector_bytes.chunks(w.max(1)).take(count) {
                    m = fmax(m, ip_float_bit(qi, d));
                }
                sum += m;
            }
        }
        (Query::Bytes(q), VectorElement::Byte) if !f.hamming => {
            for qi in q {
                let mut m = f32::NEG_INFINITY;
                for d in dv.vector_bytes.chunks(w.max(1)).take(count) {
                    if d.len() != qi.len() {
                        return Err(es_iae(format!("vector dimensions incompatible: {}!= {}", qi.len(), d.len()), &[]));
                    }
                    let s: i32 = qi.iter().zip(d).map(|(a, b)| *a as i32 * *b as i32).sum();
                    m = fmax(m, s as f32);
                }
                sum += m;
            }
        }
        (Query::Bytes(q), VectorElement::Bit) if !f.hamming => {
            for qi in q {
                let mut m = f32::NEG_INFINITY;
                for d in dv.vector_bytes.chunks(w.max(1)).take(count) {
                    let s = if qi.len() == d.len() {
                        qi.iter().zip(d).map(|(a, b)| ((*a & *b) as u8).count_ones()).sum::<u32>() as f32
                    } else if qi.len() == d.len() * 8 {
                        ip_byte_bit(qi, d)
                    } else {
                        return Err(es_iae(format!("vector dimensions incompatible: {}!= 8 x {}", qi.len(), d.len()), &[]));
                    };
                    m = fmax(m, s);
                }
                sum += m;
            }
        }
        (Query::Bytes(q), VectorElement::Byte | VectorElement::Bit) => {
            let bits = (w * 8) as f32;
            for qi in q {
                let mut m = f32::NEG_INFINITY;
                for d in dv.vector_bytes.chunks(w.max(1)).take(count) {
                    if d.len() != qi.len() {
                        return Err(dims_differ(d.len(), qi.len()));
                    }
                    let x = d.iter().zip(qi).map(|(a, b)| ((*a ^ *b) as u8).count_ones()).sum::<u32>() as f32;
                    m = fmax(m, (bits - x) / bits);
                }
                sum += m;
            }
        }
        (Query::Floats(_), VectorElement::Byte) => {
            return Err(es_error(cls::UNSUPPORTED_OPERATION_EXCEPTION, "use [float maxSimDotProduct(byte[][] queryVector)] instead", &[]));
        }
        (Query::Bytes(_), _) => {
            return Err(es_error(cls::UNSUPPORTED_OPERATION_EXCEPTION, "use [float maxSimDotProduct(float[][] queryVector)] instead", &[]));
        }
    }
    Ok(sum as f64)
}

fn score(vm: &mut Vm, a: &[Value], hamming: bool) -> VR {
    let f = binding(vm, |vm| build(vm, a, hamming))?;
    let missing = || es_iae("A document doesn't have a value for a multi-vector field!", &[]);
    if !vm.st.has_doc {
        return Err(missing());
    }
    let mut buf = std::mem::take(&mut vm.st.vec_buf);
    let out = match vm.host.doc_values(&f.field, &mut buf) {
        Ok(true) if buf.kind == DocKind::RankVectors && buf.vector_count > 0 => score_doc(&f, &buf),
        Ok(_) => Err(missing()),
        Err(e) => Err(super::doc::host_error(e)),
    };
    vm.st.vec_buf = buf;
    out.map(Value::Double)
}

pub fn s_max_sim_dot(vm: &mut Vm, a: &mut [Value]) -> VR {
    score(vm, a, false)
}
pub fn s_max_sim_inv_hamming(vm: &mut Vm, a: &mut [Value]) -> VR {
    score(vm, a, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dv(element: VectorElement, f: &[f32], b: &[i8], count: usize) -> DocValues {
        let mut d = DocValues::default();
        d.clear(DocKind::RankVectors);
        d.element = element;
        d.vector.extend_from_slice(f);
        d.vector_bytes.extend_from_slice(b);
        d.vector_count = count;
        d
    }

    #[test]
    fn max_sim_sums_the_best_match_of_each_query_vector() {
        let doc = dv(VectorElement::Float, &[1.0, 0.0, 0.0, 1.0, 0.5, 0.5], &[], 3);
        let f = MaxSimFn { field: Rc::from("v"), query: Query::Floats(vec![vec![1.0, 0.0], vec![0.0, 2.0]]), hamming: false };
        // best of [1, 0, 0.5] + best of [0, 2, 1]
        assert_eq!(score_doc(&f, &doc).unwrap(), 3.0);
        let doc = dv(VectorElement::Byte, &[], &[1, 2, -3, 4], 2);
        let f = MaxSimFn { field: Rc::from("v"), query: Query::Bytes(vec![vec![1, 1]]), hamming: false };
        assert_eq!(score_doc(&f, &doc).unwrap(), 3.0);
        let f = MaxSimFn { field: Rc::from("v"), query: Query::Bytes(vec![vec![1, 2]]), hamming: true };
        // identical first vector: no differing bits
        assert_eq!(score_doc(&f, &doc).unwrap(), 1.0);
        let r = RankVec::from_doc(&doc).unwrap();
        assert_eq!((r.dims(), r.count), (2, 2));
        assert_eq!(&*r.magnitudes, &[(5f64).sqrt() as f32, 5.0]);
    }

    #[test]
    fn bit_vectors_mask_queries() {
        let doc = dv(VectorElement::Bit, &[], &[0x80u8 as i8, 0x01], 1);
        let q: Vec<f32> = (1..=16).map(|x| x as f32).collect();
        let f = MaxSimFn { field: Rc::from("v"), query: Query::Floats(vec![q]), hamming: false };
        assert_eq!(score_doc(&f, &doc).unwrap(), 17.0);
        let f = MaxSimFn { field: Rc::from("v"), query: Query::Bytes(vec![vec![-1, -1]]), hamming: false };
        assert_eq!(score_doc(&f, &doc).unwrap(), 2.0);
    }
}
