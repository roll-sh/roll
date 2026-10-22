//! `doc` (`LeafDocLookup`), `ScriptDocValues` and the field API (`field('f')`, `$('f', d)`).
//!
//! Doc values come from the host one field at a time into reusable [`DocValues`] buffers.
//! `doc['f'].value` with a literal field name compiles to a single instruction that reads the
//! buffer directly ([`doc_fast`]); everything else materializes a [`Sdv`] snapshot whose class
//! (`ScriptDocValues.Longs`, `.Dates`, …) decides the methods scripts can call.

use std::rc::Rc;

use super::{es, es_error, es_value, server_frames, EsObj};
use crate::host::{DocKind, DocValues, HostError};
use crate::lookup::Builder;
use crate::types::{cls, ClassId};
use crate::value::{ArrayData, JArray, Obj, Value};
use crate::vm::{DocWhat, Throw, Thrown, Vm, VR};

/// `ScriptDocValues` of one field for the current document.
pub struct Sdv {
    pub class: ClassId,
    pub kind: DocKind,
    pub field: Rc<str>,
    pub vals: Vec<Value>,
    /// The vector of a `dense_vector` field.
    pub vector: Option<super::dense::Vector>,
    /// The vectors of a `rank_vectors` field.
    pub rank: Option<Rc<super::rank::RankVec>>,
}

/// `ScriptDocValues` class for a field kind.
pub fn sdv_class(kind: DocKind) -> ClassId {
    use DocKind::*;
    match kind {
        Long | Integer | Short | Byte => cls::SDV_LONGS,
        UnsignedLong => cls::SDV_UNSIGNED_LONG,
        Double | Float | HalfFloat | ScaledFloat => cls::SDV_DOUBLES,
        String | Ip => cls::SDV_STRINGS,
        Version => cls::SDV_VERSION,
        Boolean => cls::SDV_BOOLEANS,
        Date | DateNanos => cls::SDV_DATES,
        GeoPoint => cls::SDV_GEO_POINTS,
        GeoShape => cls::SDV_GEO_SHAPE,
        Binary => cls::SDV_BYTES_REFS,
        DenseVector => cls::SDV_DENSE_VECTOR,
        RankVectors => cls::SDV_RANK_VECTORS,
    }
}

/// The script value of doc value `i` (`ScriptDocValues.get(i)`).
fn doc_value(dv: &DocValues, i: usize) -> Value {
    use DocKind::*;
    match dv.kind {
        Long | Integer | Short | Byte | UnsignedLong => Value::Long(dv.longs[i]),
        Boolean => Value::Bool(dv.longs[i] != 0),
        Date => crate::time::zoned_utc_millis(dv.longs[i]),
        DateNanos => crate::time::zoned_utc_nanos(dv.longs[i]),
        Double | Float | HalfFloat | ScaledFloat => Value::Double(dv.doubles[i]),
        String | Ip | Version => Value::str(&dv.strings()[i]),
        GeoPoint => es_value(EsObj::GeoPoint(dv.geo[i][0], dv.geo[i][1])),
        GeoShape => es_value(EsObj::GeoShape(Rc::new(super::GeoShapeValue {
            centroid: dv.geo.first().copied().unwrap_or_default(),
            top_left: dv.geo.get(1).copied().unwrap_or_default(),
            bottom_right: dv.geo.get(2).copied().unwrap_or_default(),
            label: dv.geo.get(3).or(dv.geo.first()).copied().unwrap_or_default(),
            dimensional_type: dv.longs.first().copied().unwrap_or(0) as i32,
        }))),
        Binary => es_value(EsObj::BytesRef(Rc::from(dv.bytes[i].as_slice()))),
        DenseVector | RankVectors => Value::Null,
    }
}

pub fn sdv_from(field: &str, dv: &DocValues) -> Sdv {
    let n = dv.len();
    let vector = super::dense::Vector::from_doc(dv);
    let rank = super::rank::RankVec::from_doc(dv);
    Sdv { class: sdv_class(dv.kind), kind: dv.kind, field: Rc::from(field), vals: (0..n).map(|i| doc_value(dv, i)).collect(), vector, rank }
}

// ------------------------------------------------------------------------------ errors

const SDV: &str = "index.fielddata.ScriptDocValues";

fn class_simple(class: ClassId) -> &'static str {
    match class {
        cls::SDV_LONGS => "Longs",
        cls::SDV_DOUBLES => "Doubles",
        cls::SDV_STRINGS => "Strings",
        cls::SDV_BOOLEANS => "Booleans",
        cls::SDV_DATES => "Dates",
        cls::SDV_GEO_POINTS => "GeoPoints",
        cls::SDV_BYTES_REFS => "BytesRefs",
        _ => "",
    }
}

/// Source lines of `ScriptDocValues$X.get`: the `throwIfEmpty` call, then `throwIfBeyondLength`.
fn get_lines(class: ClassId) -> (u32, u32) {
    match class {
        cls::SDV_LONGS => (117, 118),
        cls::SDV_DOUBLES => (213, 214),
        cls::SDV_STRINGS => (483, 484),
        cls::SDV_BOOLEANS => (405, 406),
        cls::SDV_DATES => (144, 145),
        cls::SDV_GEO_POINTS => (314, 315),
        cls::SDV_BYTES_REFS => (507, 508),
        _ => (0, 0),
    }
}

/// Line of `getValue`'s call (`get(0)`, or `throwIfEmpty()` for booleans and bytes refs).
fn get_value_line(class: ClassId) -> u32 {
    match class {
        cls::SDV_LONGS => 112,
        cls::SDV_DOUBLES => 208,
        cls::SDV_STRINGS => 478,
        cls::SDV_BOOLEANS => 399,
        cls::SDV_DATES => 139,
        cls::SDV_GEO_POINTS => 285,
        cls::SDV_BYTES_REFS => 501,
        _ => 0,
    }
}

fn frame(class: ClassId, method: &str, line: u32) -> String {
    let simple = class_simple(class);
    if simple.is_empty() {
        let name = crate::lookup::class_name(class);
        let short = name.rsplit('.').next().unwrap_or(name);
        format!("{}.{method}({short}.java:{line})", name.trim_start_matches("org.elasticsearch."))
    } else {
        format!("{SDV}${simple}.{method}(ScriptDocValues.java:{line})")
    }
}

fn empty_error(class: ClassId, outer: &[String]) -> Thrown {
    let mut frames = vec![format!("{SDV}.throwIfEmpty(ScriptDocValues.java:93)")];
    frames.extend(outer.iter().cloned());
    let refs: Vec<&str> = frames.iter().map(|s| s.as_str()).collect();
    let _ = class;
    es_error(
        cls::ILLEGAL_STATE_EXCEPTION,
        "A document doesn't have a value for a field! Use doc[<field>].size()==0 to check if a document is missing a field!",
        &refs,
    )
}

/// `ScriptDocValues.get(i)`'s failures, with `outer` frames of the caller (`getValue`, …).
fn get_error(class: ClassId, kind: DocKind, n: usize, i: i32, outer: &[String]) -> Thrown {
    let (l1, l2) = get_lines(class);
    if n == 0 {
        let mut f = vec![frame(class, "get", l1)];
        f.extend(outer.iter().cloned());
        return empty_error(class, &f);
    }
    if class == cls::SDV_DATES && i >= n as i32 {
        let mut f = vec![frame(class, "get", 147)];
        f.extend(outer.iter().cloned());
        let refs: Vec<&str> = f.iter().map(|s| s.as_str()).collect();
        return es_error(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, format!("attempted to fetch the [{i}] date when there are only [{n}] dates."), &refs);
    }
    if i >= n as i32 {
        let mut f = vec![format!("{SDV}.throwIfBeyondLength(ScriptDocValues.java:101)"), frame(class, "get", l2)];
        f.extend(outer.iter().cloned());
        let refs: Vec<&str> = f.iter().map(|s| s.as_str()).collect();
        return es_error(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, format!("A document doesn't have a value for a field at position [{i}]!"), &refs);
    }
    // negative index: the supplier's array access fails; its length is Lucene's oversized
    // capacity for the values (ArrayUtil.oversize)
    let cap = oversize(n, kind);
    let mut f: Vec<String> = Vec::new();
    if matches!(kind, DocKind::Long) {
        f.push("script.field.AbstractLongDocValuesField.getLong(AbstractLongDocValuesField.java:92)".into());
        f.push("script.field.AbstractLongDocValuesField.getInternal(AbstractLongDocValuesField.java:88)".into());
        f.push("script.field.AbstractLongDocValuesField.getInternal(AbstractLongDocValuesField.java:20)".into());
    }
    f.push(frame(class, "get", l2 + 1));
    f.extend(outer.iter().cloned());
    let refs: Vec<&str> = f.iter().map(|s| s.as_str()).collect();
    es_error(cls::ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION, format!("Index {i} out of bounds for length {cap}"), &refs)
}

fn oversize(n: usize, kind: DocKind) -> usize {
    let extra = (n >> 3).max(3);
    let size = n + extra;
    match kind {
        DocKind::Long | DocKind::Integer | DocKind::Short | DocKind::Byte | DocKind::Double | DocKind::Date | DocKind::DateNanos => size,
        _ => size,
    }
}

/// "accessing a vector field's value through 'get' or 'value' is not supported".
pub fn vector_get_error() -> Thrown {
    es_error(
        cls::UNSUPPORTED_OPERATION_EXCEPTION,
        "accessing a vector field's value through 'get' or 'value' is not supported, use 'vectorValue' or 'magnitude' instead.",
        &[
            "index.mapper.vectors.DenseVectorScriptDocValues.get(DenseVectorScriptDocValues.java:57)",
            "index.mapper.vectors.DenseVectorScriptDocValues.get(DenseVectorScriptDocValues.java:16)",
        ],
    )
}

impl Sdv {
    pub fn get(&self, i: i32) -> VR {
        self.get_with(i, &[])
    }

    fn get_with(&self, i: i32, outer: &[String]) -> VR {
        if self.kind == DocKind::DenseVector {
            return Err(vector_get_error());
        }
        if self.kind == DocKind::RankVectors {
            return Err(super::rank::get_error());
        }
        if i >= 0 && (i as usize) < self.vals.len() {
            return Ok(match &self.vals[i as usize] {
                // GeoPoints.get copies the point
                Value::Obj(o) => match &**o {
                    Obj::Es(EsObj::GeoPoint(a, b)) => es_value(EsObj::GeoPoint(*a, *b)),
                    _ => self.vals[i as usize].clone(),
                },
                v => v.clone(),
            });
        }
        Err(get_error(self.class, self.kind, self.vals.len(), i, outer))
    }

    /// `get(i)` called through `List` (`x[i]` on a `def`, a `List`-typed variable): the call
    /// enters the compiler's bridge method, whose frame carries the class declaration's line.
    pub fn get_through_list(&self, i: i32) -> VR {
        let decl = match self.class {
            cls::SDV_LONGS => 105,
            cls::SDV_DATES => 128,
            cls::SDV_DOUBLES => 201,
            cls::SDV_GEO_POINTS => 275,
            cls::SDV_BOOLEANS => 392,
            cls::SDV_STRINGS => 471,
            cls::SDV_BYTES_REFS => 494,
            _ => return self.get(i),
        };
        self.get_with(i, &[frame(self.class, "get", decl)])
    }

    /// `getValue()`.
    pub fn value(&self) -> VR {
        let gv = frame(self.class, "getValue", get_value_line(self.class));
        if matches!(self.class, cls::SDV_BOOLEANS | cls::SDV_BYTES_REFS) && self.vals.is_empty() {
            return Err(empty_error(self.class, &[gv]));
        }
        self.get_with(0, &[gv])
    }
}

// ------------------------------------------------------------------------------ the doc map

fn lookup_frames() -> [&'static str; 3] {
    [
        "search.lookup.LeafDocLookup.getFactoryForDoc(LeafDocLookup.java:135)",
        "search.lookup.LeafDocLookup.get(LeafDocLookup.java:171)",
        "search.lookup.LeafDocLookup.get(LeafDocLookup.java:31)",
    ]
}

pub fn no_field(name: &str) -> Thrown {
    super::es_iae(format!("No field found for [{name}] in mapping"), &lookup_frames())
}

pub fn host_error(e: HostError) -> Thrown {
    let mut t = Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(e.reason));
    t.es_type = Some(e.kind);
    t.frames = e.frames;
    t
}

/// A host failure while `doc[...]` loads a field: the host's frames, then the lookup's.
fn lookup_error(e: HostError) -> Thrown {
    let mut t = host_error(e);
    t.frames.extend(server_frames(&[
        "search.lookup.SearchLookup.getForField(SearchLookup.java:160)",
        "search.lookup.LeafDocLookup.getFactoryForDoc(LeafDocLookup.java:156)",
        "search.lookup.LeafDocLookup.get(LeafDocLookup.java:171)",
        "search.lookup.LeafDocLookup.get(LeafDocLookup.java:31)",
    ]));
    t
}

/// `LeafDocLookup` methods that throw `UnsupportedOperationException`.
pub fn doc_unsupported(method: &str, line: u32) -> Thrown {
    let mut t = Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, None);
    t.frames = server_frames(&[&format!("search.lookup.LeafDocLookup.{method}(LeafDocLookup.java:{line})")]);
    t
}

/// The `doc_dyn` slot of field `name` (created on first use).
fn dyn_slot(vm: &mut Vm, name: &str) -> usize {
    match vm.st.doc_dyn.iter().position(|(n, _)| &**n == name) {
        Some(i) => i,
        None => {
            vm.st.doc_dyn.push((Box::from(name), DocValues::default()));
            vm.st.doc_sdv.push((u64::MAX, Value::Null));
            vm.st.doc_dyn.len() - 1
        }
    }
}

/// `doc[name]`: the field's `ScriptDocValues` for the current document. Like
/// `LeafDocLookup`, every read in one document returns the same object, and the object is
/// refilled in place for the next document when the script kept no reference to it, so a
/// loop over `doc[...]` allocates nothing per document.
pub fn doc_get(vm: &mut Vm, k: &Value) -> VR {
    let owned;
    let name: &str = match k {
        Value::Str(s) => s.as_str(),
        other => {
            owned = other.to_java_string();
            &owned
        }
    };
    let idx = dyn_slot(vm, name);
    let generation = vm.st.doc_gen;
    if vm.st.doc_sdv[idx].0 == generation {
        return Ok(vm.st.doc_sdv[idx].1.clone());
    }
    let mut buf = std::mem::take(&mut vm.st.doc_dyn[idx].1);
    let r = vm.host.doc_values(name, &mut buf);
    let out = match r {
        Ok(true) => {
            let mut cached = std::mem::replace(&mut vm.st.doc_sdv[idx].1, Value::Null);
            let reused = match &mut cached {
                Value::Obj(rc) => match Rc::get_mut(rc) {
                    Some(Obj::Es(EsObj::Values(sdv))) => {
                        refill(sdv, &buf);
                        true
                    }
                    _ => false,
                },
                _ => false,
            };
            let v = if reused { cached } else { es_value(EsObj::Values(sdv_from(name, &buf))) };
            vm.st.doc_sdv[idx] = (generation, v.clone());
            Ok(v)
        }
        Ok(false) => Err(no_field(name)),
        Err(e) => Err(lookup_error(e)),
    };
    vm.st.doc_dyn[idx].1 = buf;
    out
}

/// Refills a `ScriptDocValues` snapshot with another document's values.
fn refill(sdv: &mut Sdv, dv: &DocValues) {
    sdv.class = sdv_class(dv.kind);
    sdv.kind = dv.kind;
    sdv.vals.clear();
    sdv.vals.extend((0..dv.len()).map(|i| doc_value(dv, i)));
    sdv.vector = super::dense::Vector::from_doc(dv);
    sdv.rank = super::rank::RankVec::from_doc(dv);
}

/// `doc.containsKey(key)`.
pub fn doc_contains(vm: &mut Vm, k: &Value) -> bool {
    let name = k.to_java_string();
    vm.host.has_field(&name)
}

/// `doc['literal'].value|.size()|.empty` over the program's literal doc-values buffer.
/// Loads the doc values of literal field `field` for the current document (`DocValue` fetch).
#[inline(never)]
pub fn doc_fetch(vm: &mut Vm, field: u32) -> Result<(), crate::vm::Thrown> {
    let i = field as usize;
    let prog = vm.prog;
    let name: &str = &prog.doc_fields[i];
    match vm.host.doc_values_at(i, name, &mut vm.st.doc_bufs[i]) {
        Ok(true) => {
            vm.st.doc_buf_gen[i] = vm.st.doc_gen;
            Ok(())
        }
        Ok(false) => Err(no_field(name)),
        Err(e) => Err(lookup_error(e)),
    }
}

pub fn doc_fast(vm: &mut Vm, field: u32, what: DocWhat) -> VR {
    let i = field as usize;
    if vm.st.doc_buf_gen[i] != vm.st.doc_gen {
        let prog = vm.prog;
        let name: &str = &prog.doc_fields[i];
        // fill the buffer in place: moving it out and back would copy the whole struct twice
        let r = vm.host.doc_values_at(i, name, &mut vm.st.doc_bufs[i]);
        match r {
            Ok(true) => vm.st.doc_buf_gen[i] = vm.st.doc_gen,
            Ok(false) => return Err(no_field(name)),
            Err(e) => return Err(lookup_error(e)),
        }
    }
    let dv = &vm.st.doc_bufs[i];
    match what {
        DocWhat::Fetch => Ok(Value::Null),
        DocWhat::Size => Ok(Value::Int(dv.len() as i32)),
        DocWhat::Empty => Ok(Value::Bool(dv.is_empty())),
        DocWhat::Value => {
            let class = sdv_class(dv.kind);
            if matches!(class, cls::SDV_UNSIGNED_LONG | cls::SDV_VERSION | cls::SDV_DENSE_VECTOR | cls::SDV_RANK_VECTORS) {
                return Err(crate::vm::iae("Illegal list shortcut value [value]."));
            }
            if dv.is_empty() {
                let gv = frame(class, "getValue", get_value_line(class));
                if matches!(class, cls::SDV_BOOLEANS | cls::SDV_BYTES_REFS) {
                    return Err(empty_error(class, &[gv]));
                }
                return Err(get_error(class, dv.kind, 0, 0, &[gv]));
            }
            Ok(doc_value(dv, 0))
        }
    }
}

/// Element `i` of a literal `doc['field']` whose doc values [`doc_fast`] fetched, as iterating
/// the `ScriptDocValues` yields it (a for-each loop compiled to an index loop).
pub fn doc_at(vm: &mut Vm, field: u32, i: usize) -> VR {
    let dv = &vm.st.doc_bufs[field as usize];
    use crate::host::DocKind as K;
    Ok(match dv.kind {
        K::Long if i < dv.longs.len() => Value::Long(dv.longs[i]),
        K::Double if i < dv.doubles.len() => Value::Double(dv.doubles[i]),
        K::RankVectors => return Err(super::rank::get_error()),
        K::DenseVector => {
            // the list iterator's `next()` calls `get`
            let mut t = vector_get_error();
            t.frames.push("java.base/java.util.AbstractList$Itr.next(AbstractList.java:373)".into());
            return Err(t);
        }
        _ => doc_value(dv, i),
    })
}

// ------------------------------------------------------------------------------ the field API

/// A field of the field API: typed values of the current document (`EmptyField` when the
/// field is not mapped).
pub struct DocField {
    pub class: ClassId,
    pub name: Rc<str>,
    pub kind: Option<DocKind>,
    /// What `get` returns, in the field's Java type.
    pub vals: Vec<Value>,
    /// Alternative views: `asDouble` of half floats, `asString` of IPs.
    pub alt: Vec<Value>,
    pub vector: Option<super::dense::Vector>,
    pub rank: Option<Rc<super::rank::RankVec>>,
    /// Element type of a vector field.
    pub element: crate::host::VectorElement,
}

pub fn field_class(kind: DocKind) -> ClassId {
    use DocKind::*;
    match kind {
        Long => cls::LONG_DOC_VALUES_FIELD,
        Integer => cls::INTEGER_DOC_VALUES_FIELD,
        Short => cls::SHORT_DOC_VALUES_FIELD,
        Byte => cls::BYTE_DOC_VALUES_FIELD,
        UnsignedLong => cls::UNSIGNED_LONG_DOC_VALUES_FIELD,
        Double => cls::DOUBLE_DOC_VALUES_FIELD,
        Float => cls::FLOAT_DOC_VALUES_FIELD,
        HalfFloat => cls::HALF_FLOAT_DOC_VALUES_FIELD,
        ScaledFloat => cls::SCALED_FLOAT_DOC_VALUES_FIELD,
        String => cls::KEYWORD_DOC_VALUES_FIELD,
        Ip => cls::IP_DOC_VALUES_FIELD,
        Version => cls::VERSION_STRING_DOC_VALUES_FIELD,
        Boolean => cls::BOOLEAN_DOC_VALUES_FIELD,
        Date => cls::DATE_MILLIS_DOC_VALUES_FIELD,
        DateNanos => cls::DATE_NANOS_DOC_VALUES_FIELD,
        GeoPoint => cls::GEO_POINT_DOC_VALUES_FIELD,
        GeoShape => cls::GEO_SHAPE_DOC_VALUES_FIELD,
        Binary => cls::BINARY_DOC_VALUES_FIELD,
        DenseVector => cls::DENSE_VECTOR_DOC_VALUES_FIELD,
        RankVectors => cls::RANK_VECTORS_DOC_VALUES_FIELD,
    }
}

fn field_value(dv: &DocValues, i: usize) -> (Value, Value) {
    use DocKind::*;
    match dv.kind {
        Integer => (Value::Int(dv.longs[i] as i32), Value::Null),
        Short => (Value::Short(dv.longs[i] as i16), Value::Null),
        Byte => (Value::Byte(dv.longs[i] as i8), Value::Null),
        Float => (Value::Float(dv.doubles[i] as f32), Value::Null),
        HalfFloat => (Value::Float(dv.doubles[i] as f32), Value::Double(dv.doubles[i])),
        Ip => {
            let s = &dv.strings()[i];
            let ip = s.parse::<std::net::IpAddr>().map(|a| es_value(EsObj::Ip(a))).unwrap_or_else(|_| Value::str(s));
            (ip, Value::str(s))
        }
        Binary => (super::misc::nio(super::misc::NioBuf::wrap(Rc::from(dv.bytes[i].as_slice()))), Value::Null),
        _ => (doc_value(dv, i), Value::Null),
    }
}

pub fn field_of(vm: &mut Vm, name: &str) -> Result<DocField, Thrown> {
    if !vm.st.has_doc {
        return Ok(empty_field(name));
    }
    let mut dv = DocValues::default();
    match vm.host.field_values(name, &mut dv) {
        Ok(true) => {}
        Ok(false) => return Ok(empty_field(name)),
        Err(e) => return Err(host_error(e)),
    }
    let n = dv.len();
    let (mut vals, mut alt) = (Vec::with_capacity(n), Vec::new());
    for i in 0..n {
        let (v, a) = field_value(&dv, i);
        vals.push(v);
        if !a.is_null() {
            alt.push(a);
        }
    }
    let vector = super::dense::Vector::from_doc(&dv);
    let rank = super::rank::RankVec::from_doc(&dv);
    if dv.kind == DocKind::DenseVector {
        vals = if vector.is_some() { vec![Value::Null] } else { Vec::new() };
    }
    if dv.kind == DocKind::RankVectors {
        vals = if rank.is_some() { vec![Value::Null] } else { Vec::new() };
    }
    Ok(DocField { class: field_class(dv.kind), name: Rc::from(name), kind: Some(dv.kind), vals, alt, vector, rank, element: dv.element })
}

fn empty_field(name: &str) -> DocField {
    DocField { class: cls::EMPTY_FIELD, name: Rc::from(name), kind: None, vals: Vec::new(), alt: Vec::new(), vector: None, rank: None, element: Default::default() }
}

fn field_ref(v: &Value) -> Result<&DocField, Thrown> {
    match es(v) {
        Some(EsObj::Field(f)) => Ok(f),
        _ => Err(crate::vm::cce("not a field")),
    }
}

fn sdv_ref(v: &Value) -> Result<&Sdv, Thrown> {
    match es(v) {
        Some(EsObj::Values(s)) => Ok(s),
        _ => Err(crate::vm::cce("not doc values")),
    }
}

fn f_field(vm: &mut Vm, a: &mut [Value]) -> VR {
    let name = a[0].to_java_string();
    Ok(es_value(EsObj::Field(field_of(vm, &name)?)))
}

/// `$('field', default)`: `field('field').get(default)`.
fn f_dollar(vm: &mut Vm, a: &mut [Value]) -> VR {
    let name = a[0].to_java_string();
    let f = field_of(vm, &name)?;
    Ok(match f.vals.first() {
        Some(v) if !matches!(f.kind, Some(DocKind::DenseVector | DocKind::RankVectors)) => v.clone(),
        _ => a[1].clone(),
    })
}

fn fl_get1(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_ref(&a[0])?;
    Ok(f.vals.first().cloned().unwrap_or_else(|| a[1].clone()))
}

fn fl_get2(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_ref(&a[0])?;
    let i = crate::lib_lang::int(&a[1]);
    Ok(if i >= 0 && (i as usize) < f.vals.len() { f.vals[i as usize].clone() } else { a[2].clone() })
}

fn fl_alt1(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_ref(&a[0])?;
    Ok(f.alt.first().cloned().unwrap_or_else(|| a[1].clone()))
}

fn fl_alt2(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_ref(&a[0])?;
    let i = crate::lib_lang::int(&a[1]);
    Ok(if i >= 0 && (i as usize) < f.alt.len() { f.alt[i as usize].clone() } else { a[2].clone() })
}

fn fl_alts(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_ref(&a[0])?;
    Ok(crate::value::JList::array_list(f.alt.clone()))
}

fn fl_name(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::str(&field_ref(&a[0])?.name))
}

fn fl_is_empty(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(field_ref(&a[0])?.vals.is_empty()))
}

fn fl_size(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(field_ref(&a[0])?.vals.len() as i32))
}

fn fl_rank(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(es_value(EsObj::RankVectors(field_ref(&a[0])?.rank.clone())))
}

fn fl_rank_or(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_ref(&a[0])?;
    Ok(match &f.rank {
        Some(r) => es_value(EsObj::RankVectors(Some(r.clone()))),
        None => a[1].clone(),
    })
}

fn fl_vector(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(es_value(EsObj::Vector(field_ref(&a[0])?.vector.clone())))
}

fn fl_vector_or(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_ref(&a[0])?;
    Ok(match &f.vector {
        Some(v) => es_value(EsObj::Vector(Some(v.clone()))),
        None => a[1].clone(),
    })
}

// ------------------------------------------------------------------------------ natives

fn sdv_get(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = crate::lib_lang::int(&a[1]);
    sdv_ref(&a[0])?.get(i)
}

fn sdv_value(_: &mut Vm, a: &mut [Value]) -> VR {
    sdv_ref(&a[0])?.value()
}

fn unmodifiable(frames: &[&str]) -> Thrown {
    let mut t = Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, Some("doc values are unmodifiable".into()));
    t.frames = server_frames(frames);
    t
}

fn sdv_add(_: &mut Vm, _: &mut [Value]) -> VR {
    let mut t = unmodifiable(&["index.fielddata.ScriptDocValues.add(ScriptDocValues.java:68)"]);
    t.frames.push("java.base/java.util.AbstractList.add(AbstractList.java:113)".into());
    Err(t)
}

fn sdv_add_at(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(unmodifiable(&["index.fielddata.ScriptDocValues.add(ScriptDocValues.java:68)"]))
}

fn sdv_add_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    if crate::lib_coll::elements(vm, &a[1])?.is_empty() {
        return Ok(Value::Bool(false));
    }
    let mut t = unmodifiable(&["index.fielddata.ScriptDocValues.add(ScriptDocValues.java:68)"]);
    t.frames.push("java.base/java.util.AbstractList.add(AbstractList.java:113)".into());
    t.frames.push("java.base/java.util.AbstractCollection.addAll(AbstractCollection.java:336)".into());
    Err(t)
}

fn sdv_set(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(unmodifiable(&["index.fielddata.ScriptDocValues.set(ScriptDocValues.java:83)"]))
}

fn sdv_remove_at(_: &mut Vm, _: &mut [Value]) -> VR {
    let mut t = Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, None);
    t.frames = vec!["java.base/java.util.AbstractList.remove(AbstractList.java:169)".into()];
    Err(t)
}

fn sdv_replace_all(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(unmodifiable(&["index.fielddata.ScriptDocValues.replaceAll(ScriptDocValues.java:78)"]))
}

fn sdv_sort(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(unmodifiable(&["index.fielddata.ScriptDocValues.sort(ScriptDocValues.java:88)"]))
}

fn sdv_clear(_: &mut Vm, a: &mut [Value]) -> VR {
    if sdv_ref(&a[0])?.vals.is_empty() {
        return Ok(Value::Null);
    }
    sdv_remove_at_frames()
}

fn sdv_remove_at_frames() -> VR {
    let mut t = Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, None);
    t.frames = vec![
        "java.base/java.util.AbstractList$Itr.remove(AbstractList.java:389)".into(),
        "java.base/java.util.AbstractList.removeRange(AbstractList.java:600)".into(),
        "java.base/java.util.AbstractList.clear(AbstractList.java:245)".into(),
    ];
    Err(t)
}

fn sdv_remove_if(vm: &mut Vm, a: &mut [Value]) -> VR {
    let s = sdv_ref(&a[0])?;
    let vals = s.vals.clone();
    let f = a[1].clone();
    for v in vals {
        if crate::func::call_bool(vm, &f, &[v])? {
            let mut t = Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, None);
            t.frames = vec![
                "java.base/java.util.AbstractList.remove(AbstractList.java:169)".into(),
                "java.base/java.util.AbstractList$Itr.remove(AbstractList.java:389)".into(),
                "java.base/java.util.Collection.removeIf(Collection.java:583)".into(),
            ];
            return Err(t);
        }
    }
    Ok(Value::Bool(false))
}

fn geo_of(v: &Value) -> Result<(f64, f64), Thrown> {
    match es(v) {
        Some(EsObj::GeoPoint(a, b)) => Ok((*a, *b)),
        _ => Err(crate::vm::npe("Cannot invoke \"org.elasticsearch.common.geo.GeoPoint.lat()\" because \"point\" is null")),
    }
}

fn geo_value_with(s: &Sdv, outer: &str, line: u32) -> Result<(f64, f64), Thrown> {
    let gv = frame(s.class, "getValue", get_value_line(s.class));
    let extra = frame(s.class, outer, line);
    let v = s.get_with(0, &[gv, extra])?;
    geo_of(&v)
}

fn gp_lat(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(geo_value_with(sdv_ref(&a[0])?, "getLat", 289)?.0))
}

fn gp_lon(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(geo_value_with(sdv_ref(&a[0])?, "getLon", 309)?.1))
}

fn gp_coords(a: &[Value], lat: bool) -> VR {
    let s = sdv_ref(&a[0])?;
    let mut out = Vec::with_capacity(s.vals.len());
    for v in &s.vals {
        let (la, lo) = geo_of(v)?;
        out.push(if lat { la } else { lo });
    }
    Ok(Value::obj(Obj::Array(JArray::from_data(crate::types::Ty::DOUBLE, ArrayData::Double(out)))))
}

fn gp_lats(_: &mut Vm, a: &mut [Value]) -> VR {
    gp_coords(a, true)
}

fn gp_lons(_: &mut Vm, a: &mut [Value]) -> VR {
    gp_coords(a, false)
}

fn gp_arc(_: &mut Vm, a: &mut [Value]) -> VR {
    let (la, lo) = geo_value_with(sdv_ref(&a[0])?, "arcDistance", 326)?;
    Ok(Value::Double(super::score::haversin_meters(la, lo, crate::lib_lang::double(&a[1]), crate::lib_lang::double(&a[2]))))
}

fn gp_arc_default(vm: &mut Vm, a: &mut [Value]) -> VR {
    if sdv_ref(&a[0])?.vals.is_empty() {
        return Ok(a[3].clone());
    }
    gp_arc(vm, a)
}

fn gp_plane(_: &mut Vm, a: &mut [Value]) -> VR {
    let (la, lo) = geo_value_with(sdv_ref(&a[0])?, "planeDistance", 338)?;
    Ok(Value::Double(super::score::plane_distance(la, lo, crate::lib_lang::double(&a[1]), crate::lib_lang::double(&a[2]))))
}

fn gp_plane_default(vm: &mut Vm, a: &mut [Value]) -> VR {
    if sdv_ref(&a[0])?.vals.is_empty() {
        return Ok(a[3].clone());
    }
    gp_plane(vm, a)
}

fn gp_geohash(_: &mut Vm, a: &mut [Value]) -> VR {
    let (la, lo) = geo_value_with(sdv_ref(&a[0])?, "geohashDistance", 350)?;
    let hash = a[1].to_java_string();
    let (hla, hlo) = super::score::geohash_decode(&hash)?;
    Ok(Value::Double(super::score::haversin_meters(la, lo, hla, hlo)))
}

fn gp_geohash_default(vm: &mut Vm, a: &mut [Value]) -> VR {
    if sdv_ref(&a[0])?.vals.is_empty() {
        return Ok(a[2].clone());
    }
    gp_geohash(vm, a)
}

fn gp_dim_type(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(if sdv_ref(&a[0])?.vals.is_empty() { -1 } else { 0 }))
}

fn gp_centroid(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = sdv_ref(&a[0])?;
    if s.vals.is_empty() {
        return Ok(Value::Null);
    }
    let (mut la, mut lo) = (0.0, 0.0);
    for v in &s.vals {
        let (x, y) = geo_of(v)?;
        la += x;
        lo += y;
    }
    let n = s.vals.len() as f64;
    Ok(es_value(EsObj::GeoPoint(la / n, lo / n)))
}

fn gp_bbox(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = sdv_ref(&a[0])?;
    if s.vals.is_empty() {
        return Ok(Value::Null);
    }
    let (mut top, mut bottom, mut left, mut right) = (f64::NEG_INFINITY, f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY);
    for v in &s.vals {
        let (x, y) = geo_of(v)?;
        top = top.max(x);
        bottom = bottom.min(x);
        left = left.min(y);
        right = right.max(y);
    }
    Ok(es_value(EsObj::BoundingBox((top, left), (bottom, right))))
}

/// The shape of a `geo_shape` field's doc values (`None` when the document has none).
fn shape_of(v: &Value) -> Result<Option<super::GeoShapeValue>, Thrown> {
    Ok(match sdv_ref(v)?.vals.first() {
        Some(Value::Obj(o)) => match &**o {
            Obj::Es(EsObj::GeoShape(g)) => Some(**g),
            _ => None,
        },
        _ => None,
    })
}

fn gs_dim_type(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(shape_of(&a[0])?.map_or(-1, |g| g.dimensional_type)))
}
fn gs_centroid(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(shape_of(&a[0])?.map_or(Value::Null, |g| es_value(EsObj::GeoPoint(g.centroid[0], g.centroid[1]))))
}
fn gs_label(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(shape_of(&a[0])?.map_or(Value::Null, |g| es_value(EsObj::GeoPoint(g.label[0], g.label[1]))))
}
fn gs_bbox(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(shape_of(&a[0])?.map_or(Value::Null, |g| es_value(EsObj::BoundingBox((g.top_left[0], g.top_left[1]), (g.bottom_right[0], g.bottom_right[1])))))
}
fn gs_no_bbox() -> Thrown {
    crate::vm::npe("Cannot invoke \"org.elasticsearch.common.geo.GeoBoundingBox.right()\" because the return value of \"org.elasticsearch.xpack.spatial.index.fielddata.plain.AbstractAtomicGeoShapeShapeFieldData$GeoShapeScriptValues.getBoundingBox()\" is null")
}
/// `lonToSphericalMercator(right) - lonToSphericalMercator(left)`.
fn gs_mercator_width(_: &mut Vm, a: &mut [Value]) -> VR {
    let g = shape_of(&a[0])?.ok_or_else(gs_no_bbox)?;
    Ok(Value::Double(super::score::lon_to_spherical_mercator(g.bottom_right[1]) - super::score::lon_to_spherical_mercator(g.top_left[1])))
}
/// `latToSphericalMercator(top) - latToSphericalMercator(bottom)`.
fn gs_mercator_height(_: &mut Vm, a: &mut [Value]) -> VR {
    let g = shape_of(&a[0])?.ok_or_else(gs_no_bbox)?;
    Ok(Value::Double(super::score::lat_to_spherical_mercator(g.top_left[0]) - super::score::lat_to_spherical_mercator(g.bottom_right[0])))
}

fn gp_zero(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Double(0.0))
}

fn geo_new0(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(es_value(EsObj::GeoPoint(0.0, 0.0)))
}

fn geo_new2(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(es_value(EsObj::GeoPoint(crate::lib_lang::double(&a[0]), crate::lib_lang::double(&a[1]))))
}

fn geo_get_lat(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(geo_of(&a[0])?.0))
}

fn geo_get_lon(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(geo_of(&a[0])?.1))
}

fn bbox_top_left(_: &mut Vm, a: &mut [Value]) -> VR {
    match es(&a[0]) {
        Some(EsObj::BoundingBox(tl, _)) => Ok(es_value(EsObj::GeoPoint(tl.0, tl.1))),
        _ => Ok(Value::Null),
    }
}

fn bbox_bottom_right(_: &mut Vm, a: &mut [Value]) -> VR {
    match es(&a[0]) {
        Some(EsObj::BoundingBox(_, br)) => Ok(es_value(EsObj::GeoPoint(br.0, br.1))),
        _ => Ok(Value::Null),
    }
}

fn bytes_of(v: &Value) -> Result<Rc<[u8]>, Thrown> {
    match es(v) {
        Some(EsObj::BytesRef(b)) => Ok(b.clone()),
        _ => Err(crate::vm::npe("Cannot read field \"bytes\" because \"other\" is null")),
    }
}

fn br_bytes(_: &mut Vm, a: &mut [Value]) -> VR {
    let b = bytes_of(&a[0])?;
    Ok(Value::obj(Obj::Array(JArray::from_data(crate::types::Ty::BYTE, ArrayData::Byte(b.iter().map(|x| *x as i8).collect())))))
}

fn br_offset(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Int(0))
}

fn br_length(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(bytes_of(&a[0])?.len() as i32))
}

fn br_equals(_: &mut Vm, a: &mut [Value]) -> VR {
    let x = bytes_of(&a[0])?;
    let y = bytes_of(&a[1])?;
    Ok(Value::Bool(x == y))
}

fn br_utf8(_: &mut Vm, a: &mut [Value]) -> VR {
    let b = bytes_of(&a[0])?;
    Ok(Value::string(String::from_utf8_lossy(&b).into_owned()))
}

fn dv_vector_value(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = sdv_ref(&a[0])?;
    match &s.vector {
        Some(v) => Ok(v.to_array()),
        None => Err(es_error(
            cls::ILLEGAL_ARGUMENT_EXCEPTION,
            "A document doesn't have a value for a vector field!",
            &[
                "index.mapper.vectors.DenseVectorScriptDocValues.getCheckedVector(DenseVectorScriptDocValues.java:36)",
                "index.mapper.vectors.DenseVectorScriptDocValues.getVectorValue(DenseVectorScriptDocValues.java:45)",
            ],
        )),
    }
}

fn dv_magnitude(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = sdv_ref(&a[0])?;
    match &s.vector {
        Some(v) => Ok(Value::Float(v.magnitude())),
        None => Err(es_error(
            cls::ILLEGAL_ARGUMENT_EXCEPTION,
            "A document doesn't have a value for a vector field!",
            &[
                "index.mapper.vectors.DenseVectorScriptDocValues.getCheckedVector(DenseVectorScriptDocValues.java:36)",
                "index.mapper.vectors.DenseVectorScriptDocValues.getMagnitude(DenseVectorScriptDocValues.java:52)",
            ],
        )),
    }
}

fn doc_size(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(doc_unsupported("size", 186))
}
fn doc_is_empty(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(doc_unsupported("isEmpty", 191))
}
fn doc_contains_value(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(doc_unsupported("containsValue", 196))
}
fn doc_put(_: &mut Vm, a: &mut [Value]) -> VR {
    // the bridge method casts the value first
    if !matches!(es(&a[2]), Some(EsObj::Values(_))) && !a[2].is_null() {
        let from = crate::lookup::java_binary_name(a[2].class());
        let mut t = Throw::new(
            cls::CLASS_CAST_EXCEPTION,
            Some(format!(
                "class {from} cannot be cast to class org.elasticsearch.index.fielddata.ScriptDocValues ({from} is in module java.base of loader 'bootstrap'; org.elasticsearch.index.fielddata.ScriptDocValues is in module org.elasticsearch.server@9.5.5 of loader 'app')"
            )),
        );
        t.frames = server_frames(&["search.lookup.LeafDocLookup.put(LeafDocLookup.java:31)"]);
        return Err(t);
    }
    Err(doc_unsupported("put", 201))
}
fn doc_remove(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(doc_unsupported("remove", 206))
}
fn doc_put_all(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(doc_unsupported("putAll", 211))
}
fn doc_clear(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(doc_unsupported("clear", 216))
}
fn doc_key_set(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(doc_unsupported("keySet", 221))
}
fn doc_values(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(doc_unsupported("values", 226))
}
fn doc_entry_set(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(doc_unsupported("entrySet", 231))
}

/// `params._fields[name]`: the stored field's values.
pub fn stored_field(vm: &mut Vm, k: &Value) -> VR {
    let name = k.to_java_string();
    match vm.host.stored_field(&name) {
        Ok(Some(vals)) => Ok(es_value(EsObj::FieldLookup(Rc::from(vals)))),
        Ok(None) => Err(super::es_iae(
            format!("No field found for [{name}] in mapping"),
            &[
                "search.lookup.LeafStoredFieldsLookup.loadFieldData(LeafStoredFieldsLookup.java:118)",
                "search.lookup.LeafStoredFieldsLookup.get(LeafStoredFieldsLookup.java:47)",
                "search.lookup.LeafStoredFieldsLookup.get(LeafStoredFieldsLookup.java:21)",
            ],
        )),
        Err(e) => Err(host_error(e)),
    }
}

/// `LeafStoredFieldsLookup`'s unsupported `Map` methods.
pub fn stored_fields_unsupported(method: &str, line: u32) -> Thrown {
    let mut t = Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, None);
    t.frames = server_frames(&[&format!("search.lookup.LeafStoredFieldsLookup.{method}(LeafStoredFieldsLookup.java:{line})")]);
    t
}

fn fl_lookup(v: &Value) -> Result<Rc<[Value]>, Thrown> {
    match es(v) {
        Some(EsObj::FieldLookup(f)) => Ok(f.clone()),
        _ => Err(crate::vm::cce("not a FieldLookup")),
    }
}
fn flk_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(fl_lookup(&a[0])?.first().cloned().unwrap_or(Value::Null))
}
fn flk_values(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(crate::value::JList::array_list(fl_lookup(&a[0])?.to_vec()))
}
fn flk_is_empty(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(fl_lookup(&a[0])?.is_empty()))
}
fn sfl_size(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(stored_fields_unsupported("size", 65))
}

pub fn register(b: &mut Builder) {
    use cls::*;
    // params._fields
    b.supers(LEAF_STORED_FIELDS, &[OBJECT, MAP]);
    b.m(LEAF_STORED_FIELDS, "int size()", sfl_size);
    b.supers(FIELD_LOOKUP, &[OBJECT]);
    b.m(FIELD_LOOKUP, "def getValue()", flk_value);
    b.m(FIELD_LOOKUP, "List getValues()", flk_values);
    b.m(FIELD_LOOKUP, "boolean isEmpty()", flk_is_empty);
    // doc
    b.supers(DOC_MAP, &[OBJECT, MAP]);
    for (sig, f) in [
        ("int size()", doc_size as crate::lookup::Native),
        ("boolean isEmpty()", doc_is_empty),
        ("boolean containsValue(def)", doc_contains_value),
        ("def put(def,def)", doc_put),
        ("def remove(def)", doc_remove),
        ("void putAll(Map)", doc_put_all),
        ("void clear()", doc_clear),
        ("Set keySet()", doc_key_set),
        ("Collection values()", doc_values),
        ("Set entrySet()", doc_entry_set),
    ] {
        b.m(DOC_MAP, sig, f);
    }

    // ScriptDocValues
    b.supers(SCRIPT_DOC_VALUES, &[ABSTRACT_LIST]);
    for (sig, f) in [
        ("boolean add(def)", sdv_add as crate::lookup::Native),
        ("void add(int,def)", sdv_add_at),
        ("boolean addAll(Collection)", sdv_add_all),
        ("def set(int,def)", sdv_set),
        ("def remove(int)", sdv_remove_at),
        ("void replaceAll(UnaryOperator)", sdv_replace_all),
        ("void sort(Comparator)", sdv_sort),
        ("void clear()", sdv_clear),
        ("boolean removeIf(Predicate)", sdv_remove_if),
    ] {
        b.m(SCRIPT_DOC_VALUES, sig, f);
    }
    for c in [SDV_STRINGS, SDV_LONGS, SDV_DATES, SDV_DOUBLES, SDV_GEO_POINTS, SDV_BOOLEANS, SDV_BYTES_REFS, SDV_UNSIGNED_LONG, SDV_VERSION, SDV_DENSE_VECTOR, SDV_RANK_VECTORS] {
        b.supers(c, &[SCRIPT_DOC_VALUES]);
    }
    b.supers(SDV_IP, &[SCRIPT_DOC_VALUES]);
    b.interface(SDV_GEOMETRY, &[]);
    b.supers(SDV_GEO_SHAPE, &[SCRIPT_DOC_VALUES, SDV_GEOMETRY]);
    b.m(SDV_GEO_SHAPE, "def get(int)", sdv_get);
    b.m(SDV_GEO_SHAPE, "def getValue()", sdv_value);
    b.m(SDV_GEO_SHAPE, "int getDimensionalType()", gs_dim_type);
    b.m(SDV_GEO_SHAPE, "GeoPoint getCentroid()", gs_centroid);
    b.m(SDV_GEO_SHAPE, "GeoBoundingBox getBoundingBox()", gs_bbox);
    b.m(SDV_GEO_SHAPE, "GeoPoint getLabelPosition()", gs_label);
    b.m(SDV_GEO_SHAPE, "double getMercatorWidth()", gs_mercator_width);
    b.m(SDV_GEO_SHAPE, "double getMercatorHeight()", gs_mercator_height);
    b.supers(GEO_SHAPE_VALUE, &[OBJECT]);
    b.m(SDV_STRINGS, "String get(int)", sdv_get);
    b.m(SDV_STRINGS, "String getValue()", sdv_value);
    b.m(SDV_LONGS, "Long get(int)", sdv_get);
    b.m(SDV_LONGS, "long getValue()", sdv_value);
    b.m(SDV_DATES, "ZonedDateTime get(int)", sdv_get);
    b.m(SDV_DATES, "ZonedDateTime getValue()", sdv_value);
    b.m(SDV_DOUBLES, "Double get(int)", sdv_get);
    b.m(SDV_DOUBLES, "double getValue()", sdv_value);
    b.m(SDV_BOOLEANS, "Boolean get(int)", sdv_get);
    b.m(SDV_BOOLEANS, "boolean getValue()", sdv_value);
    b.m(SDV_BYTES_REFS, "BytesRef get(int)", sdv_get);
    b.m(SDV_BYTES_REFS, "BytesRef getValue()", sdv_value);
    b.m(SDV_UNSIGNED_LONG, "def get(int)", sdv_get);
    b.m(SDV_VERSION, "String get(int)", sdv_get);
    b.m(SDV_DENSE_VECTOR, "def get(int)", sdv_get);
    b.m(SDV_DENSE_VECTOR, "float[] getVectorValue()", dv_vector_value);
    b.m(SDV_DENSE_VECTOR, "float getMagnitude()", dv_magnitude);
    b.m(SDV_RANK_VECTORS, "def get(int)", sdv_get);
    b.m(SDV_RANK_VECTORS, "Iterator getVectorValues()", super::rank::sdv_vector_values);
    b.m(SDV_RANK_VECTORS, "float[] getMagnitudes()", super::rank::sdv_magnitudes);
    b.m(SDV_GEO_POINTS, "GeoPoint get(int)", sdv_get);
    b.m(SDV_GEO_POINTS, "GeoPoint getValue()", sdv_value);
    b.m(SDV_GEO_POINTS, "double getLat()", gp_lat);
    b.m(SDV_GEO_POINTS, "double getLon()", gp_lon);
    b.m(SDV_GEO_POINTS, "double[] getLats()", gp_lats);
    b.m(SDV_GEO_POINTS, "double[] getLons()", gp_lons);
    b.m(SDV_GEO_POINTS, "double arcDistance(double,double)", gp_arc);
    b.m(SDV_GEO_POINTS, "double arcDistanceWithDefault(double,double,double)", gp_arc_default);
    b.m(SDV_GEO_POINTS, "double planeDistance(double,double)", gp_plane);
    b.m(SDV_GEO_POINTS, "double planeDistanceWithDefault(double,double,double)", gp_plane_default);
    b.m(SDV_GEO_POINTS, "double geohashDistance(String)", gp_geohash);
    b.m(SDV_GEO_POINTS, "double geohashDistanceWithDefault(String,double)", gp_geohash_default);
    b.m(SDV_GEO_POINTS, "int getDimensionalType()", gp_dim_type);
    b.m(SDV_GEO_POINTS, "GeoPoint getCentroid()", gp_centroid);
    b.m(SDV_GEO_POINTS, "GeoBoundingBox getBoundingBox()", gp_bbox);
    b.m(SDV_GEO_POINTS, "GeoPoint getLabelPosition()", gp_centroid);
    b.m(SDV_GEO_POINTS, "double getMercatorWidth()", gp_zero);
    b.m(SDV_GEO_POINTS, "double getMercatorHeight()", gp_zero);

    // GeoPoint, GeoBoundingBox, BytesRef
    b.supers(GEO_POINT, &[OBJECT]);
    b.c(GEO_POINT, "()", geo_new0);
    b.c(GEO_POINT, "(double,double)", geo_new2);
    b.m(GEO_POINT, "double getLat()", geo_get_lat);
    b.m(GEO_POINT, "double getLon()", geo_get_lon);
    b.supers(GEO_BOUNDING_BOX, &[OBJECT]);
    b.m(GEO_BOUNDING_BOX, "GeoPoint topLeft()", bbox_top_left);
    b.m(GEO_BOUNDING_BOX, "GeoPoint bottomRight()", bbox_bottom_right);
    b.supers(BYTES_REF, &[OBJECT, COMPARABLE]);
    b.f(BYTES_REF, "byte[] bytes", br_bytes);
    b.f(BYTES_REF, "int offset", br_offset);
    b.f(BYTES_REF, "int length", br_length);
    b.m(BYTES_REF, "boolean bytesEquals(BytesRef)", br_equals);
    b.m(BYTES_REF, "String utf8ToString()", br_utf8);

    // the field API
    b.interface(FIELD, &[ITERABLE]);
    b.dynamic_type(FIELD);
    b.m(FIELD, "String getName()", fl_name);
    b.m(FIELD, "boolean isEmpty()", fl_is_empty);
    b.m(FIELD, "int size()", fl_size);
    b.supers(EMPTY_FIELD, &[OBJECT, FIELD]);
    b.dynamic_type(EMPTY_FIELD);
    b.m(EMPTY_FIELD, "def get(def)", fl_get1);
    b.m(EMPTY_FIELD, "def get(int,def)", fl_get2);
    b.supers(DOC_VALUES_FIELD, &[OBJECT, FIELD]);
    b.supers(DELEGATE_DOC_VALUES_FIELD, &[DOC_VALUES_FIELD]);
    b.supers(ABSTRACT_LONG_DOC_VALUES_FIELD, &[DOC_VALUES_FIELD]);
    b.supers(BASE_KEYWORD_DOC_VALUES_FIELD, &[DOC_VALUES_FIELD]);
    for c in [
        DOC_VALUES_FIELD,
        DELEGATE_DOC_VALUES_FIELD,
        ABSTRACT_LONG_DOC_VALUES_FIELD,
        BASE_KEYWORD_DOC_VALUES_FIELD,
        BINARY_DOC_VALUES_FIELD,
        BOOLEAN_DOC_VALUES_FIELD,
        INTEGER_DOC_VALUES_FIELD,
        LONG_DOC_VALUES_FIELD,
        DOUBLE_DOC_VALUES_FIELD,
        FLOAT_DOC_VALUES_FIELD,
        SCALED_FLOAT_DOC_VALUES_FIELD,
        HALF_FLOAT_DOC_VALUES_FIELD,
        BYTE_DOC_VALUES_FIELD,
        SHORT_DOC_VALUES_FIELD,
        DATE_MILLIS_DOC_VALUES_FIELD,
        DATE_NANOS_DOC_VALUES_FIELD,
        KEYWORD_DOC_VALUES_FIELD,
        FLATTENED_DOC_VALUES_FIELD,
        GEO_POINT_DOC_VALUES_FIELD,
        IP_DOC_VALUES_FIELD,
        SEQ_NO_DOC_VALUES_FIELD,
        VERSION_DOC_VALUES_FIELD,
        UNSIGNED_LONG_DOC_VALUES_FIELD,
        VERSION_STRING_DOC_VALUES_FIELD,
        DENSE_VECTOR_DOC_VALUES_FIELD,
        GEO_SHAPE_DOC_VALUES_FIELD,
    ] {
        b.dynamic_type(c);
    }
    for c in [BINARY_DOC_VALUES_FIELD, BOOLEAN_DOC_VALUES_FIELD, DOUBLE_DOC_VALUES_FIELD, FLOAT_DOC_VALUES_FIELD, SCALED_FLOAT_DOC_VALUES_FIELD] {
        b.supers(c, &[DOC_VALUES_FIELD]);
    }
    for c in [HALF_FLOAT_DOC_VALUES_FIELD, BYTE_DOC_VALUES_FIELD, SHORT_DOC_VALUES_FIELD, INTEGER_DOC_VALUES_FIELD, GEO_POINT_DOC_VALUES_FIELD, GEO_SHAPE_DOC_VALUES_FIELD] {
        b.supers(c, &[DOC_VALUES_FIELD]);
    }
    for c in [DATE_MILLIS_DOC_VALUES_FIELD, DATE_NANOS_DOC_VALUES_FIELD, IP_DOC_VALUES_FIELD, VERSION_STRING_DOC_VALUES_FIELD, DENSE_VECTOR_DOC_VALUES_FIELD, RANK_VECTORS_DOC_VALUES_FIELD] {
        b.supers(c, &[DOC_VALUES_FIELD]);
    }
    for c in [LONG_DOC_VALUES_FIELD, SEQ_NO_DOC_VALUES_FIELD, VERSION_DOC_VALUES_FIELD, UNSIGNED_LONG_DOC_VALUES_FIELD] {
        b.supers(c, &[ABSTRACT_LONG_DOC_VALUES_FIELD]);
    }
    for c in [KEYWORD_DOC_VALUES_FIELD, FLATTENED_DOC_VALUES_FIELD] {
        b.supers(c, &[BASE_KEYWORD_DOC_VALUES_FIELD]);
    }
    let typed: &[(ClassId, &str)] = &[
        (BINARY_DOC_VALUES_FIELD, "ByteBuffer"),
        (BOOLEAN_DOC_VALUES_FIELD, "boolean"),
        (INTEGER_DOC_VALUES_FIELD, "int"),
        (ABSTRACT_LONG_DOC_VALUES_FIELD, "long"),
        (DOUBLE_DOC_VALUES_FIELD, "double"),
        (FLOAT_DOC_VALUES_FIELD, "float"),
        (SCALED_FLOAT_DOC_VALUES_FIELD, "double"),
        (BYTE_DOC_VALUES_FIELD, "byte"),
        (SHORT_DOC_VALUES_FIELD, "short"),
        (DATE_MILLIS_DOC_VALUES_FIELD, "ZonedDateTime"),
        (DATE_NANOS_DOC_VALUES_FIELD, "ZonedDateTime"),
        (BASE_KEYWORD_DOC_VALUES_FIELD, "String"),
        (GEO_POINT_DOC_VALUES_FIELD, "GeoPoint"),
        (IP_DOC_VALUES_FIELD, "IPAddress"),
    ];
    for (c, t) in typed {
        let (ret, def) = match *t {
            "int" | "byte" | "short" => (*t, "int"),
            other => (other, other),
        };
        b.m(*c, &format!("{ret} get({def})"), fl_get1);
        b.m(*c, &format!("{ret} get(int,{def})"), fl_get2);
    }
    b.m(HALF_FLOAT_DOC_VALUES_FIELD, "float get(double)", fl_get1);
    b.m(HALF_FLOAT_DOC_VALUES_FIELD, "float get(int,double)", fl_get2);
    b.m(HALF_FLOAT_DOC_VALUES_FIELD, "List asDoubles()", fl_alts);
    b.m(HALF_FLOAT_DOC_VALUES_FIELD, "double asDouble(double)", fl_alt1);
    b.m(HALF_FLOAT_DOC_VALUES_FIELD, "double asDouble(int,double)", fl_alt2);
    b.m(IP_DOC_VALUES_FIELD, "List asStrings()", fl_alts);
    b.m(IP_DOC_VALUES_FIELD, "String asString(String)", fl_alt1);
    b.m(IP_DOC_VALUES_FIELD, "String asString(int,String)", fl_alt2);
    b.m(RANK_VECTORS_DOC_VALUES_FIELD, "RankVectors get()", fl_rank);
    b.m(RANK_VECTORS_DOC_VALUES_FIELD, "RankVectors get(RankVectors)", fl_rank_or);
    b.m(DENSE_VECTOR_DOC_VALUES_FIELD, "DenseVector get()", fl_vector);
    b.m(DENSE_VECTOR_DOC_VALUES_FIELD, "DenseVector get(DenseVector)", fl_vector_or);

    // context functions of document-based scripts
    b.s(DOC_BASED_SCRIPT, "Field field(String)", f_field);
    b.s(DOC_BASED_SCRIPT, "def $(String,def)", f_dollar);
}
