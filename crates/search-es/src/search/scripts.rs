//! Painless in search: a [`DocHost`] serving a script `doc['field']` values (from doc values,
//! typed as Elasticsearch's `ScriptDocValues`), `params._source`, `_score`, the doc id and the
//! shard; and the engine's script hooks over it — filter scripts (`script` query), score scripts
//! (`script_score`, `function_score` `script_score`) — plus script fields.
//!
//! A compiled `Script` is shared (`Arc`); each segment gets its own `Instance` (Painless keeps
//! per-instance state and specializes on the first document). Instances are not `Send` (Painless
//! values are reference counted without atomics): a leaf creates its instance on the thread
//! that runs the segment and keeps it there, which is how the engine drives leaves.

use crate::engine::fields::{FieldTable, Kind};
use crate::error::EsResult;
use crate::scripting::Bound;
use search_codec::points::encode;
use search_index::query::function_score::LeafScoreScript;
use search_index::query::{FilterScript, LeafFilterScript, ScoreScript};
use search_index::SegmentContext;
use search_painless::host::{DocKind, DocValues, Host, HostError};
use std::sync::Arc;

/// Per-document data for one segment.
pub struct DocHost {
    pub table: Arc<FieldTable>,
    pub seg: SegmentContext,
    pub doc: u32,
    pub score: f64,
    pub shard: i32,
    pub index: String,
    buf: Vec<u8>,
}

impl DocHost {
    pub fn new(table: Arc<FieldTable>, seg: SegmentContext, shard: i32, index: String) -> DocHost {
        DocHost { table, seg, doc: 0, score: 0.0, shard, index, buf: Vec::new() }
    }
}

fn kind_of(k: Kind) -> Option<DocKind> {
    Some(match k {
        Kind::Long | Kind::TokenCount => DocKind::Long,
        Kind::Integer => DocKind::Integer,
        Kind::Short => DocKind::Short,
        Kind::Byte => DocKind::Byte,
        Kind::UnsignedLong => DocKind::UnsignedLong,
        Kind::Double => DocKind::Double,
        Kind::Float => DocKind::Float,
        Kind::HalfFloat => DocKind::HalfFloat,
        Kind::ScaledFloat(_) => DocKind::ScaledFloat,
        Kind::Keyword | Kind::ConstantKeyword | Kind::Wildcard | Kind::Flattened | Kind::CountedKeyword | Kind::Text | Kind::MatchOnlyText => DocKind::String,
        Kind::Version => DocKind::Version,
        Kind::Boolean => DocKind::Boolean,
        Kind::Date => DocKind::Date,
        Kind::DateNanos => DocKind::DateNanos,
        Kind::GeoPoint => DocKind::GeoPoint,
        Kind::Ip => DocKind::Ip,
        Kind::Binary => DocKind::Binary,
        Kind::DenseVector => DocKind::DenseVector,
        _ => return None,
    })
}

impl Host for DocHost {
    fn score(&mut self) -> f64 {
        self.score
    }

    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        let table = self.table.clone();
        let Some(f) = table.get(field) else {
            return match field {
                "_id" => Err(HostError::illegal_argument("Fielddata access on the _id field is disallowed, you can re-enable it by updating the dynamic cluster setting: indices.id_field_data.enabled")),
                _ => Ok(false),
            };
        };
        let Some(kind) = kind_of(f.kind) else {
            return Err(HostError::illegal_argument(format!("Field [{}] of type [{}] doesn't support scripts", f.name, f.ty)));
        };
        out.clear(kind);
        let r = &self.seg.reader;
        let d = self.doc;
        match f.kind {
            Kind::Text | Kind::MatchOnlyText => {
                let Some(fd) = f.subfield("fielddata") else {
                    return Err(HostError::illegal_argument(format!(
                        "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{}] in order to load field data by uninverting the inverted index. Note that this can use significant memory.",
                        f.name
                    )));
                };
                if let Some(dv) = r.sorted_set(fd) {
                    for o in dv.ords(d) {
                        self.buf.clear();
                        dv.lookup_ord(o, &mut self.buf);
                        out.push_str(&String::from_utf8_lossy(&self.buf));
                    }
                }
            }
            Kind::ConstantKeyword => {
                if let Some(c) = &f.constant_value {
                    out.push_str(c);
                }
            }
            Kind::Keyword | Kind::Wildcard | Kind::Flattened | Kind::Version | Kind::CountedKeyword => {
                if let Some(dv) = r.sorted_set(f.number) {
                    for o in dv.ords(d) {
                        self.buf.clear();
                        dv.lookup_ord(o, &mut self.buf);
                        out.push_str(&String::from_utf8_lossy(&self.buf));
                    }
                }
            }
            Kind::Ip => {
                if let Some(dv) = r.sorted_set(f.number) {
                    for o in dv.ords(d) {
                        self.buf.clear();
                        dv.lookup_ord(o, &mut self.buf);
                        out.push_str(&search_index::query::range::inet_to_string(&self.buf));
                    }
                }
            }
            Kind::GeoPoint => {
                if let Some(dv) = r.sorted_numeric(f.number) {
                    for v in dv.values(d) {
                        out.geo.push([encode::decode_latitude((v >> 32) as i32), encode::decode_longitude(v as i32)]);
                    }
                }
            }
            Kind::Double => {
                if let Some(dv) = r.sorted_numeric(f.number) {
                    out.doubles.extend(dv.values(d).map(encode::sortable_f64));
                }
            }
            Kind::Float => {
                if let Some(dv) = r.sorted_numeric(f.number) {
                    out.doubles.extend(dv.values(d).map(|v| encode::sortable_f32(v as i32) as f64));
                }
            }
            Kind::HalfFloat => {
                if let Some(dv) = r.sorted_numeric(f.number) {
                    out.doubles.extend(dv.values(d).map(|v| encode::sortable_f16(v as i16) as f64));
                }
            }
            Kind::ScaledFloat(sf) => {
                if let Some(dv) = r.sorted_numeric(f.number) {
                    out.doubles.extend(dv.values(d).map(|v| v as f64 / sf));
                }
            }
            Kind::UnsignedLong => {
                if let Some(dv) = r.sorted_numeric(f.number) {
                    out.longs.extend(dv.values(d).map(|v| ((v as u64) ^ (1 << 63)) as i64));
                }
            }
            Kind::Binary => {
                if let Some(dv) = r.binary(f.number)
                    && let Some(b) = dv.get(d)
                {
                    out.bytes.extend(crate::engine::convert::decode_binary_dv(b).into_iter().map(<[u8]>::to_vec));
                }
            }
            Kind::DenseVector => {
                let spec = f.vector;
                out.element = match spec.map(|s| s.element) {
                    Some(search_codec::VectorElement::Byte) => search_painless::VectorElement::Byte,
                    Some(search_codec::VectorElement::Bit) => search_painless::VectorElement::Bit,
                    _ => search_painless::VectorElement::Float,
                };
                let raw: Option<Vec<u8>> = if f.index {
                    r.vectors(f.number).and_then(|v| v.ord(d).map(|o| v.vector(o).to_vec()))
                } else {
                    out.vector_binary = true;
                    r.binary(f.number).and_then(|b| b.get(d).map(|x| x.to_vec()))
                };
                if let Some(raw) = raw {
                    match out.element {
                        search_painless::VectorElement::Float | search_painless::VectorElement::BFloat16 => {
                            out.vector.extend(raw.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])));
                        }
                        _ => out.vector_bytes.extend(raw.iter().map(|b| *b as i8)),
                    }
                }
            }
            _ => {
                if let Some(dv) = r.sorted_numeric(f.number) {
                    out.longs.extend(dv.values(d));
                }
            }
        }
        Ok(true)
    }

    fn has_field(&mut self, field: &str) -> bool {
        self.table.get(field).is_some()
    }

    fn source(&mut self) -> Option<search_painless::Value> {
        self.seg.ensure_stored().ok()?;
        let stored = self.seg.reader.stored().document(self.doc).ok()?;
        let b = crate::fetch::source_bytes(&stored)?;
        let s = std::str::from_utf8(b).ok()?;
        search_painless::xcontent::parse_source(s).ok()
    }

    fn doc_id(&mut self) -> i32 {
        (self.seg.base + self.doc) as i32
    }

    fn shard(&mut self) -> (i32, String) {
        (self.shard, self.index.clone())
    }
}

/// An instance bound to a segment, with its host.
pub struct Leaf {
    // Field order matters: the instance borrows `script` and must drop first.
    inst: search_painless::Instance<'static>,
    host: DocHost,
    _script: Arc<search_painless::Script>,
}

// SAFETY: a leaf is created by the thread that runs its segment and only used there; the
// instance's reference-counted values are never shared with another live object.
unsafe impl Send for Leaf {}

#[allow(clippy::result_large_err)]
impl Leaf {
    pub fn new(script: &Arc<search_painless::Script>, params: &serde_json::Value, host: DocHost) -> Leaf {
        let s = script.clone();
        // SAFETY: `_script` keeps the script alive for as long as the instance exists.
        let inst: search_painless::Instance<'static> = unsafe { std::mem::transmute(s.instance()) };
        let mut leaf = Leaf { inst, host, _script: s };
        leaf.inst.set_params(params);
        leaf
    }

    pub fn run(&mut self, doc: u32, score: f64) -> Result<search_painless::Value, search_painless::ScriptError> {
        self.host.doc = doc;
        self.host.score = score;
        self.inst.execute(&mut self.host)
    }

    pub fn run_f64(&mut self, doc: u32, score: f64) -> Result<f64, search_painless::ScriptError> {
        self.host.doc = doc;
        self.host.score = score;
        self.inst.execute_f64(&mut self.host)
    }

    pub fn run_bool(&mut self, doc: u32) -> Result<bool, search_painless::ScriptError> {
        self.host.doc = doc;
        self.inst.execute_bool(&mut self.host)
    }

    pub fn set_var(&mut self, name: &str, v: search_painless::Value) {
        self.inst.set_var(name, v);
    }

    pub fn explanation(&self) -> Option<String> {
        self.inst.explanation().map(str::to_string)
    }

    /// Runs a runtime-field script for `doc`; its values are in [`Leaf::emitted`] /
    /// [`Leaf::emitted_fields`].
    pub fn run_emit(&mut self, doc: u32) -> Result<(), search_painless::ScriptError> {
        self.host.doc = doc;
        self.inst.execute(&mut self.host).map(|_| ())
    }

    pub fn emitted(&self) -> &[search_painless::Value] {
        self.inst.emitted()
    }

    pub fn emitted_fields(&self) -> &[(String, Vec<search_painless::Value>)] {
        self.inst.emitted_fields()
    }

    pub fn set_field_name(&mut self, name: &str) {
        self.inst.set_field_name(name);
    }
}

fn script_err(e: search_painless::ScriptError) -> search_index::Error {
    let ee = crate::scripting::script_error(e);
    let reason = format!("{}{}", ee.reason(), park_script_error(ee.clone()));
    search_index::Error::Es { kind: std::borrow::Cow::Borrowed("script_exception"), reason, status: ee.status }
}

/// Separates a parked error's ticket from the reason text the engine carries.
const TICKET: char = '\u{1f}';

/// Full script errors (`script_stack`, `position`, `caused_by`) parked while the engine carries
/// only a type and a reason; the reason gets a ticket that [`take_script_error`] redeems. Script
/// failures can surface on any engine thread, so the slab is global; it is bounded (the oldest
/// tickets go first) in case an engine error is dropped without being converted.
static PARKED: std::sync::Mutex<std::collections::VecDeque<(u64, crate::error::EsError)>> = std::sync::Mutex::new(std::collections::VecDeque::new());
static NEXT_TICKET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
const MAX_PARKED: usize = 256;

fn park_script_error(e: crate::error::EsError) -> String {
    let id = NEXT_TICKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut q = PARKED.lock().unwrap_or_else(|p| p.into_inner());
    if q.len() >= MAX_PARKED {
        q.pop_front();
    }
    q.push_back((id, e));
    format!("{TICKET}{id}")
}

/// The full script error behind an engine error reason (and the reason without its ticket).
pub fn take_script_error(reason: &str) -> (Option<crate::error::EsError>, &str) {
    let Some((text, id)) = reason.rsplit_once(TICKET) else { return (None, reason) };
    let Ok(id) = id.parse::<u64>() else { return (None, reason) };
    let mut q = PARKED.lock().unwrap_or_else(|p| p.into_inner());
    let found = q.iter().position(|(i, _)| *i == id).and_then(|i| q.remove(i)).map(|(_, e)| e);
    (found, text)
}

/// A painless script bound to an index for engine hooks.
pub struct Painless {
    pub bound: Bound,
    pub table: Arc<FieldTable>,
    pub shard: i32,
    pub index: String,
}

impl Painless {
    fn leaf_of(&self, seg: &SegmentContext) -> search_index::Result<Leaf> {
        let s = self.bound.painless().ok_or_else(|| search_index::Error::illegal("not a painless script"))?;
        Ok(Leaf::new(s, &self.bound.params, DocHost::new(self.table.clone(), seg.clone(), self.shard, self.index.clone())))
    }
}

struct FilterLeaf(Leaf);

impl LeafFilterScript for FilterLeaf {
    fn matches(&mut self, doc: u32) -> search_index::Result<bool> {
        self.0.run_bool(doc).map_err(script_err)
    }
}

impl FilterScript for Painless {
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafFilterScript>> {
        Ok(Box::new(FilterLeaf(self.leaf_of(seg)?)))
    }
    fn describe(&self) -> String {
        self.bound.describe.clone()
    }
}

struct ScoreLeaf(Leaf);

impl LeafScoreScript for ScoreLeaf {
    fn score(&mut self, doc: u32, sub_score: f32) -> search_index::Result<f64> {
        self.0.run_f64(doc, sub_score as f64).map_err(script_err)
    }
    fn explanation(&mut self) -> Option<String> {
        self.0.explanation()
    }
}

impl ScoreScript for Painless {
    fn needs_score(&self) -> bool {
        self.bound.painless().is_some_and(|s| s.needs_score())
    }
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafScoreScript>> {
        Ok(Box::new(ScoreLeaf(self.leaf_of(seg)?)))
    }
    fn describe(&self) -> String {
        self.bound.describe.clone()
    }
}

/// A script value as the response renders it.
pub fn to_value(v: &search_painless::Value) -> crate::json::Value {
    crate::scripting::from_serde(&v.to_json())
}

/// `script_fields` of one hit.
pub fn script_field(p: &Painless, seg: &SegmentContext, local: u32, score: f32) -> EsResult<Vec<crate::json::Value>> {
    let mut leaf = p.leaf_of(seg).map_err(crate::engine::convert::index_error)?;
    let v = leaf.run(local, score as f64).map_err(crate::scripting::script_error)?;
    let j = to_value(&v);
    Ok(match j {
        crate::json::Value::Arr(a) => a,
        other => vec![other],
    })
}
