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
    /// `doc['_id']` reads `_id` fielddata (OpenSearch with `indices.id_field_data.enabled`;
    /// Elasticsearch's script lookups always refuse it). Defaults to the dialect.
    pub id_fielddata: bool,
    buf: Vec<u8>,
    /// Fields this script read, resolved once: a script reads the same few fields on every
    /// document, so the per-document cost is a short scan instead of a hashed table lookup.
    resolved: Vec<(Box<str>, Arc<crate::engine::fields::FieldInfo>)>,
    /// The script's literal doc fields ([`Host::doc_values_at`] slots), resolved on first use:
    /// numeric-like fields keep their reader, so a read is the values of one doc, without a
    /// name or field-number lookup.
    slots: Vec<Slot>,
    /// Values of single-valued numeric slots decoded ahead for a run of consecutive docs
    /// ([`DocHost::prefetch`]): `cols[slot]` holds docs `block_start..block_start + len`.
    cols: Vec<Option<Col>>,
    block_start: u32,
    /// Scripts a search runs have no loop statement limit and poll `ctl` instead
    /// (`ContextIndexSearcher::checkCancelled`); the `_execute` API's don't.
    cancellable: bool,
    /// The search's deadline / cancellation, as it was when the host was created.
    ctl: Option<search_index::search::SearchControl>,
    /// `_termStats` of a score script over its query's terms ([`super::term_stats`]).
    pub term_stats: Option<Box<super::term_stats::SegTermStats>>,
    /// The search's runtime fields, which `doc[...]` reads like mapped fields.
    pub runtime: Option<Arc<super::runtime::RuntimeScope>>,
    /// The runtime fields being resolved for this script, outermost first
    /// (`SearchLookup.fieldChain`): its own field when it is a runtime field's script.
    pub chain: Vec<Box<str>>,
    /// Runtime fields this script read, bound to the segment.
    rt_leaves: Vec<(Box<str>, Box<super::runtime::EvalLeaf>)>,
    /// The vector field [`Host::with_vector`] reads, resolved once, and a buffer for vectors
    /// whose bytes are not aligned as floats.
    vector: Option<(Box<str>, VectorSlot)>,
    vector_buf: Vec<f32>,
}

/// How [`Host::with_vector`] reads its field in this segment.
enum VectorSlot {
    /// Not an indexed float vector field: read through [`Host::doc_values`].
    Other,
    /// No vectors in this segment.
    Absent,
    Reader(search_codec::VectorsReader),
}

/// A literal doc field of the script, as [`DocHost::doc_values_at`] resolved it.
enum Slot {
    Unresolved,
    /// A field read from sorted-numeric doc values (`None`: none in this segment).
    Numeric { kind: Kind, doc_kind: DocKind, dv: Option<Box<search_codec::SortedNumericDocValues>> },
    /// Anything else (unmapped, strings, vectors…): the by-name path.
    Other,
}

/// One slot's values over a prefetched run of docs.
struct Col {
    values: Vec<i64>,
    /// Docs in the run (0: the column is not filled).
    len: u32,
}

/// Runs shorter than this are read doc by doc.
const PREFETCH_MIN: usize = 16;

/// [`DocHost::prefetch`] is on (tests compare against the per-doc path).
static PREFETCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Turns the block prefetch of script doc values off or on (tests: results must not change).
#[doc(hidden)]
pub fn set_prefetch(on: bool) {
    PREFETCH.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Distinct fields remembered per host (scripts rarely read more).
const RESOLVED_MAX: usize = 8;

impl DocHost {
    pub fn new(table: Arc<FieldTable>, seg: SegmentContext, shard: i32, index: String) -> DocHost {
        let ctl = search_index::search::ambient_control();
        let runtime = super::runtime::RuntimeScope::of_control(ctl.as_ref());
        DocHost { table, seg, doc: 0, score: 0.0, shard, index, id_fielddata: crate::opensearch::active(), buf: Vec::new(), resolved: Vec::new(), slots: Vec::new(), cols: Vec::new(), block_start: 0, cancellable: search_scripts_poll(), ctl, term_stats: None, runtime, chain: Vec::new(), rt_leaves: Vec::new(), vector: None, vector_buf: Vec::new() }
    }

    /// `doc[field]` of a runtime field (`None`: `field` is no runtime field of the search on
    /// this index). Runtime fields shadow mapped fields of the same name.
    fn runtime_values(&mut self, field: &str, out: &mut DocValues) -> Option<Result<bool, HostError>> {
        let scope = self.runtime.as_ref()?;
        if scope.index_name() != self.index {
            return None;
        }
        let ev = scope.get(field)?;
        Some(self.read_runtime(field, ev, out))
    }

    fn read_runtime(&mut self, field: &str, ev: Arc<super::runtime::Evaluator>, out: &mut DocValues) -> Result<bool, HostError> {
        let i = match self.rt_leaves.iter().position(|(n, _)| &**n == field) {
            Some(i) => i,
            None => {
                // `SearchLookup.forkAndTrackFieldReferences`.
                let mut chain: Vec<Box<str>> = self.chain.clone();
                let ty = ev.field.ty;
                if chain.iter().any(|c| &**c == field) {
                    let mut names: Vec<&str> = chain.iter().map(|c| &**c).collect();
                    names.push(field);
                    let e = HostError::illegal_argument(format!("Cyclic dependency detected while resolving runtime fields: {}", names.join(" -> ")));
                    return Err(super::runtime::with_fork_frames(e, ty, 131));
                }
                chain.push(field.into());
                if chain.len() > super::runtime::MAX_FIELD_CHAIN_DEPTH {
                    let names: Vec<&str> = chain.iter().map(|c| &**c).collect();
                    let e = HostError::illegal_argument(format!("Field requires resolving too many dependent fields: {}", names.join(" -> ")));
                    return Err(super::runtime::with_fork_frames(e, ty, 134));
                }
                let leaf = ev.leaf_chained(&self.seg, chain).map_err(|e| HostError::illegal_argument(super::runtime::innermost_reason(&e)))?;
                self.rt_leaves.push((field.into(), Box::new(leaf)));
                self.rt_leaves.len() - 1
            }
        };
        let doc = self.doc;
        let vals = self.rt_leaves[i].1.values(doc).map_err(|e| HostError::illegal_argument(super::runtime::innermost_reason(&e)))?;
        super::runtime::fill_doc_values(ev.field.ty, &vals, out);
        Ok(true)
    }

    /// A host for scripts outside a search (the `_execute` API): loops count statements.
    /// Sets [`DocHost::id_fielddata`] (`super::id_fielddata::scripts` of the request's state).
    pub fn with_id_fielddata(mut self, on: bool) -> DocHost {
        self.id_fielddata = on;
        self
    }

    pub fn without_cancellation(mut self) -> DocHost {
        self.cancellable = false;
        self.ctl = None;
        self
    }

}

/// The mapped field `name`, through the resolved-field cache (borrowing only the cache and the
/// table, so the caller keeps using the host's other fields; no reference count changes).
#[inline]
fn resolve<'a>(resolved: &'a mut Vec<(Box<str>, Arc<crate::engine::fields::FieldInfo>)>, table: &'a FieldTable, name: &str) -> Option<&'a crate::engine::fields::FieldInfo> {
    if let Some(i) = resolved.iter().position(|(n, _)| &**n == name) {
        return Some(&resolved[i].1);
    }
    // `root.key` of a `flattened` field reads its keyed values (`KeyedFlattenedFieldType`).
    let f = match table.get(name) {
        Some(f) => f.clone(),
        None => table.keyed(name)?,
    };
    // Keyed fields aren't in the table, so they are kept even past the cache's size.
    if resolved.len() < RESOLVED_MAX || f.flattened_key.is_some() {
        resolved.push((name.into(), f));
        return resolved.last().map(|(_, f)| &**f);
    }
    table.get(name).map(|f| &**f)
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
        Kind::GeoShape => DocKind::GeoShape,
        Kind::Ip => DocKind::Ip,
        Kind::Binary => DocKind::Binary,
        Kind::DenseVector => DocKind::DenseVector,
        _ => return None,
    })
}

/// The kinds [`fill_sorted_numeric`] reads.
fn sorted_numeric_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::GeoPoint
            | Kind::Double
            | Kind::Float
            | Kind::HalfFloat
            | Kind::ScaledFloat(_)
            | Kind::UnsignedLong
            | Kind::Long
            | Kind::TokenCount
            | Kind::Integer
            | Kind::Short
            | Kind::Byte
            | Kind::Boolean
            | Kind::Date
            | Kind::DateNanos
    )
}

/// One value of a sorted-numeric field, as [`fill_sorted_numeric`] reads it.
#[inline]
fn push_numeric(kind: Kind, v: i64, out: &mut DocValues) {
    match kind {
        Kind::GeoPoint => out.geo.push([encode::decode_latitude((v >> 32) as i32), encode::decode_longitude(v as i32)]),
        Kind::Double => out.doubles.push(encode::sortable_f64(v)),
        Kind::Float => out.doubles.push(encode::sortable_f32(v as i32) as f64),
        Kind::HalfFloat => out.doubles.push(encode::sortable_f16(v as i16) as f64),
        Kind::ScaledFloat(sf) => out.doubles.push(super::hits::scaled_double(v, sf)),
        Kind::UnsignedLong => out.longs.push(((v as u64) ^ (1 << 63)) as i64),
        _ => out.longs.push(v),
    }
}

/// `MappedFieldType.failIfNoDocValues`, which a script's fielddata lookup reaches for a field
/// mapped with `doc_values: false` (`None` when the field has doc values or isn't such a type).
fn no_doc_values(f: &crate::engine::fields::FieldInfo) -> Option<HostError> {
    if f.doc_values {
        return None;
    }
    let server = |frame: &str| format!("{}org.elasticsearch.index.mapper.{frame}", search_painless::es::SERVER);
    let builder = match f.kind {
        Kind::Boolean => server("BooleanFieldMapper$BooleanFieldType.fielddataBuilder(BooleanFieldMapper.java:451)"),
        Kind::Keyword => server("KeywordFieldMapper$KeywordFieldType.fielddataBuilder(KeywordFieldMapper.java:1094)"),
        Kind::Long | Kind::Integer | Kind::Short | Kind::Byte | Kind::Double | Kind::Float | Kind::HalfFloat => {
            server("NumberFieldMapper$NumberFieldType.fielddataBuilder(NumberFieldMapper.java:2346)")
        }
        Kind::Date | Kind::DateNanos => server("DateFieldMapper$DateFieldType.fielddataBuilder(DateFieldMapper.java:1109)"),
        Kind::GeoPoint => server("GeoPointFieldMapper$GeoPointFieldType.fielddataBuilder(GeoPointFieldMapper.java:482)"),
        Kind::Ip => server("IpFieldMapper$IpFieldType.fielddataBuilder(IpFieldMapper.java:674)"),
        Kind::UnsignedLong => "org.elasticsearch.mapper.unsignedlong@9.5.5/org.elasticsearch.xpack.unsignedlong.UnsignedLongFieldMapper$UnsignedLongFieldType.fielddataBuilder(UnsignedLongFieldMapper.java:526)".to_string(),
        Kind::ScaledFloat(_) => "org.elasticsearch.mapper.extras@9.5.5/org.elasticsearch.index.mapper.extras.ScaledFloatFieldMapper$ScaledFloatFieldType.fielddataBuilder(ScaledFloatFieldMapper.java:498)".to_string(),
        _ => return None,
    };
    let mut e = HostError::illegal_argument(format!("Can't load fielddata on [{}] because fielddata is unsupported on fields of type [{}]. Use doc values instead.", f.name, f.ty));
    e.frames = vec![server("MappedFieldType.failIfNoDocValues(MappedFieldType.java:497)"), builder];
    e.frames.extend(
        [
            "index.fielddata.IndexFieldDataService.getForField(IndexFieldDataService.java:92)",
            "index.IndexService.loadFielddata(IndexService.java:1451)",
            "index.query.SearchExecutionContext.lambda$setLookupProviders$5(SearchExecutionContext.java:570)",
        ]
        .iter()
        .map(|f| format!("{}org.elasticsearch.{f}", search_painless::es::SERVER)),
    );
    Some(e)
}

impl DocHost {
    /// The field API's values of a field without doc values, from `_source`, sorted like doc
    /// values (numbers ascending, strings by their bytes; values that don't parse are skipped).
    fn source_values(&mut self, f: &crate::engine::fields::FieldInfo, out: &mut DocValues) -> Result<bool, HostError> {
        use crate::json::Value;
        use crate::mapping::params::NumKind;
        let Some(kind) = kind_of(f.kind) else { return Ok(true) };
        out.clear(kind);
        if self.seg.ensure_stored().is_err() {
            return Ok(true);
        }
        let Ok(stored) = self.seg.reader.stored().document(self.doc) else { return Ok(true) };
        let Some(Ok(Some(Value::Obj(src)))) = crate::fetch::source_bytes(&stored).map(crate::json::parse) else { return Ok(true) };
        let mut vals = Vec::new();
        if f.kind == Kind::GeoPoint {
            crate::engine::source::extract_values_keeping_coordinates(&src, &f.name, &mut vals);
        } else {
            crate::engine::source::extract_values(&src, &f.name, &mut vals);
        }
        let num = |k: NumKind, v: &Value| crate::mapping::params::parse_number(k, v, true).ok();
        let mut strs: Vec<String> = Vec::new();
        for v in vals.into_iter().filter(|v| !v.is_null()) {
            match f.kind {
                Kind::Boolean => match v {
                    Value::Bool(b) => out.longs.push(*b as i64),
                    Value::Str(s) if s == "true" => out.longs.push(1),
                    Value::Str(s) if s == "false" || s.is_empty() => out.longs.push(0),
                    _ => {}
                },
                Kind::Keyword => strs.push(v.to_es_string()),
                Kind::Long | Kind::Integer | Kind::Short | Kind::Byte => {
                    let k = match f.kind {
                        Kind::Long => NumKind::Long,
                        Kind::Integer => NumKind::Integer,
                        Kind::Short => NumKind::Short,
                        _ => NumKind::Byte,
                    };
                    if let Some(n) = num(k, v).and_then(|n| n.as_i64()) {
                        out.longs.push(n);
                    }
                }
                Kind::UnsignedLong => {
                    if let Some(Value::Num(n)) = num(NumKind::UnsignedLong, v)
                        && let Ok(u) = n.to_string().parse::<u64>()
                    {
                        out.longs.push(u as i64);
                    }
                }
                Kind::Double | Kind::Float | Kind::HalfFloat => {
                    let k = match f.kind {
                        Kind::Double => NumKind::Double,
                        Kind::Float => NumKind::Float,
                        _ => NumKind::HalfFloat,
                    };
                    if let Some(d) = num(k, v).and_then(|n| n.as_f64()) {
                        out.doubles.push(match f.kind {
                            Kind::Float => d as f32 as f64,
                            Kind::HalfFloat => encode::sortable_f16(encode::f16_sortable(d as f32)) as f64,
                            _ => d,
                        });
                    }
                }
                Kind::ScaledFloat(sf) => {
                    if let Some(d) = num(NumKind::Double, v).and_then(|n| n.as_f64()) {
                        out.doubles.push(super::hits::scaled_double((d * sf).round() as i64, sf));
                    }
                }
                Kind::Date | Kind::DateNanos => {
                    let text = v.to_es_string();
                    let parsed = match &f.date_format {
                        Some(fmt) => fmt.parse(&text, None, false).ok(),
                        None => crate::time::DateFormatter::new(crate::time::DateFormatter::DEFAULT).ok().and_then(|fmt| fmt.parse(&text, None, false).ok()),
                    };
                    if let Some(i) = parsed {
                        out.longs.push(if f.kind == Kind::DateNanos { i.nanos_since_epoch().unwrap_or(i64::MAX) } else { i.millis() });
                    }
                }
                Kind::GeoPoint => {
                    if let Ok(p) = crate::mapping::parse_geo_point(v)
                        && let (Some(lat), Some(lon)) = (p.get("lat").and_then(Value::as_f64), p.get("lon").and_then(Value::as_f64))
                    {
                        // Doc values precision, as the fallback stores encoded points.
                        let (y, x) = (encode::encode_latitude(lat), encode::encode_longitude(lon));
                        out.geo.push([encode::decode_latitude(y), encode::decode_longitude(x)]);
                    }
                }
                _ => {}
            }
        }
        out.longs.sort_unstable();
        out.doubles.sort_unstable_by(f64::total_cmp);
        out.geo.sort_unstable_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
        strs.sort_unstable();
        for s in &strs {
            out.push_str(s);
        }
        Ok(true)
    }

    /// Decodes ahead, for the consecutive docs `docs` (ascending), the values of the numeric
    /// fields this script already read that hold one value in every doc (`match_all` scoring
    /// over a dense field): [`Host::doc_values_at`] then reads them from the run instead of
    /// ranking and unpacking each doc's value on its own. Other runs and fields keep the
    /// per-doc path.
    pub fn prefetch(&mut self, docs: &[u32]) {
        let n = docs.len();
        let run = n >= PREFETCH_MIN && docs[n - 1] - docs[0] == (n - 1) as u32 && PREFETCH.load(std::sync::atomic::Ordering::Relaxed);
        if self.cols.len() < self.slots.len() {
            self.cols.resize_with(self.slots.len(), || None);
        }
        for (slot, s) in self.slots.iter().enumerate() {
            let col = &mut self.cols[slot];
            if let Some(c) = col.as_mut() {
                c.len = 0;
            }
            if !run {
                continue;
            }
            let Slot::Numeric { kind, dv: Some(dv), .. } = s else { continue };
            if !sorted_numeric_kind(*kind) {
                continue;
            }
            let (Some(vals), set) = (dv.singleton_values(), dv.docs()) else { continue };
            if !set.is_all() || docs[n - 1] >= set.max_doc() {
                continue;
            }
            let c = col.get_or_insert_with(|| Col { values: Vec::new(), len: 0 });
            c.values.clear();
            c.values.resize(n, 0);
            // Every doc has a value: a doc's rank is the doc itself.
            vals.get_range(docs[0] as u64, &mut c.values);
            c.len = n as u32;
        }
        self.block_start = docs.first().copied().unwrap_or(0);
    }
}

/// Fills `out` with the values of `d` for the kinds whose doc values are sorted numerics read
/// as numbers or points (Elasticsearch's numeric, date, boolean and geo-point doc values);
/// false for other kinds.
#[inline]
fn fill_sorted_numeric(kind: Kind, dv: &search_codec::SortedNumericDocValues, d: u32, out: &mut DocValues) -> bool {
    match kind {
        Kind::GeoPoint => out.geo.extend(dv.values(d).map(|v| [encode::decode_latitude((v >> 32) as i32), encode::decode_longitude(v as i32)])),
        Kind::Double => out.doubles.extend(dv.values(d).map(encode::sortable_f64)),
        Kind::Float => out.doubles.extend(dv.values(d).map(|v| encode::sortable_f32(v as i32) as f64)),
        Kind::HalfFloat => out.doubles.extend(dv.values(d).map(|v| encode::sortable_f16(v as i16) as f64)),
        Kind::ScaledFloat(sf) => out.doubles.extend(dv.values(d).map(|v| super::hits::scaled_double(v, sf))),
        Kind::UnsignedLong => out.longs.extend(dv.values(d).map(|v| ((v as u64) ^ (1 << 63)) as i64)),
        Kind::Long | Kind::TokenCount | Kind::Integer | Kind::Short | Kind::Byte | Kind::Boolean | Kind::Date | Kind::DateNanos => out.longs.extend(dv.values(d)),
        _ => return false,
    }
    true
}

/// Whether search-time scripts poll for cancellation instead of counting loop statements:
/// Elasticsearch 9.5 hands the scripts its search runs (`_setCancellationCheck`) its
/// `ContextIndexSearcher.checkCancelled`; OpenSearch keeps `max_loop_counter` everywhere.
pub fn search_scripts_poll() -> bool {
    !crate::opensearch::active()
}

/// `ContextIndexSearcher.checkCancelled` through a search control: the deadline or the
/// task's cancellation stops the script (the searcher reports the interruption).
fn check_control(ctl: Option<&search_index::search::SearchControl>) -> Result<(), search_painless::HostError> {
    let Some(ctl) = ctl else { return Ok(()) };
    match ctl.check() {
        Ok(()) => Ok(()),
        Err(e) => {
            let kind = e.kind();
            let kind: &'static str = if kind == "task_cancelled_exception" { "task_cancelled_exception" } else { "time_exceeded_exception" };
            let reason = e.to_string();
            search_index::search::note_interrupted(e);
            Err(search_painless::HostError { kind, reason, frames: Vec::new() })
        }
    }
}

/// The host of a search's scripts without documents that Elasticsearch still makes
/// cancellable (`scripted_metric` init / combine / reduce, `bucket_script`,
/// `bucket_selector`): no loop statement limit, polling the search's control instead.
pub struct CancelHost(Option<search_index::search::SearchControl>);

impl CancelHost {
    /// A host polling the search running on this thread.
    pub fn ambient() -> CancelHost {
        CancelHost(search_index::search::ambient_control())
    }

    /// Runs `inst` with this host as a search runs it (see [`search_scripts_poll`]).
    pub fn prepare(inst: &mut search_painless::Instance<'_>) -> CancelHost {
        inst.set_cancellable(search_scripts_poll());
        CancelHost::ambient()
    }
}

impl Host for CancelHost {
    fn check_cancelled(&mut self) -> Result<(), search_painless::HostError> {
        check_control(self.0.as_ref())
    }
}

impl Host for DocHost {
    fn score(&mut self) -> f64 {
        self.score
    }

    fn term_stats(&mut self) -> Option<search_painless::TermStats> {
        let doc = self.doc;
        self.term_stats.as_mut().map(|t| t.get(doc))
    }

    fn check_cancelled(&mut self) -> Result<(), search_painless::HostError> {
        check_control(self.ctl.as_ref())
    }

    fn doc_values_at(&mut self, slot: usize, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        if slot >= self.slots.len() {
            self.slots.resize_with(slot + 1, || Slot::Unresolved);
        }
        if matches!(self.slots[slot], Slot::Unresolved) {
            self.slots[slot] = match self.table.get(field) {
                Some(f) => match kind_of(f.kind) {
                    Some(doc_kind) if sorted_numeric_kind(f.kind) && f.doc_values => {
                        Slot::Numeric { kind: f.kind, doc_kind, dv: self.seg.reader.sorted_numeric(f.number).cloned().map(Box::new) }
                    }
                    _ => Slot::Other,
                },
                None => Slot::Other,
            };
        }
        if self.runtime.is_some()
            && let Some(r) = self.runtime_values(field, out)
        {
            return r;
        }
        match &self.slots[slot] {
            Slot::Numeric { kind, doc_kind, dv } => {
                out.clear(*doc_kind);
                if let Some(Some(c)) = self.cols.get(slot)
                    && let i = self.doc.wrapping_sub(self.block_start)
                    && i < c.len
                {
                    push_numeric(*kind, c.values[i as usize], out);
                } else if let Some(dv) = dv {
                    fill_sorted_numeric(*kind, dv, self.doc, out);
                }
                Ok(true)
            }
            _ => self.doc_values(field, out),
        }
    }

    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        if self.runtime.is_some()
            && let Some(r) = self.runtime_values(field, out)
        {
            return r;
        }
        let Some(f) = resolve(&mut self.resolved, &self.table, field) else {
            return match field {
                // Numeric doc values Lucene keeps for every document.
                "_seq_no" | "_version" => {
                    out.clear(DocKind::Long);
                    let n = if field == "_seq_no" { search_index::schema::SEQ_NO_FIELD } else { search_index::schema::VERSION_FIELD };
                    if let Some(v) = self.seg.reader.numeric(n).and_then(|dv| dv.get(self.doc)) {
                        out.longs.push(v);
                    }
                    Ok(true)
                }
                "_id" if self.id_fielddata => {
                    out.clear(DocKind::String);
                    if super::id_fielddata::value(&self.seg, self.doc, &mut self.buf) {
                        out.push_str(&String::from_utf8_lossy(&self.buf));
                    }
                    Ok(true)
                }
                "_id" => {
                    let mut e = HostError::illegal_argument(super::id_fielddata::DISABLED);
                    // `ProvidedIdFieldMapper.IdFieldType.fielddataBuilder` (OpenSearch's
                    // `IdFieldMapper`), reached through the search's fielddata loader.
                    if crate::opensearch::active() {
                        e.frames = [
                            "index.mapper.IdFieldMapper$IdFieldType.fielddataBuilder(IdFieldMapper.java:190)",
                            "index.fielddata.IndexFieldDataService.getForField(IndexFieldDataService.java:134)",
                            "index.query.QueryShardContext.lambda$lookup$0(QueryShardContext.java:530)",
                            "search.lookup.SearchLookup.lambda$new$0(SearchLookup.java:118)",
                            "search.lookup.LeafDocLookup.lambda$get$0(LeafDocLookup.java:93)",
                            "secure_sm.AccessController.doPrivileged(AccessController.java:76)",
                            "search.lookup.LeafDocLookup.get(LeafDocLookup.java:93)",
                            "search.lookup.LeafDocLookup.get(LeafDocLookup.java:55)",
                        ]
                        .iter()
                        .map(|f| format!("org.opensearch.{f}"))
                        .collect();
                    } else {
                        e.frames = [
                            "index.mapper.ProvidedIdFieldMapper$IdFieldType.fielddataBuilder(ProvidedIdFieldMapper.java:164)",
                            "index.fielddata.IndexFieldDataService.getForField(IndexFieldDataService.java:92)",
                            "index.IndexService.loadFielddata(IndexService.java:1451)",
                            "index.query.SearchExecutionContext.lambda$setLookupProviders$5(SearchExecutionContext.java:570)",
                        ]
                        .iter()
                        .map(|f| format!("{}org.elasticsearch.{f}", search_painless::es::SERVER))
                        .collect();
                    }
                    Err(e)
                }
                _ => Ok(false),
            };
        };
        let Some(kind) = kind_of(f.kind) else {
            return Err(HostError::illegal_argument(format!("Field [{}] of type [{}] doesn't support scripts", f.name, f.ty)));
        };
        if let Some(e) = no_doc_values(f) {
            return Err(e);
        }
        out.clear(kind);
        let r = &self.seg.reader;
        let d = self.doc;
        match f.kind {
            Kind::Text | Kind::MatchOnlyText => {
                let Some(fd) = f.subfield("fielddata") else {
                    // `doc[...]` loads with `FielddataOperation.SEARCH` (the field API reads
                    // `_source` instead: `field_values`).
                    return Err(HostError::illegal_argument(crate::search::aggs::fielddata_disabled(&f.name, &self.index)));
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
            Kind::Version => {
                if let Some(dv) = r.sorted_set(f.number) {
                    for o in dv.ords(d) {
                        self.buf.clear();
                        dv.lookup_ord(o, &mut self.buf);
                        out.push_str(&super::version_field::decode(&self.buf));
                    }
                }
            }
            Kind::Flattened if f.flattened_key.is_some() => {
                let key = f.flattened_key.as_deref().unwrap_or_default();
                if let Some(dv) = r.sorted_set(f.number) {
                    for o in dv.ords(d) {
                        self.buf.clear();
                        dv.lookup_ord(o, &mut self.buf);
                        if let Some(v) = self.buf.strip_prefix(key.as_bytes()).and_then(|rest| rest.strip_prefix(b"\0")) {
                            out.push_str(&String::from_utf8_lossy(v));
                        }
                    }
                }
            }
            Kind::Keyword | Kind::Wildcard | Kind::Flattened | Kind::CountedKeyword => {
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
            Kind::GeoPoint | Kind::Double | Kind::Float | Kind::HalfFloat | Kind::ScaledFloat(_) | Kind::UnsignedLong => {
                if let Some(dv) = r.sorted_numeric(f.number) {
                    fill_sorted_numeric(f.kind, dv, d, out);
                }
            }
            // `GeoShapeValues.GeoShapeValue`: centroid, bounding box corners and label
            // position (`[lat, lon]`), dimensional type in `longs`.
            Kind::GeoShape => {
                if let Some(b) = r.binary(f.number).and_then(|dv| dv.get(d)) {
                    let c = f.subfield("centroid").and_then(|n| r.binary(n)).and_then(|dv| dv.get(d));
                    if let Ok(Some(v)) = search_aggs::shape_field::ShapeValue::from_doc_value(b, c, search_aggs::shape_field::Coords::Geo) {
                        let (cx, cy) = v.centroid_xy();
                        let bb = v.bounding_box();
                        let (lx, ly) = v.label_position();
                        out.geo.push([cy, cx]);
                        out.geo.push([bb.top, bb.min_x()]);
                        out.geo.push([bb.bottom, bb.max_x()]);
                        out.geo.push([ly, lx]);
                        out.longs.push(v.centroid.shape_type as i64);
                    }
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
                // Decoded straight from the segment's bytes (no copy per doc: `script_score`
                // over vectors reads one per doc).
                let fill = |raw: &[u8], out: &mut DocValues| match out.element {
                    search_painless::VectorElement::Float | search_painless::VectorElement::BFloat16 => {
                        out.vector.extend(raw.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)));
                    }
                    _ => out.vector_bytes.extend(raw.iter().map(|b| *b as i8)),
                };
                if f.index {
                    if let Some(v) = r.vectors(f.number)
                        && let Some(o) = v.ord(d)
                    {
                        fill(v.vector(o), out);
                    }
                } else {
                    out.vector_binary = true;
                    if let Some(x) = r.binary(f.number).and_then(|b| b.get(d)) {
                        fill(x, out);
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

    /// An indexed float vector lent in place (or decoded into a reused buffer when its bytes
    /// are not aligned): `script_score` over vectors costs no field lookup and no copy per
    /// doc. Anything else (runtime fields, byte and doc-values-only vectors) goes through
    /// [`Host::doc_values`].
    fn with_vector(&mut self, field: &str, f: &mut dyn FnMut(&search_painless::VectorView<'_>)) -> Result<Option<bool>, HostError> {
        if self.runtime.is_some() {
            return Ok(None);
        }
        if self.vector.as_ref().is_none_or(|(n, _)| &**n != field) {
            let slot = match resolve(&mut self.resolved, &self.table, field) {
                Some(fi) if fi.kind == Kind::DenseVector && fi.index && fi.vector.is_some_and(|s| s.element == search_codec::VectorElement::Float32) => {
                    match self.seg.reader.vectors(fi.number) {
                        Some(v) => VectorSlot::Reader(v.clone()),
                        None => VectorSlot::Absent,
                    }
                }
                _ => VectorSlot::Other,
            };
            self.vector = Some((field.into(), slot));
        }
        let v = match &self.vector {
            Some((_, VectorSlot::Reader(v))) => v,
            Some((_, VectorSlot::Absent)) => return Ok(Some(false)),
            _ => return Ok(None),
        };
        let Some(o) = v.ord(self.doc) else { return Ok(Some(false)) };
        let floats = match v.vector_f32_slice(o) {
            Some(x) => x,
            None => {
                v.vector_f32(o, &mut self.vector_buf);
                &self.vector_buf
            }
        };
        f(&search_painless::VectorView { element: search_painless::VectorElement::Float, floats, bytes: &[], binary: false, magnitude: None });
        Ok(Some(true))
    }

    fn has_field(&mut self, field: &str) -> bool {
        self.table.get(field).is_some() || self.runtime.as_ref().is_some_and(|r| r.get(field).is_some())
    }

    /// `FielddataOperation.SCRIPT`: a `text` field without fielddata reads its `_source`
    /// values (`SourceValueFetcherSortedBinaryIndexFieldData`: as strings, sorted).
    fn field_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        let text = match resolve(&mut self.resolved, &self.table, field) {
            // `TextFieldMapper.fielddataBuilder` with `FielddataOperation.SCRIPT` reads `_source`
            // whether or not fielddata is enabled.
            Some(f) if matches!(f.kind, Kind::Text | Kind::MatchOnlyText) => f.name.clone(),
            // Fields without doc values read `_source` (`SourceValueFetcher*IndexFieldData`),
            // except `ip`, whose field type has no source fallback.
            Some(f) if f.kind != Kind::Ip && no_doc_values(f).is_some() => {
                let f = f.clone();
                return self.source_values(&f, out);
            }
            _ => return self.doc_values(field, out),
        };
        out.clear(DocKind::String);
        if self.seg.ensure_stored().is_err() {
            return Ok(true);
        }
        let Ok(stored) = self.seg.reader.stored().document(self.doc) else { return Ok(true) };
        let Some(Ok(Some(crate::json::Value::Obj(src)))) = crate::fetch::source_bytes(&stored).map(crate::json::parse) else { return Ok(true) };
        let mut vals = Vec::new();
        crate::engine::source::extract_values(&src, &text, &mut vals);
        let mut strs: Vec<String> = vals.into_iter().filter(|v| !v.is_null()).map(crate::json::Value::to_es_string).collect();
        strs.sort_unstable();
        for s in strs {
            out.push_str(&s);
        }
        Ok(true)
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
    inst: Inst,
    host: DocHost,
    _script: search_painless::Compiled,
}

/// A segment's evaluator: Painless, or a Lucene expression (`lang: expression`, which
/// Elasticsearch accepts in the filter, score, aggs, number-sort and terms-set contexts).
// One per segment and used on every document: not boxed, the size difference is the point.
#[allow(clippy::large_enum_variant)]
enum Inst {
    Painless(search_painless::Instance<'static>),
    Expression(search_painless::expression::Instance<'static>),
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
        let cancellable = host.cancellable;
        let mut leaf = Leaf { inst: Inst::Painless(inst), host, _script: search_painless::Compiled::Painless(s) };
        if let Inst::Painless(i) = &mut leaf.inst {
            i.set_params(params);
            i.set_cancellable(cancellable);
        }
        leaf
    }

    /// A leaf for a Painless script or a Lucene expression (`None` for a Mustache template).
    pub fn from_bound(bound: &Bound, host: DocHost) -> Option<Leaf> {
        match &bound.compiled {
            search_painless::Compiled::Painless(s) => Some(Leaf::new(s, &bound.params, host)),
            search_painless::Compiled::Expression(e) => {
                let e = e.clone();
                // SAFETY: `_script` keeps the expression alive for as long as the instance exists.
                let mut inst: search_painless::expression::Instance<'static> = unsafe { std::mem::transmute(e.instance()) };
                inst.set_params(&bound.params);
                Some(Leaf { inst: Inst::Expression(inst), host, _script: search_painless::Compiled::Expression(e) })
            }
            search_painless::Compiled::Mustache(_) => None,
        }
    }

    pub fn run(&mut self, doc: u32, score: f64) -> Result<search_painless::Value, search_painless::ScriptError> {
        self.host.doc = doc;
        self.host.score = score;
        match &mut self.inst {
            Inst::Painless(i) => i.execute(&mut self.host),
            Inst::Expression(i) => i.execute(&mut self.host),
        }
    }

    pub fn run_f64(&mut self, doc: u32, score: f64) -> Result<f64, search_painless::ScriptError> {
        self.host.doc = doc;
        self.host.score = score;
        match &mut self.inst {
            Inst::Painless(i) => i.execute_f64(&mut self.host),
            Inst::Expression(i) => i.execute_f64(&mut self.host),
        }
    }

    pub fn run_bool(&mut self, doc: u32) -> Result<bool, search_painless::ScriptError> {
        self.host.doc = doc;
        match &mut self.inst {
            Inst::Painless(i) => i.execute_bool(&mut self.host),
            Inst::Expression(i) => i.execute_bool(&mut self.host),
        }
    }

    pub fn set_var(&mut self, name: &str, v: search_painless::Value) {
        match &mut self.inst {
            Inst::Painless(i) => i.set_var(name, v),
            Inst::Expression(i) => i.set_var(name, v),
        }
    }

    pub fn explanation(&self) -> Option<String> {
        match &self.inst {
            Inst::Painless(i) => i.explanation().map(str::to_string),
            Inst::Expression(_) => None,
        }
    }

    /// Runs a runtime-field script for `doc`; its values are in [`Leaf::emitted`] /
    /// [`Leaf::emitted_fields`].
    pub fn run_emit(&mut self, doc: u32) -> Result<(), search_painless::ScriptError> {
        self.run(doc, 0.0).map(|_| ())
    }

    pub fn emitted(&self) -> &[search_painless::Value] {
        match &self.inst {
            Inst::Painless(i) => i.emitted(),
            Inst::Expression(_) => &[],
        }
    }

    pub fn emitted_fields(&self) -> &[(String, Vec<search_painless::Value>)] {
        match &self.inst {
            Inst::Painless(i) => i.emitted_fields(),
            Inst::Expression(_) => &[],
        }
    }

    pub fn set_field_name(&mut self, name: &str) {
        if let Inst::Painless(i) = &mut self.inst {
            i.set_field_name(name);
        }
    }
}

/// An error raised inside an engine hook, kept whole for the response: script errors keep
/// their `script_stack` and causes through [`park_script_error`].
pub(crate) fn park_error(e: crate::error::EsError) -> search_index::Error {
    if e.kind == "script_exception" {
        let reason = format!("{}{}", e.reason(), park_script_error(e.clone()));
        return search_index::Error::Es { kind: std::borrow::Cow::Borrowed("script_exception"), reason, status: e.status };
    }
    search_index::Error::Es { kind: e.kind.clone(), reason: e.reason().to_string(), status: e.status }
}

pub(crate) fn script_err(e: search_painless::ScriptError) -> search_index::Error {
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

pub(crate) fn park_script_error(e: crate::error::EsError) -> String {
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
    /// [`DocHost::id_fielddata`] for the script's documents.
    pub id_fielddata: bool,
    /// The script counts loop statements (`max_loop_counter`) even in a search: the call
    /// site gives it no cancellation check (`function_score` script functions).
    pub counted: bool,
}

impl Painless {
    /// A document host for `seg`.
    pub fn host(&self, seg: &SegmentContext) -> DocHost {
        let mut h = DocHost::new(self.table.clone(), seg.clone(), self.shard, self.index.clone());
        h.id_fielddata = self.id_fielddata;
        if self.counted { h.without_cancellation() } else { h }
    }

    /// A score script leaf over a prepared host.
    pub fn score_leaf(&self, host: DocHost) -> search_index::Result<Box<dyn LeafScoreScript>> {
        let leaf = Leaf::from_bound(&self.bound, host).ok_or_else(|| search_index::Error::illegal("not a painless script"))?;
        Ok(Box::new(ScoreLeaf(leaf)))
    }

    fn leaf_of(&self, seg: &SegmentContext) -> search_index::Result<Leaf> {
        Leaf::from_bound(&self.bound, self.host(seg))
            .ok_or_else(|| search_index::Error::illegal("not a painless script"))
    }
}

/// A filter script on one segment. Deleted documents never match and never run the script:
/// Lucene checks live docs before `TwoPhaseIterator.matches`, so a script that reads a field
/// every live document has can't fail on a deleted one that lacks it.
struct FilterLeaf(Leaf, SegmentContext);

impl LeafFilterScript for FilterLeaf {
    fn matches(&mut self, doc: u32) -> search_index::Result<bool> {
        if !self.1.is_live(doc) {
            return Ok(false);
        }
        self.0.run_bool(doc).map_err(script_err)
    }
}

impl FilterScript for Painless {
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafFilterScript>> {
        Ok(Box::new(FilterLeaf(self.leaf_of(seg)?, seg.clone())))
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
    fn score_block(&mut self, docs: &[u32], subs: &[f32], out: &mut [f64]) -> std::result::Result<(), (usize, search_index::Error)> {
        self.0.host.prefetch(docs);
        for (i, (&d, &sub)) in docs.iter().zip(subs).enumerate() {
            out[i] = self.0.run_f64(d, sub as f64).map_err(|e| (i, script_err(e)))?;
        }
        Ok(())
    }
    fn explanation(&mut self) -> Option<String> {
        self.0.explanation()
    }
}

impl ScoreScript for Painless {
    fn needs_score(&self) -> bool {
        match &self.bound.compiled {
            search_painless::Compiled::Painless(s) => s.needs_score(),
            search_painless::Compiled::Expression(e) => e.needs_score(),
            search_painless::Compiled::Mustache(_) => false,
        }
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
