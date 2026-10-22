//! Elasticsearch's extensions to Painless: everything a script sees beyond the JDK.
//!
//! * [`doc`]: the `doc` map (`LeafDocLookup`), `ScriptDocValues` (`doc['f'].value`, `.size()`,
//!   `.empty`, dates as `ZonedDateTime`, `GeoPoint`, `BytesRef`, dense vectors) and the field
//!   API (`field('f').get(default)`, `$('f', default)`).
//! * [`score`]: the score-script functions (`saturation`, `sigmoid`, `randomScore`, the decay
//!   functions, `cosineSimilarity`/`dotProduct`/`l1norm`/`l2norm`/`hamming`), `_score`,
//!   `_termStats` and `explanation`.
//! * [`emit`]: `emit(...)` of runtime fields with Elasticsearch's value and character limits,
//!   and `parse(...)` of date runtime fields.
//! * [`write`]: `ctx` of update/ingest/reindex scripts with metadata validation, `metadata()`
//!   and the `WriteField`/`NestedDocument` API.
//! * [`misc`]: `Debug.explain`, `Json`, `String.sha1/sha256/sha512`, `CIDR`, `IPAddress`,
//!   `java.nio.ByteBuffer`, and the objects of similarity, interval and analysis scripts.
//! * [`movfn`]: `MovingFunctions` of `moving_fn` aggregations.
//!
//! Elasticsearch objects are [`EsObj`] values inside [`Obj::Es`]; the generic collection code
//! (`lib_coll`) reaches list- and map-like ones through the methods below.

pub mod dense;
pub mod doc;
pub mod emit;
pub mod extract;
pub mod misc;
pub mod movfn;
pub mod rank;
pub mod processors;
pub mod score;
pub mod uri;
pub mod vecmath;
pub mod write;

use std::cell::RefCell;
use std::rc::Rc;

use crate::contexts::{Context, EmitKind, Family};
use crate::lookup::{Builder, MethodId, Registry};
use crate::types::{cls, ClassId};
use crate::value::{identity_hash, JMap, Obj, Value};
use crate::vm::{Throw, Thrown, Vm, VR};

pub use doc::{DocField, Sdv};

/// Module prefix of Elasticsearch server frames in `script_stack`.
pub const SERVER: &str = "org.elasticsearch.server@9.5.5/";

/// Frames of Elasticsearch server code, innermost first.
pub fn server_frames(frames: &[&str]) -> Vec<String> {
    frames.iter().map(|f| format!("{SERVER}org.elasticsearch.{f}")).collect()
}

/// An exception thrown by Elasticsearch code (a JDK exception class: the `script_exception`
/// stays the root cause).
pub fn es_error(class: ClassId, msg: impl Into<String>, frames: &[&str]) -> Thrown {
    let mut t = Throw::new(class, Some(msg.into()));
    t.frames = server_frames(frames);
    t
}

/// An `IllegalArgumentException` from Elasticsearch code.
pub fn es_iae(msg: impl Into<String>, frames: &[&str]) -> Thrown {
    es_error(cls::ILLEGAL_ARGUMENT_EXCEPTION, msg, frames)
}

/// Elasticsearch objects exposed to scripts.
pub enum EsObj {
    /// `_score`, read from the host when the variable is loaded.
    LazyScore,
    /// `_termStats`, built from the host when the variable is loaded.
    LazyTermStats,
    /// The `doc` map of the current document.
    Doc,
    /// A `params` entry search contexts resolve per document (`doc`, `_source`, `_doc`,
    /// `_fields`).
    LazyParam(&'static str),
    /// `ScriptDocValues` of one field for the current document.
    Values(Sdv),
    /// `org.elasticsearch.common.geo.GeoPoint`.
    GeoPoint(f64, f64),
    /// `params._fields` of search scripts (`LeafStoredFieldsLookup`).
    StoredFields,
    /// A stored field's values (`FieldLookup`).
    FieldLookup(Rc<[Value]>),
    /// A `geo_shape` doc value: centroid, bounding box (top left, bottom right), label
    /// position, dimensional type.
    GeoShape(Rc<GeoShapeValue>),
    /// `GeoBoundingBox` (top left, bottom right).
    BoundingBox((f64, f64), (f64, f64)),
    /// `org.apache.lucene.util.BytesRef`.
    BytesRef(Rc<[u8]>),
    /// A field of the field API (`field('f')`), or `EmptyField` when unmapped.
    Field(DocField),
    /// `DenseVector` of a document (`None` is `DenseVector.EMPTY`).
    Vector(Option<dense::Vector>),
    /// `RankVectors` of a document (`None` is `RankVectors.EMPTY`).
    RankVectors(Option<Rc<rank::RankVec>>),
    /// `IPAddress`.
    Ip(std::net::IpAddr),
    /// `CIDR`.
    Cidr(misc::Cidr),
    /// A `java.nio` buffer: `ByteBuffer` over bytes (`binary` field values) or a typed view.
    ByteBuffer(Rc<misc::NioBuf>),
    /// `java.nio.ByteOrder` (true: big endian).
    ByteOrder(bool),
    /// `ScoreScript.ExplanationHolder`.
    Explanation,
    /// `ScriptTermStats`.
    TermStats(Rc<crate::host::TermStats>),
    /// `StatsSummary` (min, max, sum, count).
    StatsSummary(score::Summary),
    /// Objects of similarity, interval and analysis scripts.
    Similarity(misc::SimObj),
    /// `ctx` of write scripts: source and metadata with validation.
    Ctx(Rc<write::Ctx>),
    /// `metadata()` of write scripts.
    Metadata(Rc<write::Ctx>),
    /// `WriteField` of write scripts.
    WriteField(write::WriteField),
    /// `SourceMapField` of ingest conditions (read-only).
    SourceField(write::WriteField),
    /// `NamedGroupExtractor` (`grok`, `dissect`) of runtime fields.
    Extractor(std::sync::Arc<extract::Extractor>),
    /// `NestedDocument` of write scripts.
    Nested(write::Nested),
    /// `ScriptHeuristic.LongAccessor`: a mutable `Number` the aggregation updates per bucket.
    LongAccessor(std::cell::Cell<i64>),
}

impl EsObj {
    pub fn class(&self) -> ClassId {
        match self {
            EsObj::LazyScore => cls::DOUBLE_BOX,
            EsObj::LazyTermStats | EsObj::TermStats(_) => cls::SCRIPT_TERM_STATS,
            EsObj::Doc => cls::DOC_MAP,
            EsObj::LazyParam(_) => cls::OBJECT,
            EsObj::Values(s) => s.class,
            EsObj::GeoPoint(..) => cls::GEO_POINT,
            EsObj::GeoShape(_) => cls::GEO_SHAPE_VALUE,
            EsObj::StoredFields => cls::LEAF_STORED_FIELDS,
            EsObj::FieldLookup(_) => cls::FIELD_LOOKUP,
            EsObj::BoundingBox(..) => cls::GEO_BOUNDING_BOX,
            EsObj::BytesRef(_) => cls::BYTES_REF,
            EsObj::Field(f) => f.class,
            EsObj::Vector(Some(v)) => v.imp.class(),
            EsObj::Vector(None) => cls::DENSE_VECTOR_EMPTY,
            EsObj::RankVectors(Some(r)) => r.class(),
            EsObj::RankVectors(None) => cls::RANK_VECTORS_EMPTY,
            EsObj::Ip(_) => cls::IP_ADDRESS,
            EsObj::Cidr(_) => cls::CIDR,
            EsObj::ByteBuffer(b) => b.class(),
            EsObj::ByteOrder(_) => cls::BYTE_ORDER,
            EsObj::Explanation => cls::EXPLANATION_HOLDER,
            EsObj::StatsSummary(_) => cls::STATS_SUMMARY,
            EsObj::Similarity(s) => s.class(),
            EsObj::Ctx(_) => cls::CTX_MAP,
            EsObj::Metadata(_) => cls::METADATA,
            EsObj::WriteField(_) => cls::WRITE_FIELD,
            EsObj::SourceField(_) => cls::SOURCE_MAP_FIELD,
            EsObj::Extractor(_) => cls::NAMED_GROUP_EXTRACTOR,
            EsObj::Nested(_) => cls::NESTED_DOCUMENT,
            EsObj::LongAccessor(_) => cls::LONG_ACCESSOR,
        }
    }

    /// Elements when the object is a `List` (for `List.equals`).
    pub fn list_values(&self) -> Option<Vec<Value>> {
        match self {
            EsObj::Values(s) => Some(s.vals.clone()),
            _ => None,
        }
    }

    fn same(&self, other: &Value) -> bool {
        match other {
            Value::Obj(o) => match &**o {
                Obj::Es(e) => std::ptr::eq(self, e),
                _ => false,
            },
            _ => false,
        }
    }

    pub fn equals(&self, other: &Value) -> bool {
        match self {
            EsObj::Values(s) => match other.list_snapshot() {
                Some(b) => s.vals.len() == b.len() && s.vals.iter().zip(b.iter()).all(|(x, y)| x.java_equals(y)),
                None => false,
            },
            EsObj::GeoPoint(a, b) => match other {
                Value::Obj(o) => match &**o {
                    Obj::Es(EsObj::GeoPoint(c, d)) => a.to_bits() == c.to_bits() && b.to_bits() == d.to_bits(),
                    _ => false,
                },
                _ => false,
            },
            EsObj::BytesRef(a) => match other {
                Value::Obj(o) => matches!(&**o, Obj::Es(EsObj::BytesRef(b)) if a == b),
                _ => false,
            },
            EsObj::Ip(a) => match other {
                Value::Obj(o) => matches!(&**o, Obj::Es(EsObj::Ip(b)) if a == b),
                _ => false,
            },
            EsObj::ByteBuffer(a) => match other {
                Value::Obj(o) => matches!(&**o, Obj::Es(EsObj::ByteBuffer(b)) if a.equals(b)),
                _ => false,
            },
            EsObj::Ctx(c) => match other {
                Value::Obj(o) => match &**o {
                    Obj::Es(EsObj::Ctx(d)) => Rc::ptr_eq(c, d),
                    _ => false,
                },
                _ => false,
            },
            EsObj::Similarity(misc::SimObj::TermAttr(a)) => match other {
                Value::Obj(o) => matches!(&**o, Obj::Es(EsObj::Similarity(misc::SimObj::TermAttr(b))) if a.attribute_equals(b)),
                _ => false,
            },
            _ => self.same(other),
        }
    }

    pub fn java_hash(&self) -> i32 {
        match self {
            EsObj::Values(s) => {
                let mut h: i32 = 1;
                for v in &s.vals {
                    h = h.wrapping_mul(31).wrapping_add(v.java_hash());
                }
                h
            }
            EsObj::GeoPoint(lat, lon) => {
                let bits = |d: f64| if d != 0.0 { d.to_bits() as i64 } else { 0 };
                let a = bits(*lat);
                let r = (a ^ ((a as u64) >> 32) as i64) as i32;
                let b = bits(*lon);
                r.wrapping_mul(31).wrapping_add((b ^ ((b as u64) >> 32) as i64) as i32)
            }
            EsObj::BytesRef(b) => {
                // StringHelper.murmurhash3_x86_32 with the good-fast-hash seed is not stable
                // across JVMs; any content hash keeps equal refs equal
                b.iter().fold(0i32, |h, x| h.wrapping_mul(31).wrapping_add(*x as i8 as i32))
            }
            EsObj::Ip(a) => crate::java::string_hash(&misc::ip_to_string(a)),
            EsObj::ByteBuffer(b) => b.java_hash(),
            EsObj::Similarity(misc::SimObj::TermAttr(t)) => t.attribute_hash(),
            _ => identity_hash(self),
        }
    }

    pub fn write_string(&self, out: &mut String, depth: usize) {
        match self {
            EsObj::Values(s) => {
                out.push('[');
                for (i, v) in s.vals.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    v.write_java_string(out, depth + 1);
                }
                out.push(']');
            }
            EsObj::GeoPoint(lat, lon) => {
                out.push_str(&crate::java::fmt_double(*lat));
                out.push_str(", ");
                out.push_str(&crate::java::fmt_double(*lon));
            }
            EsObj::BoundingBox(tl, br) => {
                out.push_str(&format!(
                    "BBOX ({}, {}, {}, {})",
                    crate::java::fmt_double(tl.1),
                    crate::java::fmt_double(br.1),
                    crate::java::fmt_double(tl.0),
                    crate::java::fmt_double(br.0)
                ));
            }
            EsObj::BytesRef(b) => {
                out.push('[');
                for (i, x) in b.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    out.push_str(&format!("{x:x}"));
                }
                out.push(']');
            }
            EsObj::Ip(a) => out.push_str(&misc::ip_to_string(a)),
            EsObj::Similarity(misc::SimObj::TermAttr(t)) => out.push_str(&t.term),
            EsObj::LongAccessor(v) => out.push_str(&v.get().to_string()),
            EsObj::ByteBuffer(b) => b.write_string(out),
            EsObj::ByteOrder(big) => out.push_str(if *big { "BIG_ENDIAN" } else { "LITTLE_ENDIAN" }),
            EsObj::StatsSummary(s) => out.push_str(&s.to_java_string()),
            EsObj::Ctx(c) => c.write_string(out, depth),
            _ => {
                out.push_str(crate::lookup::java_binary_name(self.class()));
                out.push('@');
                out.push_str(&format!("{:x}", identity_hash(self) as u32));
            }
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            EsObj::Values(s) => serde_json::Value::Array(s.vals.iter().map(|v| v.to_json()).collect()),
            EsObj::GeoPoint(lat, lon) => serde_json::json!({ "lat": lat, "lon": lon }),
            EsObj::Ip(a) => serde_json::Value::String(misc::ip_to_string(a)),
            EsObj::Ctx(c) => c.to_json(),
            _ => {
                let mut s = String::new();
                self.write_string(&mut s, 0);
                serde_json::Value::String(s)
            }
        }
    }

    pub fn is_map(&self) -> bool {
        matches!(self, EsObj::Doc | EsObj::Ctx(_) | EsObj::StoredFields)
    }

    pub fn is_list(&self) -> bool {
        matches!(self, EsObj::Values(_))
    }

    /// Elements when iterated (`for (x : obj)`, streams, `forEach`).
    pub fn iter_values(&self, vm: &mut Vm) -> Result<Option<Vec<Value>>, Thrown> {
        match self {
            EsObj::Values(s) => {
                if s.kind == crate::host::DocKind::RankVectors && !s.vals.is_empty() {
                    return Err(rank::get_error());
                }
                if s.kind == crate::host::DocKind::DenseVector && !s.vals.is_empty() {
                    let mut t = doc::vector_get_error();
                    t.frames.push("java.base/java.util.AbstractList$Itr.next(AbstractList.java:373)".into());
                    return Err(t);
                }
                Ok(Some(s.vals.clone()))
            }
            EsObj::Field(f) => Ok(Some(f.vals.clone())),
            EsObj::WriteField(f) | EsObj::SourceField(f) => Ok(Some(f.values(vm)?)),
            EsObj::Doc => Err(doc::doc_unsupported("entrySet", 231)),
            EsObj::Ctx(c) => Ok(Some(c.entries().into_iter().map(|(k, _)| k).collect())),
            _ => Ok(None),
        }
    }

    pub fn size(&self, vm: &mut Vm) -> Result<usize, Thrown> {
        match self {
            EsObj::Values(s) => Ok(s.vals.len()),
            EsObj::Field(f) => Ok(f.vals.len()),
            EsObj::Doc => Err(doc::doc_unsupported("size", 186)),
            EsObj::StoredFields => Err(doc::stored_fields_unsupported("size", 65)),
            EsObj::Ctx(c) => Ok(c.entries().len()),
            EsObj::WriteField(f) => Ok(f.values(vm)?.len()),
            _ => Ok(0),
        }
    }

    pub fn list_get(&self, vm: &mut Vm, i: i32) -> VR {
        let _ = vm;
        match self {
            EsObj::Values(s) => s.get_through_list(i),
            _ => Err(crate::vm::cce("not a list")),
        }
    }

    pub fn map_get(&self, vm: &mut Vm, k: &Value) -> VR {
        match self {
            EsObj::Doc => doc::doc_get(vm, k),
            EsObj::StoredFields => doc::stored_field(vm, k),
            EsObj::Ctx(c) => Ok(c.get(k)),
            _ => Ok(Value::Null),
        }
    }

    pub fn map_put(&self, vm: &mut Vm, k: Value, v: Value) -> VR {
        let _ = vm;
        match self {
            EsObj::Doc => Err(doc::doc_unsupported("put", 201)),
            EsObj::Ctx(c) => c.put(k, v),
            _ => Err(crate::vm::uoe()),
        }
    }

    pub fn map_contains(&self, vm: &mut Vm, k: &Value) -> Result<bool, Thrown> {
        match self {
            EsObj::Doc => Ok(doc::doc_contains(vm, k)),
            EsObj::StoredFields => Ok(matches!(vm.host.stored_field(&k.to_java_string()), Ok(Some(_)))),
            EsObj::Ctx(c) => Ok(c.contains(k)),
            _ => Ok(false),
        }
    }

    pub fn map_entries(&self, vm: &mut Vm) -> Result<Vec<(Value, Value)>, Thrown> {
        let _ = vm;
        match self {
            EsObj::Doc => Err(doc::doc_unsupported("entrySet", 231)),
            EsObj::Ctx(c) => Ok(c.entries()),
            _ => Ok(Vec::new()),
        }
    }

    pub fn compare(&self, other: &Value) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (EsObj::ByteBuffer(a), Value::Obj(o)) => match &**o {
                Obj::Es(EsObj::ByteBuffer(b)) => a.compare_to(b).map(|c| c.cmp(&0)),
                _ => None,
            },
            _ => None,
        }
    }
}

/// The implementation a typed instance call runs: Elasticsearch objects behind an interface
/// type (`Map doc`, `List` doc values, `Field`) dispatch to their class's override, as the JVM's
/// virtual calls do.
#[inline]
pub fn virtual_imp(reg: &Registry, m: MethodId, args: &[Value]) -> crate::lookup::Native {
    let mi = reg.method(m);
    if let Some(Value::Obj(o)) = args.first()
        && let Obj::Es(e) = &**o
            && mi.kind == crate::lookup::MethodKind::Instance {
                let c = e.class();
                if c != mi.owner
                    && let Some(m2) = reg.lookup_method(c, false, &mi.name, mi.params.len()) {
                        return reg.method(m2).imp;
                    }
            }
    mi.imp
}

/// The `EsObj` inside a value, if any.
pub fn es(v: &Value) -> Option<&EsObj> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Es(e) => Some(e),
            _ => None,
        },
        _ => None,
    }
}

pub fn es_value(e: EsObj) -> Value {
    Value::obj(Obj::Es(e))
}

// ------------------------------------------------------------------------------ context functions

/// The class whose static methods are a family's context functions.
fn family_class(fam: Family) -> ClassId {
    match fam {
        Family::DocBased => cls::DOC_BASED_SCRIPT,
        Family::Score => cls::SCORE_SCRIPT,
        Family::Write => cls::WRITE_SCRIPT,
        Family::Conditional => cls::INGEST_CONDITIONAL_SCRIPT,
        Family::Emit(k) => match k {
            EmitKind::Boolean => cls::BOOLEAN_FIELD_SCRIPT,
            EmitKind::Date => cls::DATE_FIELD_SCRIPT,
            EmitKind::Double => cls::DOUBLE_FIELD_SCRIPT,
            EmitKind::GeoPoint => cls::GEO_POINT_FIELD_SCRIPT,
            EmitKind::Geometry => cls::GEOMETRY_FIELD_SCRIPT,
            EmitKind::Ip => cls::IP_FIELD_SCRIPT,
            EmitKind::Keyword => cls::STRING_FIELD_SCRIPT,
            EmitKind::Long => cls::LONG_FIELD_SCRIPT,
            EmitKind::Composite => cls::COMPOSITE_FIELD_SCRIPT,
        },
    }
}

/// A context function (`emit`, `field`, `metadata`, decay functions, …) by name and arity.
pub fn context_method(reg: &Registry, fam: Family, name: &str, arity: usize) -> Option<MethodId> {
    reg.lookup_method(family_class(fam), true, name, arity)
}

/// Whether a context function is a class binding with per-call-site state (Elasticsearch
/// constructs the binding on the first call with the leading arguments).
pub fn is_binding(reg: &Registry, m: MethodId) -> bool {
    let mi = reg.method(m);
    mi.owner == cls::SCORE_SCRIPT && score::BINDINGS.contains(&&*mi.name)
}

/// Whether some subclass of a `@dynamic_type` class has the method (calls then dispatch at
/// run time, like Elasticsearch's dynamic-type lookups).
pub fn has_subclass_method(reg: &Registry, c: ClassId, name: &str, arity: usize) -> bool {
    reg.classes.iter().any(|ci| ci.id != c && ci.ancestors.contains(&c) && ci.methods.contains_key(&(Box::<str>::from(name), arity as u8)))
}

/// Resolves a `params` value of a search context: lazy entries read the current document.
pub fn resolve_param(vm: &mut Vm, v: Value) -> VR {
    let key = match es(&v) {
        Some(EsObj::LazyParam(k)) => *k,
        _ => return Ok(v),
    };
    Ok(match key {
        "_source" => vm.host.source().unwrap_or(Value::Null),
        "doc" | "_doc" => es_value(EsObj::Doc),
        _ => es_value(EsObj::StoredFields),
    })
}

/// Value of a lazily bound context variable when it is loaded.
pub fn load_lazy(vm: &mut Vm, e: &EsObj) -> Option<Value> {
    match e {
        EsObj::LazyScore => Some(Value::Double(vm.host.score())),
        EsObj::LazyTermStats => Some(es_value(EsObj::TermStats(Rc::new(vm.host.term_stats().unwrap_or_default())))),
        _ => None,
    }
}

/// `doc['field'].value|.size()|.empty` with a literal field name, read straight from the
/// host's doc values.
pub fn doc_fast(vm: &mut Vm, field: u32, what: crate::vm::DocWhat) -> VR {
    doc::doc_fast(vm, field, what)
}

/// Fills the doc-values buffer of a literal `doc['field']` for the current document.
pub use doc::doc_fetch;

/// Initial value of a context variable before the host binds it (`doc`, `_score`, …).
pub fn default_var(ctx: Context, name: &str) -> Value {
    match name {
        "params" => Value::Map(Rc::new(JMap::hash_map())),
        "doc" if !matches!(ctx, Context::Similarity) => es_value(EsObj::Doc),
        "_score" => es_value(EsObj::LazyScore),
        "_termStats" => es_value(EsObj::LazyTermStats),
        "explanation" => es_value(EsObj::Explanation),
        "state" if matches!(ctx, Context::AggsMap | Context::AggsCombine) => Value::Map(Rc::new(JMap::hash_map())),
        "states" => crate::value::JList::array_list(Vec::new()),
        _ => Value::Null,
    }
}

/// Wraps `params` for a context. Contexts that hand scripts `Script.getParams()` directly see
/// an unmodifiable map; search contexts copy it into a mutable map that also resolves `_source`
/// and `doc` lazily through the host.
pub fn wrap_params(ctx: Context, v: Value) -> Value {
    use crate::contexts::Context::*;
    let Value::Map(m) = &v else { return v };
    match ctx {
        Score | Filter | Field | Aggs | AggsMap | NumberSort | StringSort | BytesRefSort | TermsSet | BooleanField | DateField | DoubleField
        | GeoPointField | GeometryField | IpField | KeywordField | LongField | CompositeField => {
            // `new HashMap<>(params)` sizes the copy for its entries
            let mut n = JMap::new(cls::HASH_MAP);
            n.presize_for_copy(m.len(), false);
            for (k, x) in m.entries() {
                n.insert_raw(k, x);
            }
            // what LeafSearchLookup.asMap() / AbstractFieldScript add to the user's params
            let lazy: &[&'static str] = match ctx {
                Filter => &[],
                BooleanField | DateField | DoubleField | GeoPointField | GeometryField | IpField | KeywordField | LongField | CompositeField => &["_source", "_fields"],
                _ => &["doc", "_source", "_doc", "_fields"],
            };
            for k in lazy {
                n.insert_raw(Value::str(k), es_value(EsObj::LazyParam(k)));
            }
            n.lazy_host = !lazy.is_empty();
            Value::Map(Rc::new(n))
        }
        // `ScriptHeuristic` hands the script its own `HashMap` (user params and accessors)
        AggsInit | AggsCombine | AggsReduce | ScriptHeuristic => v,
        _ => {
            let n = JMap::new(cls::UNMODIFIABLE_MAP);
            for (k, x) in m.entries() {
                n.data.borrow_mut().m.insert(crate::value::Key(k), x);
            }
            Value::Map(Rc::new(n))
        }
    }
}

pub fn register(b: &mut Builder) {
    use cls::*;
    for c in [DOC_BASED_SCRIPT, SCORE_SCRIPT, WRITE_SCRIPT, BOOLEAN_FIELD_SCRIPT, DATE_FIELD_SCRIPT, DOUBLE_FIELD_SCRIPT, GEO_POINT_FIELD_SCRIPT] {
        b.supers(c, &[OBJECT]);
    }
    for c in [GEOMETRY_FIELD_SCRIPT, IP_FIELD_SCRIPT, STRING_FIELD_SCRIPT, LONG_FIELD_SCRIPT, COMPOSITE_FIELD_SCRIPT] {
        b.supers(c, &[OBJECT]);
    }
    doc::register(b);
    score::register(b);
    emit::register(b);
    write::register(b);
    misc::register(b);
    movfn::register(b);
    extract::register(b);
    processors::register(b);
}

/// Per-call-site state of a class binding, created on the first call.
pub fn binding<T: 'static>(vm: &mut Vm, init: impl FnOnce(&mut Vm) -> Result<T, Thrown>) -> Result<Rc<T>, Thrown> {
    let slot = vm.st.cur_binding as usize;
    if let Some(Some(b)) = vm.st.bindings.get(slot)
        && let Some(v) = b.downcast_ref::<Rc<T>>() {
            return Ok(v.clone());
        }
    let loc = vm.st.binding_loc;
    let v = Rc::new(init(vm).map_err(|mut t| {
        if t.offset.is_none() {
            t.offset = Some(loc);
        }
        t
    })?);
    if let Some(s) = vm.st.bindings.get_mut(slot) {
        *s = Some(Box::new(v.clone()));
    }
    Ok(v)
}

/// What scripts read of a `geo_shape` doc value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoShapeValue {
    pub centroid: [f64; 2],
    pub top_left: [f64; 2],
    pub bottom_right: [f64; 2],
    pub label: [f64; 2],
    pub dimensional_type: i32,
}

/// A `RefCell` helper for objects that are shared but mutated by scripts.
pub type Shared<T> = Rc<RefCell<T>>;
