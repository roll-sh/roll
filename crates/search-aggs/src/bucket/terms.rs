//! `terms` (and the result type of `multi_terms`): buckets per distinct value, the top `size`
//! by `order`, with Elasticsearch's distributed counting semantics.
//!
//! Shard side, values map to bucket ordinals by one of three strategies (Elasticsearch's
//! execution modes):
//! - **global ordinals** for keyword-like fields without a script: per segment, segment
//!   ordinals map to the searcher's global ordinals (`search_index`'s `GlobalOrdinals`), and a
//!   top-level terms aggregation without sub-aggregations counts straight into an array indexed
//!   by global ordinal — no hashing, one array increment per value;
//! - **map** for scripts, `missing` and `execution_hint: map`: a byte-string hash;
//! - **numeric** for longs, dates, booleans and doubles: a hash of the long (or the double's
//!   sortable long).
//!
//! Each owning bucket keeps its top `shard_size` buckets (`size * 1.5 + 10` by default unless
//! ordered by key), honouring `shard_min_doc_count`, and reports the doc counts it dropped as
//! `sum_other_doc_count`. Shard results are sorted by key so the reduce merges them in one
//! pass. The reduce computes `doc_count_error_upper_bound` (and the per-bucket error with
//! `show_term_doc_count_error`) exactly as `AbstractInternalTerms` does, keeps buckets with at
//! least `min_doc_count` docs and the top `size` of them by the requested order (compound, with
//! `_key asc` as tie-breaker), and counts them against `search.max_buckets`.

use std::cmp::Ordering;
use std::sync::Arc;

use search_index::search::ordinals::GlobalOrdinals;
use search_index::{Searcher, SegmentContext};

use crate::bucket::include::{IncludeExclude, LongFilter, StringFilter, bit};
use crate::collect::{Aggregator, Batch, BindCtx, Subs, grow};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, bucket_property, multi_bucket_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, BucketOrder, PathElement, java_double_compare};
use crate::util::{BytesOrdMap, OrdMap};
use crate::values::{BoundVs, BytesReader, DocCounter, DvOrds, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

/// `SubAggCollectionMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectMode {
    DepthFirst,
    BreadthFirst,
}

impl CollectMode {
    pub fn parse(s: &str) -> Result<CollectMode> {
        match s {
            "depth_first" => Ok(CollectMode::DepthFirst),
            "breadth_first" => Ok(CollectMode::BreadthFirst),
            _ => Err(AggError::new(400, "parse_exception", format!("no [collect_mode] found for value [{s}]"))),
        }
    }
}

/// A parsed `terms`.
#[derive(Clone, Debug)]
pub struct TermsSpec {
    pub vs: VsSpec,
    pub size: usize,
    /// `None` = not set (`-1`).
    pub shard_size: Option<usize>,
    pub min_doc_count: u64,
    pub shard_min_doc_count: u64,
    pub order: BucketOrder,
    pub include_exclude: Option<IncludeExclude>,
    pub execution_hint: Option<String>,
    pub collect_mode: Option<CollectMode>,
    pub show_term_doc_count_error: bool,
}

/// `TermsAggregationBuilder.PARSER`.
pub fn parse(p: &mut XParser<'_>, name: &str) -> Result<TermsSpec> {
    let opts = VsOpts::new(true, true, false);
    let mut fields = VsSpec::fields(opts);
    fields.extend_from_slice(&[
        Field::new("show_term_doc_count_error", Tokens::BOOLEAN),
        Field::new("shard_size", Tokens::INT),
        Field::new("min_doc_count", Tokens::LONG),
        Field::new("shard_min_doc_count", Tokens::LONG),
        Field::new("size", Tokens::INT),
        Field::new("execution_hint", Tokens::STRING),
        Field::new("collect_mode", Tokens::STRING),
        Field::new("order", Tokens::OBJECT_ARRAY),
        Field::new("include", Tokens::OBJECT_ARRAY_OR_STRING),
        Field::new("exclude", Tokens::STRING_ARRAY),
    ]);
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut spec = TermsSpec {
        vs: VsSpec::default(),
        size: 10,
        shard_size: None,
        min_doc_count: 1,
        shard_min_doc_count: 0,
        order: BucketOrder::compound(vec![BucketOrder::COUNT_DESC]),
        include_exclude: None,
        execution_hint: None,
        collect_mode: None,
        show_term_doc_count_error: false,
    };
    p.object("terms", &fields, |p, i| {
        let f = names[i];
        if spec.vs.parse_field(p, f)? {
            return Ok(());
        }
        match f {
            "show_term_doc_count_error" => spec.show_term_doc_count_error = p.boolean_value()?,
            "shard_size" => {
                let v = p.int_value()?;
                if v <= 0 {
                    return Err(AggError::illegal_argument(format!("[shardSize] must be greater than 0. Found [{v}] in [{name}]")));
                }
                spec.shard_size = Some(v as usize);
            }
            "min_doc_count" => {
                let v = p.long_value()?;
                if v < 0 {
                    return Err(AggError::illegal_argument(format!("[minDocCount] must be greater than or equal to 0. Found [{v}] in [{name}]")));
                }
                spec.min_doc_count = v as u64;
            }
            "shard_min_doc_count" => {
                let v = p.long_value()?;
                if v < 0 {
                    return Err(AggError::illegal_argument(format!(
                        "[shardMinDocCount] must be greater than or equal to 0. Found [{v}] in [{name}]"
                    )));
                }
                spec.shard_min_doc_count = v as u64;
            }
            "size" => {
                let v = p.int_value()?;
                if v <= 0 {
                    return Err(AggError::illegal_argument(format!("[size] must be greater than 0. Found [{v}] in [{name}]")));
                }
                spec.size = v as usize;
            }
            "execution_hint" => spec.execution_hint = Some(p.text()?),
            "collect_mode" => spec.collect_mode = Some(CollectMode::parse(&p.text()?)?),
            "order" => {
                let list = BucketOrder::parse_list(p)?;
                spec.order = order_of(list);
            }
            "include" => {
                let inc = IncludeExclude::parse_include(p)?;
                spec.include_exclude = IncludeExclude::merge(Some(inc), spec.include_exclude.take())?;
            }
            _ => {
                let exc = IncludeExclude::parse_exclude(p)?;
                spec.include_exclude = IncludeExclude::merge(spec.include_exclude.take(), Some(exc))?;
            }
        }
        Ok(())
    })?;
    spec.vs.check_required(opts)?;
    Ok(spec)
}

/// `order(List)`: `BucketOrder.compound(list)`.
pub fn order_of(list: Vec<BucketOrder>) -> BucketOrder {
    BucketOrder::compound(list)
}

/// `TermsAggregatorFactory.adjustBucketCountThresholds` + `ensureValidity`: (size, shard_size,
/// shard_min_doc_count).
pub fn adjust_thresholds(size: usize, shard_size: Option<usize>, min_doc_count: u64, shard_min_doc_count: u64, order: &BucketOrder) -> (usize, usize, u64) {
    let mut shard = match shard_size {
        None if !order.is_key_order() => suggest_shard_size(size),
        None => 0,
        Some(s) => s,
    };
    if shard < size {
        shard = size;
    }
    let smdc = shard_min_doc_count.min(min_doc_count);
    (size, shard, smdc)
}

/// `BucketUtils.suggestShardSideQueueSize`.
pub fn suggest_shard_size(size: usize) -> usize {
    ((size as f64 * 1.5 + 10.0) as u64).min(i32::MAX as u64) as usize
}

/// Validates an order against the aggregator's sub-aggregations (`order.validate(this)`).
pub fn validate_order(order: &BucketOrder, subs: &Subs) -> Result<()> {
    for o in order.elements() {
        if let BucketOrder::Agg { path, .. } = o
            && let Err(e) = subs.validate_sort(&path.elements)
        {
            return Err(AggError::invalid_path(format!("Invalid aggregation order path [{path}]. {}", e.reason)));
        }
    }
    Ok(())
}

pub fn create(def: &AggDef, spec: &TermsSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::Keyword, false)?;
    if vs.is_counter() {
        return Err(vs.unsupported("terms"));
    }
    let subs = Subs::bind(def, ctx)?;
    validate_order(&spec.order, &subs)?;
    let (size, shard_size, shard_min_doc_count) = adjust_thresholds(spec.size, spec.shard_size, spec.min_doc_count, spec.shard_min_doc_count, &spec.order);
    if let Some(h) = &spec.execution_hint
        && h != "map"
        && h != "global_ordinals"
    {
        return Err(AggError::illegal_argument(format!("Unknown `execution_hint`: [{h}], expected any of [map, global_ordinals]")));
    }
    let cfg = Arc::new(TermsConfig {
        order: spec.order.clone(),
        size,
        shard_size,
        min_doc_count: spec.min_doc_count,
        shard_min_doc_count,
        show_term_doc_count_error: spec.show_term_doc_count_error,
        format: vs.format.clone(),
    });
    if vs.unmapped && vs.missing.is_none() {
        return Ok(Box::new(UnmappedTermsAgg { cfg, subs }));
    }
    let many = !ctx.cardinality.is_one();
    let kind = match vs.vs_type {
        VsType::Keyword | VsType::Ip => {
            if let Some(ie) = &spec.include_exclude
                && ie.is_regex_based()
                && !vs.format.is_raw()
            {
                return Err(AggError::illegal_argument(format!(
                    "Aggregation [{}] cannot support regular expression style include/exclude settings as they can only be applied to string fields. Use an array of values for include/exclude clauses",
                    def.name
                )));
            }
            let filter = match &spec.include_exclude {
                Some(ie) => Some(ie.string_filter(&vs.format)?),
                None => None,
            };
            let ords = vs.has_ordinals() && vs.missing.is_none() && spec.execution_hint.as_deref() != Some("map");
            let gords = if ords { vs.field.as_ref().and_then(|f| ctx.searcher.global_ordinals(f.number)) } else { None };
            if ords {
                Mode::Ords(OrdsState { gords, field: vs.field.as_ref().map(|f| f.number).unwrap_or(0), dv: DvOrds::None, seg: 0, accepted: None, filter, batch: Default::default(), assign: if many { Assign::Hash(OrdMap::new()) } else if subs.is_empty() && spec.min_doc_count > 0 { Assign::Direct } else { Assign::Dense(Vec::new(), Vec::new()) } })
            } else {
                Mode::Bytes(BytesState { reader: None, filter, ords: BytesOrdMap::new() })
            }
        }
        VsType::Numeric | VsType::Date | VsType::Boolean => {
            if let Some(ie) = &spec.include_exclude
                && ie.is_regex_based()
            {
                return Err(AggError::illegal_argument(format!(
                    "Aggregation [{}] cannot support regular expression style include/exclude settings as they can only be applied to string fields. Use an array of numeric values for include/exclude clauses used to filter numeric fields",
                    def.name
                )));
            }
            let double = !vs.is_integral();
            let filter = match &spec.include_exclude {
                Some(ie) => Some(if double { ie.double_filter()? } else { ie.long_filter(&vs.format)? }),
                None => None,
            };
            Mode::Num(NumState { reader: None, batch: Default::default(), filter, ords: OrdMap::new(), double })
        }
        _ => return Err(vs.unsupported("terms")),
    };
    Ok(Box::new(TermsAgg {
        cfg,
        vs,
        mode: kind,
        counts: Vec::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        searcher: ctx.searcher.clone(),
        tracker: ctx.tracker(def),
    }))
}

/// Settings every terms result carries.
#[derive(Debug)]
pub struct TermsConfig {
    pub order: BucketOrder,
    pub size: usize,
    pub shard_size: usize,
    pub min_doc_count: u64,
    pub shard_min_doc_count: u64,
    pub show_term_doc_count_error: bool,
    pub format: DocValueFormat,
}

enum Assign {
    /// Bucket ordinal = global ordinal (single owning bucket, no sub-aggregations).
    Direct,
    /// Global ordinal → bucket ordinal (single owning bucket), and its inverse.
    Dense(Vec<u32>, Vec<u64>),
    /// `(owning, global ordinal)` → bucket ordinal.
    Hash(OrdMap),
}

struct OrdsState {
    gords: Option<Arc<GlobalOrdinals>>,
    field: u32,
    dv: DvOrds,
    seg: u32,
    /// Accepted segment ordinals (include/exclude) of the current segment.
    accepted: Option<Vec<u64>>,
    filter: Option<StringFilter>,
    assign: Assign,
    /// Batch-decoded ordinals.
    batch: crate::values::OrdBatch,
}

struct BytesState {
    reader: Option<BytesReader>,
    filter: Option<StringFilter>,
    ords: BytesOrdMap,
}

struct NumState {
    reader: Option<NumReader>,
    /// Batch-decoded doc values (fields without script or `missing`).
    batch: crate::values::NumBatch,
    filter: Option<LongFilter>,
    /// Keys are longs, or sortable longs of doubles.
    ords: OrdMap,
    double: bool,
}

/// The collection strategy (one per aggregator: variant sizes don't matter).
#[allow(clippy::large_enum_variant)]
enum Mode {
    Ords(OrdsState),
    Bytes(BytesState),
    Num(NumState),
}

struct TermsAgg {
    cfg: Arc<TermsConfig>,
    vs: BoundVs,
    mode: Mode,
    counts: Vec<u64>,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    searcher: Searcher,
    tracker: MemTracker,
}

/// A candidate bucket at build time.
#[derive(Clone, Copy)]
struct Cand {
    ord: u32,
    count: u64,
    /// Key rank: global ordinal, or the long key (sortable for doubles).
    key: u64,
    /// The sort values of `Agg` order elements (cached).
    sv: [f64; 2],
}

impl TermsAgg {
    #[inline]
    fn count(&mut self, ord: u32, doc: u32) -> Result<()> {
        if self.counts.len() <= ord as usize {
            grow(&mut self.counts, ord, 0);
            self.tracker.ensure(self.counts.len() * 8)?;
        }
        self.counts[ord as usize] += if self.doc_counter.is_constant() { 1 } else { self.doc_counter.count(doc) };
        Ok(())
    }

    fn key_cmp(&self, a: &Cand, b: &Cand) -> Ordering {
        match &self.mode {
            Mode::Ords(_) => a.key.cmp(&b.key),
            Mode::Bytes(s) => s.ords.get_bytes(a.ord).cmp(s.ords.get_bytes(b.ord)),
            Mode::Num(s) => (a.key as i64).cmp(&(b.key as i64)).then_with(|| {
                let _ = s;
                Ordering::Equal
            }),
        }
    }

    /// `order.partiallyBuiltBucketComparator(this)`.
    fn cmp(&self, a: &Cand, b: &Cand) -> Ordering {
        let mut agg_i = 0;
        for o in self.cfg.order.elements() {
            let c = match o {
                BucketOrder::Count { asc } => {
                    if *asc {
                        a.count.cmp(&b.count)
                    } else {
                        b.count.cmp(&a.count)
                    }
                }
                BucketOrder::Key { asc } => {
                    let c = self.key_cmp(a, b);
                    if *asc { c } else { c.reverse() }
                }
                BucketOrder::Agg { path, asc } => {
                    let (x, y) = if agg_i < 2 {
                        (a.sv[agg_i], b.sv[agg_i])
                    } else {
                        (self.subs.sort_value(&path.elements, a.ord), self.subs.sort_value(&path.elements, b.ord))
                    };
                    agg_i += 1;
                    if self.subs.sort_is_count(&path.elements) {
                        let c = (x as u64).cmp(&(y as u64));
                        if *asc { c } else { c.reverse() }
                    } else {
                        compare_discard_nan(x, y, *asc)
                    }
                }
                BucketOrder::Compound(_) => Ordering::Equal,
            };
            if c != Ordering::Equal {
                return c;
            }
        }
        Ordering::Equal
    }

    fn cand(&self, ord: u32, key: u64) -> Cand {
        let mut sv = [f64::NAN; 2];
        let mut i = 0;
        for o in self.cfg.order.elements() {
            if let BucketOrder::Agg { path, .. } = o {
                if i < 2 {
                    sv[i] = self.subs.sort_value(&path.elements, ord);
                }
                i += 1;
            }
        }
        Cand { ord, count: self.counts.get(ord as usize).copied().unwrap_or(0), key, sv }
    }

    /// Adds zero-count buckets for every accepted term (`min_doc_count: 0`).
    fn collect_zero_doc_entries(&mut self, owning: u32) -> Result<()> {
        if self.cfg.min_doc_count != 0 {
            return Ok(());
        }
        let segs: Vec<SegmentContext> = self.searcher.segments().to_vec();
        match &mut self.mode {
            Mode::Ords(st) => {
                let Some(g) = st.gords.clone() else { return Ok(()) };
                // Accepted global ordinals: the union of each segment's accepted ordinals.
                for seg in &segs {
                    let dv = DvOrds::open(seg, st.field);
                    let Some(dict) = dv.dict() else { continue };
                    let accepted = st.filter.as_ref().map(|f| f.accepted_ords(dict));
                    for o in 0..dict.len() {
                        if accepted.as_ref().is_some_and(|a| !bit(a, o)) {
                            continue;
                        }
                        let gord = g.global(seg.ord, o);
                        let ord = match &mut st.assign {
                            Assign::Direct => gord as u32,
                            Assign::Dense(map, inv) => dense_ord(map, inv, gord),
                            Assign::Hash(m) => m.add(owning, gord).0,
                        };
                        grow(&mut self.counts, ord, 0);
                    }
                }
            }
            Mode::Bytes(st) => {
                if self.cfg.order.is_count_desc() && count_owning(&st.ords, owning) >= self.cfg.size {
                    return Ok(());
                }
                for seg in &segs {
                    let mut r = self.vs.bytes_reader(seg)?;
                    for doc in 0..seg.max_doc() {
                        let n = r.load(doc, f32::NAN)?;
                        for i in 0..n {
                            let v = r.value(i);
                            if st.filter.as_ref().is_some_and(|f| !f.accept(v)) {
                                continue;
                            }
                            let (o, _) = st.ords.add(owning, v);
                            grow(&mut self.counts, o, 0);
                        }
                    }
                }
            }
            Mode::Num(st) => {
                if self.cfg.order.is_count_desc() && st.ords.keys().iter().filter(|k| k.0 == owning).count() >= self.cfg.size {
                    return Ok(());
                }
                for seg in &segs {
                    let mut r = self.vs.numeric_reader(seg)?;
                    for doc in 0..seg.max_doc() {
                        if st.double {
                            let vals = r.doubles(doc, f32::NAN)?;
                            for &v in vals {
                                let k = search_codec::points::encode::f64_sortable(v);
                                if st.filter.as_ref().is_some_and(|f| !f.accept(k)) {
                                    continue;
                                }
                                let (o, _) = st.ords.add(owning, k as u64);
                                grow(&mut self.counts, o, 0);
                            }
                        } else {
                            let vals = r.longs(doc, f32::NAN)?;
                            for &v in vals {
                                if st.filter.as_ref().is_some_and(|f| !f.accept(v)) {
                                    continue;
                                }
                                let (o, _) = st.ords.add(owning, v as u64);
                                grow(&mut self.counts, o, 0);
                            }
                        }
                    }
                }
            }
        }
        self.tracker.ensure(self.counts.len() * 8)?;
        Ok(())
    }

    /// The bucket candidates of one owning ordinal: (ordinal, key rank).
    fn candidates(&self, owning: u32, by_owning: &Option<hashbrown::HashMap<u32, Vec<u32>>>) -> Vec<(u32, u64)> {
        match &self.mode {
            Mode::Ords(st) => match &st.assign {
                Assign::Direct => {
                    if owning != 0 {
                        return Vec::new();
                    }
                    (0..self.counts.len() as u32).filter(|&o| self.has_bucket(o)).map(|o| (o, o as u64)).collect()
                }
                Assign::Dense(_, inv) => {
                    if owning != 0 {
                        return Vec::new();
                    }
                    inv.iter().enumerate().map(|(o, g)| (o as u32, *g)).collect()
                }
                Assign::Hash(m) => by_owning.as_ref().and_then(|b| b.get(&owning)).map(|v| v.iter().map(|&o| (o, m.key(o).1)).collect()).unwrap_or_default(),
            },
            Mode::Bytes(_) => by_owning.as_ref().and_then(|b| b.get(&owning)).map(|v| v.iter().map(|&o| (o, 0)).collect()).unwrap_or_default(),
            Mode::Num(st) => by_owning.as_ref().and_then(|b| b.get(&owning)).map(|v| v.iter().map(|&o| (o, st.ords.key(o).1)).collect()).unwrap_or_default(),
        }
    }

    /// Direct mode (never used with `min_doc_count: 0`): whether global ordinal `o` was collected.
    fn has_bucket(&self, o: u32) -> bool {
        self.counts[o as usize] > 0
    }

    fn key_of(&self, c: &Cand, buf: &mut Vec<u8>) -> TermKey {
        match &self.mode {
            Mode::Ords(st) => {
                if let Some(g) = &st.gords {
                    g.lookup(&self.searcher, st.field, c.key, buf);
                }
                TermKey::Bytes(buf.clone())
            }
            Mode::Bytes(st) => TermKey::Bytes(st.ords.get_bytes(c.ord).to_vec()),
            Mode::Num(st) => {
                if st.double {
                    TermKey::Double(search_codec::points::encode::sortable_f64(c.key as i64))
                } else {
                    TermKey::Long(c.key as i64)
                }
            }
        }
    }

    fn kind(&self) -> TermsKind {
        match &self.mode {
            Mode::Ords(_) | Mode::Bytes(_) => TermsKind::Str,
            Mode::Num(st) => {
                if st.double {
                    TermsKind::Double
                } else {
                    TermsKind::Long
                }
            }
        }
    }
}

fn count_owning(m: &BytesOrdMap, owning: u32) -> usize {
    (0..m.len() as u32).filter(|&o| m.owning(o) == owning).count()
}

#[inline]
fn dense_ord(map: &mut Vec<u32>, inv: &mut Vec<u64>, gord: u64) -> u32 {
    let g = gord as usize;
    if map.len() <= g {
        map.resize((g + 1).max(map.len() * 2).max(64), u32::MAX);
    }
    let o = map[g];
    if o != u32::MAX {
        return o;
    }
    let o = inv.len() as u32;
    inv.push(gord);
    map[g] = o;
    o
}

/// `Comparators.compareDiscardNaN`: NaN last whatever the direction.
#[inline]
pub fn compare_discard_nan(a: f64, b: f64, asc: bool) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => {
            if asc {
                java_double_compare(a, b)
            } else {
                java_double_compare(b, a)
            }
        }
    }
}

impl Aggregator for TermsAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        match &mut self.mode {
            Mode::Ords(st) => {
                st.dv = DvOrds::open(seg, st.field);
                st.seg = seg.ord;
                st.accepted = match (&st.filter, st.dv.dict()) {
                    (Some(f), Some(d)) => Some(f.accepted_ords(d)),
                    _ => None,
                };
            }
            Mode::Bytes(st) => st.reader = Some(self.vs.bytes_reader(seg)?),
            Mode::Num(st) => st.reader = Some(self.vs.numeric_reader(seg)?),
        }
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let mut mode = std::mem::replace(&mut self.mode, Mode::Num(NumState { reader: None, batch: Default::default(), filter: None, ords: OrdMap::new(), double: false }));
        let r = self.collect_mode(&mut mode, b);
        self.mode = mode;
        r
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        for &o in owning {
            self.collect_zero_doc_entries(o)?;
        }
        let by_owning: Option<hashbrown::HashMap<u32, Vec<u32>>> = match &self.mode {
            Mode::Ords(OrdsState { assign: Assign::Hash(m), .. }) | Mode::Num(NumState { ords: m, .. }) => {
                let mut h: hashbrown::HashMap<u32, Vec<u32>> = hashbrown::HashMap::new();
                for (o, k) in m.keys().iter().enumerate() {
                    h.entry(k.0).or_default().push(o as u32);
                }
                Some(h)
            }
            Mode::Bytes(st) => {
                let mut h: hashbrown::HashMap<u32, Vec<u32>> = hashbrown::HashMap::new();
                for o in 0..st.ords.len() as u32 {
                    h.entry(st.ords.owning(o)).or_default().push(o);
                }
                Some(h)
            }
            _ => None,
        };
        let shard_size = self.cfg.shard_size;
        let mut selected: Vec<(Vec<Cand>, i64)> = Vec::with_capacity(owning.len());
        for &own in owning {
            let cands = self.candidates(own, &by_owning);
            let mut other: i64 = 0;
            let mut keep: Vec<Cand> = Vec::with_capacity(cands.len().min(shard_size));
            for (ord, key) in cands {
                let c = self.cand(ord, key);
                other += c.count as i64;
                if c.count < self.cfg.shard_min_doc_count {
                    continue;
                }
                keep.push(c);
            }
            if keep.len() > shard_size {
                keep.select_nth_unstable_by(shard_size - 1, |a, b| self.cmp(a, b));
                keep.truncate(shard_size);
            }
            keep.sort_by(|a, b| self.cmp(a, b));
            for c in &keep {
                other -= c.count as i64;
            }
            selected.push((keep, other));
        }
        let all: Vec<u32> = selected.iter().flat_map(|(k, _)| k.iter().map(|c| c.ord)).collect();
        let mut sub_aggs = self.subs.build(&all)?.into_iter();
        let key_order = self.cfg.order.is_key_order();
        let kind = self.kind();
        let mut out = Vec::with_capacity(owning.len());
        let mut buf = Vec::new();
        for (keep, other) in selected {
            let mut buckets = Vec::with_capacity(keep.len());
            for c in &keep {
                let key = self.key_of(c, &mut buf);
                buckets.push(TermsBucket { key, doc_count: c.count, doc_count_error: 0, aggs: sub_aggs.next().unwrap_or_default() });
            }
            let reduce_order = if key_order {
                self.cfg.order.clone()
            } else {
                buckets.sort_by(|a, b| a.key.cmp_key(&b.key));
                BucketOrder::KEY_ASC
            };
            out.push(AggValue::Terms(Box::new(Terms {
                kind,
                buckets,
                reduce_order,
                cfg: self.cfg.clone(),
                doc_count_error: None,
                other_doc_count: other,
                formats: Vec::new(),
            })));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        AggValue::Terms(Box::new(Terms {
            kind: self.kind(),
            buckets: Vec::new(),
            reduce_order: if self.cfg.order.is_key_order() { self.cfg.order.clone() } else { BucketOrder::KEY_ASC },
            cfg: self.cfg.clone(),
            doc_count_error: None,
            other_doc_count: 0,
            formats: Vec::new(),
        }))
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores() || self.vs.script.as_ref().is_some_and(|s| s.needs_score())
    }
}

impl TermsAgg {
    fn collect_mode(&mut self, mode: &mut Mode, b: Batch<'_>) -> Result<()> {
        let has_subs = !self.subs.is_empty();
        match mode {
            Mode::Ords(st) => {
                let gmap: Option<Arc<GlobalOrdinals>> = st.gords.clone();
                let seg_map = gmap.as_ref().and_then(|g| g.segment_map(st.seg));
                st.dv.load_batch(b.docs, &mut st.batch);
                for (p, ords) in st.batch.docs() {
                    let doc = b.docs[p];
                    let owning = b.ords[p];
                    let i = p;
                    for &o in ords {
                        if let Some(acc) = &st.accepted
                            && !bit(acc, o)
                        {
                            continue;
                        }
                        let gord = match seg_map {
                            Some(m) => m[o as usize] as u64,
                            None => o,
                        };
                        let ord = match &mut st.assign {
                            Assign::Direct => gord as u32,
                            Assign::Dense(map, inv) => dense_ord(map, inv, gord),
                            Assign::Hash(m) => m.add(owning, gord).0,
                        };
                        self.count(ord, doc)?;
                        if has_subs {
                            self.subs.push(doc, ord, b.score(i))?;
                        }
                    }
                }
            }
            Mode::Bytes(st) => {
                let Some(r) = st.reader.as_mut() else { return Ok(()) };
                for i in 0..b.len() {
                    let doc = b.docs[i];
                    let n = r.load(doc, b.score(i))?;
                    for j in 0..n {
                        if j > 0 && r.value(j) == r.value(j - 1) {
                            continue;
                        }
                        let v = r.value(j);
                        if st.filter.as_ref().is_some_and(|f| !f.accept(v)) {
                            continue;
                        }
                        let (ord, _) = st.ords.add(b.ords[i], v);
                        self.count(ord, doc)?;
                        if has_subs {
                            self.subs.push(doc, ord, b.score(i))?;
                        }
                    }
                }
                self.tracker.ensure(self.counts.len() * 8 + st.ords.ram_bytes())?;
            }
            Mode::Num(st) => {
                let Some(r) = st.reader.as_mut() else { return Ok(()) };
                if let Some((dv, decode)) = r.raw() {
                    dv.load_batch(b.docs, &mut st.batch);
                    for (p, raw) in st.batch.docs() {
                        let doc = b.docs[p];
                        let mut prev: Option<i64> = None;
                        for &rv in raw {
                            // Keys: longs, or the sortable longs of doubles.
                            let k = if st.double { search_codec::points::encode::f64_sortable(decode.to_f64(rv)) } else { decode.to_i64(rv) };
                            if prev == Some(k) {
                                continue;
                            }
                            prev = Some(k);
                            if st.filter.as_ref().is_some_and(|f| !f.accept(k)) {
                                continue;
                            }
                            let (ord, _) = st.ords.add(b.ords[p], k as u64);
                            if self.counts.len() <= ord as usize {
                                grow(&mut self.counts, ord, 0);
                            }
                            self.counts[ord as usize] += self.doc_counter.count(doc);
                            if has_subs {
                                self.subs.push(doc, ord, b.score(p))?;
                            }
                        }
                    }
                    self.tracker.ensure(self.counts.len() * 8 + st.ords.ram_bytes())?;
                    return Ok(());
                }
                for i in 0..b.len() {
                    let doc = b.docs[i];
                    if st.double {
                        let vals = r.doubles(doc, b.score(i))?;
                        let mut prev: Option<f64> = None;
                        for &v in vals {
                            if prev.is_some_and(|p| p.to_bits() == v.to_bits()) {
                                continue;
                            }
                            prev = Some(v);
                            let k = search_codec::points::encode::f64_sortable(v);
                            if st.filter.as_ref().is_some_and(|f| !f.accept(k)) {
                                continue;
                            }
                            let (ord, _) = st.ords.add(b.ords[i], k as u64);
                            if self.counts.len() <= ord as usize {
                                grow(&mut self.counts, ord, 0);
                            }
                            self.counts[ord as usize] += self.doc_counter.count(doc);
                            if has_subs {
                                self.subs.push(doc, ord, b.score(i))?;
                            }
                        }
                    } else {
                        let vals = r.longs(doc, b.score(i))?;
                        let mut prev: Option<i64> = None;
                        for &v in vals {
                            if prev == Some(v) {
                                continue;
                            }
                            prev = Some(v);
                            if st.filter.as_ref().is_some_and(|f| !f.accept(v)) {
                                continue;
                            }
                            let (ord, _) = st.ords.add(b.ords[i], v as u64);
                            if self.counts.len() <= ord as usize {
                                grow(&mut self.counts, ord, 0);
                            }
                            self.counts[ord as usize] += self.doc_counter.count(doc);
                            if has_subs {
                                self.subs.push(doc, ord, b.score(i))?;
                            }
                        }
                    }
                }
                self.tracker.ensure(self.counts.len() * 8 + st.ords.ram_bytes())?;
            }
        }
        Ok(())
    }
}

/// Terms over an unmapped field (`UnmappedTerms`).
struct UnmappedTermsAgg {
    cfg: Arc<TermsConfig>,
    subs: Subs,
}

impl Aggregator for UnmappedTermsAgg {
    fn set_segment(&mut self, _seg: &SegmentContext) -> Result<()> {
        Ok(())
    }
    fn collect(&mut self, _b: Batch<'_>) -> Result<()> {
        Ok(())
    }
    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        Ok(owning.iter().map(|_| self.build_empty()).collect())
    }
    fn build_empty(&self) -> AggValue {
        let _ = &self.subs;
        AggValue::Terms(Box::new(Terms {
            kind: TermsKind::Unmapped,
            buckets: Vec::new(),
            reduce_order: BucketOrder::KEY_ASC,
            cfg: self.cfg.clone(),
            doc_count_error: Some(0),
            other_doc_count: 0,
            formats: Vec::new(),
        }))
    }
}

/// Which terms result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermsKind {
    Str,
    Long,
    Double,
    Unmapped,
    /// `multi_terms`.
    Multi,
}

/// A terms bucket key.
#[derive(Clone, Debug, PartialEq)]
pub enum TermKey {
    Bytes(Vec<u8>),
    Long(i64),
    Double(f64),
    Multi(Vec<TermKey>),
}

impl TermKey {
    /// Key order (`_key asc`).
    pub fn cmp_key(&self, o: &TermKey) -> Ordering {
        match (self, o) {
            (TermKey::Bytes(a), TermKey::Bytes(b)) => a.cmp(b),
            (TermKey::Long(a), TermKey::Long(b)) => a.cmp(b),
            (TermKey::Double(a), TermKey::Double(b)) => java_double_compare(*a, *b),
            (TermKey::Long(a), TermKey::Double(b)) => java_double_compare(*a as f64, *b),
            (TermKey::Double(a), TermKey::Long(b)) => java_double_compare(*a, *b as f64),
            (TermKey::Multi(a), TermKey::Multi(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    let c = x.cmp_key(y);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                a.len().cmp(&b.len())
            }
            _ => Ordering::Equal,
        }
    }

    /// A hashable identity (bytes) for grouping buckets across shards.
    pub fn identity(&self, out: &mut Vec<u8>) {
        match self {
            TermKey::Bytes(b) => {
                out.push(0);
                out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                out.extend_from_slice(b);
            }
            TermKey::Long(l) => {
                out.push(1);
                out.extend_from_slice(&l.to_le_bytes());
            }
            TermKey::Double(d) => {
                out.push(2);
                let d = if *d == 0.0 { 0.0 } else { *d };
                out.extend_from_slice(&d.to_bits().to_le_bytes());
            }
            TermKey::Multi(v) => {
                out.push(3);
                for k in v {
                    k.identity(out);
                }
            }
        }
    }
}

/// One terms bucket.
#[derive(Clone, Debug)]
pub struct TermsBucket {
    pub key: TermKey,
    pub doc_count: u64,
    pub doc_count_error: i64,
    pub aggs: Aggs,
}

/// `StringTerms` / `LongTerms` / `DoubleTerms` / `UnmappedTerms` / `InternalMultiTerms`.
#[derive(Clone, Debug)]
pub struct Terms {
    pub kind: TermsKind,
    pub buckets: Vec<TermsBucket>,
    pub reduce_order: BucketOrder,
    pub cfg: Arc<TermsConfig>,
    /// `None` on shard results (computed at reduce).
    pub doc_count_error: Option<i64>,
    pub other_doc_count: i64,
    /// Per-term formats of `multi_terms`.
    pub formats: Vec<DocValueFormat>,
}

/// `MultiTerms` share the terms result type.
pub type MultiTerms = Terms;

impl Terms {
    fn key_json(&self, k: &TermKey, format: &DocValueFormat) -> Json {
        match k {
            TermKey::Bytes(b) => format.format_bytes(b),
            TermKey::Long(l) => match format {
                DocValueFormat::UnsignedLongShifted => format.format_long(*l),
                _ => Json::Int(*l),
            },
            TermKey::Double(d) => Json::Float(*d),
            TermKey::Multi(v) => Json::Arr(
                v.iter()
                    .enumerate()
                    .map(|(i, k)| {
                        let f = self.formats.get(i).unwrap_or(&DocValueFormat::Raw);
                        match k {
                            TermKey::Bytes(b) => f.format_bytes(b),
                            TermKey::Long(l) => f.format_long(*l),
                            TermKey::Double(d) => f.format_double(*d),
                            other => self.key_json(other, f),
                        }
                    })
                    .collect(),
            ),
        }
    }

    /// `getKeyAsString()`.
    pub fn key_string(&self, k: &TermKey) -> String {
        let f = &self.cfg.format;
        match k {
            TermKey::Bytes(b) => match f.format_bytes(b) {
                Json::Str(s) => s,
                other => other.to_java_string(),
            },
            TermKey::Long(l) => f.long_string(*l),
            TermKey::Double(d) => f.double_string(*d),
            TermKey::Multi(v) => v
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    let f = self.formats.get(i).unwrap_or(&DocValueFormat::Raw);
                    match k {
                        TermKey::Bytes(b) => match f.format_bytes(b) {
                            Json::Str(s) => s,
                            o => o.to_java_string(),
                        },
                        TermKey::Long(l) => f.long_string(*l),
                        TermKey::Double(d) => f.double_string(*d),
                        _ => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join("|"),
        }
    }

    /// `getDocCountError(terms)` of one result being reduced.
    fn shard_doc_count_error(&self) -> i64 {
        let size = self.buckets.len();
        if size == 0 || size < self.cfg.shard_size || self.cfg.order.is_key_order() {
            0
        } else if self.cfg.order.is_count_desc() {
            match self.doc_count_error {
                Some(e) => e,
                None => self.buckets.iter().map(|b| b.doc_count as i64).min().unwrap_or(0),
            }
        } else {
            -1
        }
    }

    fn render_bucket(&self, b: &TermsBucket, rc: &RenderCtx) -> Json {
        let mut o = Json::obj();
        let f = &self.cfg.format;
        match (&b.key, self.kind) {
            (TermKey::Bytes(_), _) => o.push("key", Json::Str(self.key_string(&b.key))),
            (TermKey::Long(l), _) => {
                o.push("key", self.key_json(&b.key, f));
                if !f.is_raw() && !matches!(f, DocValueFormat::UnsignedLongShifted) {
                    o.push("key_as_string", Json::Str(f.long_string(*l)));
                }
            }
            (TermKey::Double(d), _) => {
                o.push("key", Json::Float(*d));
                if !f.is_raw() {
                    o.push("key_as_string", Json::Str(f.double_string(*d)));
                }
            }
            (TermKey::Multi(_), _) => {
                o.push("key", self.key_json(&b.key, f));
                o.push("key_as_string", Json::Str(self.key_string(&b.key)));
            }
        }
        o.push("doc_count", Json::from(b.doc_count));
        if self.cfg.show_term_doc_count_error {
            o.push("doc_count_error_upper_bound", Json::Int(b.doc_count_error));
        }
        b.aggs.render_into(&mut o, rc);
        o
    }

    fn promote_to_double(&mut self) {
        if self.kind != TermsKind::Long {
            return;
        }
        let fmt = self.cfg.format.clone();
        for b in &mut self.buckets {
            if let TermKey::Long(l) = b.key {
                b.key = TermKey::Double(match fmt {
                    DocValueFormat::UnsignedLongShifted => ((l as u64) ^ (1 << 63)) as f64,
                    _ => l as f64,
                });
            }
        }
        self.kind = TermsKind::Double;
    }
}

/// Reduce-side value of an order path for a bucket (`AggregationPath.resolveValue`).
#[derive(Clone, Copy, Debug)]
pub enum SortVal {
    Long(i64),
    Double(f64),
}

/// Resolves a sort path against a bucket's aggregations (`InternalAggregations.sortValue`).
pub fn resolve_sort_value(aggs: &Aggs, path: &[PathElement]) -> Result<SortVal> {
    let head = &path[0];
    let agg = aggs.get(&head.name).ok_or_else(|| AggError::illegal_argument(format!("Cannot find aggregation named [{}]", head.name)))?;
    let rest = &path[1..];
    if !rest.is_empty() {
        return match agg.value.single_aggs() {
            Some(sub) => resolve_sort_value(sub, rest),
            None => Err(AggError::illegal_argument(format!("Can't sort by a descendant of a [{}] aggregation [{}]", agg.value.type_name(), rest[0].full))),
        };
    }
    let key = head.key.as_deref().or(head.metric.as_deref());
    agg_sort_value(&agg.name, &agg.value, key)
}

/// `InternalAggregation.sortValue(key)`.
pub fn agg_sort_value(name: &str, v: &AggValue, key: Option<&str>) -> Result<SortVal> {
    if let AggValue::Single(s) = v {
        if let Some(k) = key
            && k != "doc_count"
        {
            return Err(AggError::illegal_argument(format!(
                "Unknown value key [{k}] for single-bucket aggregation [{name}]. Either use [doc_count] as key or drop the key all together."
            )));
        }
        return Ok(SortVal::Long(s.doc_count as i64));
    }
    if let Some(x) = v.single_value() {
        if let Some(k) = key
            && k != "value"
        {
            return Err(AggError::illegal_argument(format!(
                "Unknown value key [{k}] for single-value metric aggregation [{name}]. Either use [value] as key or drop the key all together."
            )));
        }
        return Ok(SortVal::Double(x));
    }
    if v.is_multi_value_metric() {
        let Some(k) = key else {
            return Err(AggError::illegal_argument("Missing value key in [null] which refers to a multi-value metric aggregation"));
        };
        return v.multi_value(k).map(SortVal::Double);
    }
    Err(AggError::illegal_argument(format!("Can't sort a [{}] aggregation [{name}]", v.type_name())))
}

/// Compares two reduce-side sort values (`SortValue.compareTo`, negated for `desc`).
pub fn cmp_sort_vals(a: SortVal, b: SortVal, asc: bool) -> Ordering {
    let c = match (a, b) {
        (SortVal::Long(x), SortVal::Long(y)) => x.cmp(&y),
        (SortVal::Double(x), SortVal::Double(y)) => java_double_compare(x, y),
        (SortVal::Long(x), SortVal::Double(y)) => java_double_compare(x as f64, y),
        (SortVal::Double(x), SortVal::Long(y)) => java_double_compare(x, y as f64),
    };
    if asc { c } else { c.reverse() }
}

/// The final-reduce comparator of buckets (`order.comparator()`), with cached sort values.
pub struct ReduceSorter<'a> {
    pub order: &'a BucketOrder,
    /// Per bucket, the values of each `Agg` element (in order).
    pub agg_vals: Vec<Vec<SortVal>>,
}

impl<'a> ReduceSorter<'a> {
    /// Precomputes sort values; fails like `resolveValue` on bad paths.
    pub fn new(order: &'a BucketOrder, aggs: &[&Aggs]) -> Result<ReduceSorter<'a>> {
        let mut agg_vals = Vec::with_capacity(aggs.len());
        let paths: Vec<&crate::request::AggPath> = order
            .elements()
            .iter()
            .filter_map(|o| if let BucketOrder::Agg { path, .. } = o { Some(path) } else { None })
            .collect();
        for a in aggs {
            let mut v = Vec::with_capacity(paths.len());
            for p in &paths {
                v.push(resolve_sort_value(a, &p.elements).map_err(|e| AggError::illegal_argument(format!("Invalid aggregation order path [{p}]. {}", e.reason)))?);
            }
            agg_vals.push(v);
        }
        Ok(ReduceSorter { order, agg_vals })
    }

    pub fn cmp(&self, i: usize, j: usize, count: impl Fn(usize) -> u64, key_cmp: impl Fn(usize, usize) -> Ordering) -> Ordering {
        let mut ai = 0;
        for o in self.order.elements() {
            let c = match o {
                BucketOrder::Count { asc } => {
                    let c = count(i).cmp(&count(j));
                    if *asc { c } else { c.reverse() }
                }
                BucketOrder::Key { asc } => {
                    let c = key_cmp(i, j);
                    if *asc { c } else { c.reverse() }
                }
                BucketOrder::Agg { asc, .. } => {
                    let c = cmp_sort_vals(self.agg_vals[i][ai], self.agg_vals[j][ai], *asc);
                    ai += 1;
                    c
                }
                BucketOrder::Compound(_) => Ordering::Equal,
            };
            if c != Ordering::Equal {
                return c;
            }
        }
        Ordering::Equal
    }
}

/// `InternalMultiBucketAggregation.countInnerBucket` of a bucket's aggregations.
pub fn count_inner_buckets(aggs: &Aggs) -> usize {
    let mut n = 0;
    for a in &aggs.list {
        if let Some(m) = a.value.multi() {
            n += m.len();
            for i in 0..m.len() {
                n += count_inner_buckets(m.aggs(i));
            }
        } else if let Some(s) = a.value.single_aggs() {
            n += count_inner_buckets(s);
        }
    }
    n
}

impl Internal for Terms {
    fn type_name(&self) -> &'static str {
        match self.kind {
            TermsKind::Str | TermsKind::Unmapped => "sterms",
            TermsKind::Long => "lterms",
            TermsKind::Double => "dterms",
            TermsKind::Multi => "multi_terms",
        }
    }

    fn class_name(&self) -> &'static str {
        match self.kind {
            TermsKind::Str => "StringTerms",
            TermsKind::Unmapped => "UnmappedTerms",
            TermsKind::Long => "LongTerms",
            TermsKind::Double => "DoubleTerms",
            TermsKind::Multi => "InternalMultiTerms",
        }
    }

    fn must_reduce_single(&self) -> bool {
        true
    }

    fn can_lead(&self) -> bool {
        self.kind != TermsKind::Unmapped
    }

    fn reduce(mut list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        // Unmapped results take part in the count but not in the merge (`canLeadReduction`).
        let n_aggs = list.len();
        if list.iter().all(|t| t.kind == TermsKind::Unmapped) {
            return Ok(list.swap_remove(0));
        }
        list.retain(|t| t.kind != TermsKind::Unmapped);
        // Long terms meeting double terms are promoted (different index mappings).
        let has_double = list.iter().any(|t| t.kind == TermsKind::Double);
        let has_long = list.iter().any(|t| t.kind == TermsKind::Long);
        let has_str = list.iter().any(|t| matches!(t.kind, TermsKind::Str));
        if has_str && (has_double || has_long) {
            return Err(crate::internal::reduce_type_mismatch(""));
        }
        if has_double && has_long {
            for t in &mut list {
                t.promote_to_double();
            }
        }
        let first = &list[0];
        let cfg = first.cfg.clone();
        let kind = first.kind;
        let formats = first.formats.clone();
        let show_error = cfg.show_term_doc_count_error;
        let mut other_doc_count: i64 = 0;
        let mut sum_error: i64 = 0;
        let mut this_reduce_order: Option<BucketOrder> = None;
        // Bucket lists per result, remembering the reduce-order direction.
        let mut lists: Vec<Vec<TermsBucket>> = Vec::with_capacity(list.len());
        for mut t in list {
            match &this_reduce_order {
                None => this_reduce_order = Some(t.reduce_order.clone()),
                Some(o) => {
                    if *o != cfg.order && *o != t.reduce_order {
                        this_reduce_order = Some(cfg.order.clone());
                    }
                }
            }
            other_doc_count += t.other_doc_count;
            let err = t.shard_doc_count_error();
            if sum_error != -1 {
                if err == -1 {
                    sum_error = -1;
                } else {
                    sum_error += err;
                }
            }
            if show_error {
                for b in &mut t.buckets {
                    b.doc_count_error -= err;
                }
            }
            if !t.buckets.is_empty() {
                lists.push(std::mem::take(&mut t.buckets));
            }
        }
        let reduce_order = this_reduce_order.unwrap_or_else(|| cfg.order.clone());
        let key_asc = !reduce_order.is_key_order() || reduce_order.is_key_asc();
        let grouped: Vec<Vec<TermsBucket>> = if reduce_order.is_key_order() {
            // reduceMergeSort: a priority queue over the key-sorted lists.
            crate::util::lucene_merge_groups(lists, |a, b| {
                let c = a.key.cmp_key(&b.key);
                (if key_asc { c } else { c.reverse() }) == Ordering::Less
            })
        } else {
            // reduceLegacy: grouped by key in result order, then ordered by key.
            let mut groups: hashbrown::HashMap<Vec<u8>, usize> = hashbrown::HashMap::new();
            let mut grouped: Vec<Vec<TermsBucket>> = Vec::new();
            let mut idbuf = Vec::new();
            for l in lists {
                for b in l {
                    idbuf.clear();
                    b.key.identity(&mut idbuf);
                    match groups.get(&idbuf) {
                        Some(&g) => grouped[g].push(b),
                        None => {
                            groups.insert(idbuf.clone(), grouped.len());
                            grouped.push(vec![b]);
                        }
                    }
                }
            }
            grouped.sort_by(|a, b| a[0].key.cmp_key(&b[0].key));
            grouped
        };
        let reduce_group = |g: Vec<TermsBucket>, ctx: &mut ReduceCtx<'_>| -> Result<TermsBucket> {
            let mut doc_count = 0u64;
            let mut err: i64 = 0;
            let key = g[0].key.clone();
            let mut aggs = Vec::with_capacity(g.len());
            for b in g {
                doc_count += b.doc_count;
                if err != -1 {
                    if !show_error || b.doc_count_error == -1 {
                        err = -1;
                    } else {
                        err += b.doc_count_error;
                    }
                }
                aggs.push(b.aggs);
            }
            Ok(TermsBucket { key, doc_count, doc_count_error: err, aggs: Aggs::reduce_lists(aggs, ctx)? })
        };
        let mut result: Vec<TermsBucket>;
        let final_order: BucketOrder;
        if cfg.order.is_key_order() && cfg.min_doc_count <= 1 {
            result = Vec::new();
            for g in grouped {
                if result.len() < cfg.size {
                    ctx.consume_buckets(1)?;
                    result.push(reduce_group(g, ctx)?);
                } else {
                    other_doc_count += g.iter().map(|b| b.doc_count as i64).sum::<i64>();
                }
            }
            final_order = if ctx.is_final() { cfg.order.clone() } else { BucketOrder::Key { asc: key_asc } };
        } else if ctx.is_final() {
            let candidates: Vec<Vec<TermsBucket>> =
                grouped.into_iter().filter(|g| g.iter().map(|b| b.doc_count).sum::<u64>() >= cfg.min_doc_count).collect();
            let needs_aggs = cfg.order.has_agg_order();
            if needs_aggs {
                // Comparisons need reduced sub-aggregations: reduce every candidate without
                // counting, then count the kept ones (Elasticsearch releases the others).
                let unlimited = crate::internal::ReduceOptions { max_buckets: usize::MAX, ..ctx.opts.clone() };
                let mut scratch = ReduceCtx::new(&unlimited);
                let mut reduced = Vec::with_capacity(candidates.len());
                for g in candidates {
                    reduced.push(reduce_group(g, &mut scratch)?);
                }
                let aggs_refs: Vec<&Aggs> = reduced.iter().map(|b| &b.aggs).collect();
                let sorter = ReduceSorter::new(&cfg.order, &aggs_refs)?;
                let mut idx: Vec<usize> = (0..reduced.len()).collect();
                idx.sort_by(|&i, &j| sorter.cmp(i, j, |k| reduced[k].doc_count, |a, b| reduced[a].key.cmp_key(&reduced[b].key)));
                let keep: hashbrown::HashSet<usize> = idx.iter().take(cfg.size).copied().collect();
                let mut slots: Vec<Option<TermsBucket>> = reduced.into_iter().map(Some).collect();
                result = Vec::with_capacity(keep.len());
                for (rank, &i) in idx.iter().enumerate() {
                    let b = slots[i].take().expect("bucket");
                    if rank < cfg.size {
                        ctx.consume_buckets(1 + count_inner_buckets(&b.aggs))?;
                        result.push(b);
                    } else {
                        other_doc_count += b.doc_count as i64;
                    }
                }
            } else {
                let counts: Vec<u64> = candidates.iter().map(|g| g.iter().map(|b| b.doc_count).sum()).collect();
                let mut idx: Vec<usize> = (0..candidates.len()).collect();
                let order = &cfg.order;
                let cmp = |i: &usize, j: &usize| -> Ordering {
                    for o in order.elements() {
                        let c = match o {
                            BucketOrder::Count { asc } => {
                                let c = counts[*i].cmp(&counts[*j]);
                                if *asc { c } else { c.reverse() }
                            }
                            BucketOrder::Key { asc } => {
                                let c = candidates[*i][0].key.cmp_key(&candidates[*j][0].key);
                                if *asc { c } else { c.reverse() }
                            }
                            _ => Ordering::Equal,
                        };
                        if c != Ordering::Equal {
                            return c;
                        }
                    }
                    Ordering::Equal
                };
                if idx.len() > cfg.size {
                    idx.select_nth_unstable_by(cfg.size - 1, cmp);
                    for &i in &idx[cfg.size..] {
                        other_doc_count += counts[i] as i64;
                    }
                    idx.truncate(cfg.size);
                }
                idx.sort_by(cmp);
                ctx.consume_buckets(idx.len())?;
                let mut slots: Vec<Option<Vec<TermsBucket>>> = candidates.into_iter().map(Some).collect();
                result = Vec::with_capacity(idx.len());
                for i in idx {
                    result.push(reduce_group(slots[i].take().expect("group"), ctx)?);
                }
            }
            final_order = cfg.order.clone();
        } else {
            result = Vec::with_capacity(grouped.len());
            for g in grouped {
                result.push(reduce_group(g, ctx)?);
            }
            final_order = BucketOrder::Key { asc: key_asc };
        }
        if show_error {
            for r in &mut result {
                if sum_error == -1 {
                    r.doc_count_error = -1;
                } else {
                    r.doc_count_error += sum_error;
                }
            }
        }
        let doc_count_error = if sum_error != -1 {
            if n_aggs == 1 && !ctx.opts.has_batched_result { 0 } else { sum_error }
        } else {
            -1
        };
        Ok(Terms {
            kind,
            buckets: result,
            reduce_order: if ctx.is_final() { cfg.order.clone() } else { final_order },
            cfg,
            doc_count_error: Some(doc_count_error),
            other_doc_count,
            formats,
        })
    }

    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        match self.kind {
            TermsKind::Unmapped => {
                out.push("doc_count_error_upper_bound", Json::Int(0));
                out.push("sum_other_doc_count", Json::Int(0));
                out.push("buckets", Json::Arr(Vec::new()));
            }
            _ => {
                out.push("doc_count_error_upper_bound", self.doc_count_error.map(Json::Int).unwrap_or(Json::Null));
                out.push("sum_other_doc_count", Json::Int(self.other_doc_count));
                out.push("buckets", Json::Arr(self.buckets.iter().map(|b| self.render_bucket(b, rc)).collect()));
            }
        }
    }

    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }

    fn ram_bytes(&self) -> usize {
        self.buckets.iter().map(|b| 64 + b.aggs.ram_bytes()).sum::<usize>() + 128
    }
}

impl MultiBuckets for Terms {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        match &self.buckets[i].key {
            TermKey::Bytes(_) => Json::Str(self.key_string(&self.buckets[i].key)),
            k => self.key_json(k, &self.cfg.format),
        }
    }
    fn key_string(&self, i: usize) -> String {
        Terms::key_string(self, &self.buckets[i].key)
    }
    fn aggs(&self, i: usize) -> &Aggs {
        &self.buckets[i].aggs
    }
    fn aggs_mut(&mut self, i: usize) -> &mut Aggs {
        &mut self.buckets[i].aggs
    }
    fn retain_buckets(&mut self, keep: &[bool]) {
        let mut i = 0;
        self.buckets.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
    }
    fn reorder(&mut self, order: &[usize]) {
        let mut slots: Vec<Option<TermsBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
}

/// Properties of a terms bucket (for single-bucket paths through `terms['key']`).
pub fn terms_bucket_property<'a>(t: &'a Terms, i: usize, name: &str, path: &[String]) -> Result<Prop<'a>> {
    bucket_property(name, t.buckets[i].doc_count, || MultiBuckets::key(t, i), &t.buckets[i].aggs, path)
}
