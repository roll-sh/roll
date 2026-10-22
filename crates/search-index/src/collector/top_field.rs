//! Top hits by sort keys (Lucene's `TopFieldCollector` with Elasticsearch's sort semantics).
//!
//! Keys: `_score`, `_doc`, and doc-values fields (numbers, dates, keywords) with
//! `missing: _first | _last | value`, multi-value `mode` (min, max, sum, avg, median as
//! Elasticsearch's `MultiValueMode`), and nested sorts (a parent sorts by the values of its
//! matching children). Hits compare key by key, then by global doc id.
//!
//! Per segment the comparator works on cheap native values: keyword keys compare by ordinal
//! inside the segment, and the queue's worst entry is translated into the segment's ordinal
//! space once (Lucene's `TermOrdValComparator.setBottom`), so a non-competitive doc costs one
//! ordinal lookup and no term bytes. When the sort is a prefix of the index sort, a segment
//! stops after `n` hits (early termination), once the total-hits threshold is reached.

use std::cmp::Ordering;
use std::sync::Arc;

use search_codec::points::encode;
use search_codec::{DocValuesReader, IndexSort, SortMissing, SortedSetDocValues};

use crate::collector::top_docs::TotalRelation;
use crate::error::{Error, Result};
use crate::search::{Collector, LeafCollector, QueryRef, Scorable, ScoreMode, Searcher, SegmentContext};
use crate::util::bitset::FixedBitSet;

/// How doc values of a sort field decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortType {
    /// Longs (long, date, date_nanos, scaled_float, boolean…).
    Long,
    /// 32-bit integers (integer, short, byte): like `Long`, with `Integer` missing bounds.
    Int,
    /// Doubles stored as sortable longs.
    Double,
    /// Floats stored as sortable ints.
    Float,
    /// Half floats stored as sortable shorts.
    HalfFloat,
    /// Terms of sorted / sorted-set doc values (keyword, ip, version…).
    Keyword,
}

/// Multi-value selection (`mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    Min,
    Max,
    Sum,
    Avg,
    Median,
}

/// A sort value (also `search_after` input and hit output).
#[derive(Clone, Debug, PartialEq)]
pub enum SortValue {
    Long(i64),
    Double(f64),
    Float(f32),
    Bytes(Vec<u8>),
    /// No value (missing with `_first`/`_last`).
    Null,
}

impl SortValue {
    fn cmp_same(&self, o: &SortValue) -> Ordering {
        match (self, o) {
            (SortValue::Long(a), SortValue::Long(b)) => a.cmp(b),
            (SortValue::Double(a), SortValue::Double(b)) => a.total_cmp(b),
            (SortValue::Float(a), SortValue::Float(b)) => a.total_cmp(b),
            (SortValue::Bytes(a), SortValue::Bytes(b)) => a.cmp(b),
            (SortValue::Null, SortValue::Null) => Ordering::Equal,
            (SortValue::Long(a), SortValue::Double(b)) => (*a as f64).total_cmp(b),
            (SortValue::Double(a), SortValue::Long(b)) => a.total_cmp(&(*b as f64)),
            _ => Ordering::Equal,
        }
    }
}

/// `missing` of a field sort.
#[derive(Clone, Debug, PartialEq)]
pub enum Missing {
    First,
    Last,
    Value(SortValue),
}

/// Nested sort: parents sort by values of their children.
#[derive(Clone, Debug)]
pub struct NestedSort {
    /// Parent docs of this nesting level (Elasticsearch's parent filter).
    pub parent_filter: QueryRef,
    /// Children to consider (the nested path's docs, with the user's `filter`).
    pub child_filter: QueryRef,
    /// `max_children`.
    pub max_children: Option<usize>,
}

/// One sort key on a field.
#[derive(Clone, Debug)]
pub struct FieldSort {
    pub field: u32,
    pub kind: SortType,
    /// Descending.
    pub reverse: bool,
    pub missing: Missing,
    pub mode: SortMode,
    pub nested: Option<NestedSort>,
}

/// How `_geo_distance` measures (Elasticsearch's `distance_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeoDistanceType {
    /// Haversine (`SloppyMath.haversinMeters`), the default.
    Arc,
    /// Equirectangular approximation.
    Plane,
}

/// `_geo_distance` sort: by the distance between a doc's points and the origin points.
#[derive(Clone, Debug)]
pub struct GeoDistanceSort {
    /// A `geo_point` field (sorted numeric doc values, `LatLonDocValuesField` encoding).
    pub field: u32,
    /// Origins `(lat, lon)`; a doc's values are its distances to each of them.
    pub points: Vec<(f64, f64)>,
    pub distance_type: GeoDistanceType,
    /// Meters per output unit (1 for meters).
    pub unit_meters: f64,
    pub reverse: bool,
    /// `None`: min ascending, max descending (Elasticsearch).
    pub mode: Option<SortMode>,
    pub nested: Option<NestedSort>,
}

/// A script computing a doc's numeric sort value (`_script` sort with `type: number`).
pub trait SortScript: Send + Sync {
    fn leaf(&self, seg: &SegmentContext) -> Result<Box<dyn LeafSortScript>>;
}

/// A sort script bound to a segment.
pub trait LeafSortScript: Send {
    fn number(&mut self, doc: u32) -> Result<f64>;
}

/// `_script` sort (numbers).
#[derive(Clone)]
pub struct ScriptSort {
    pub script: Arc<dyn SortScript>,
    pub reverse: bool,
    pub mode: SortMode,
    pub nested: Option<NestedSort>,
}

impl std::fmt::Debug for ScriptSort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScriptSort(reverse={}, mode={:?})", self.reverse, self.mode)
    }
}

/// One sort key.
#[derive(Clone, Debug)]
pub enum SortKey {
    /// `_score` (descending unless `reverse`).
    Score { reverse: bool },
    /// `_doc` (ascending unless `reverse`).
    Doc { reverse: bool },
    Field(FieldSort),
    /// `_geo_distance`: doubles in the requested unit; docs without points sort as
    /// `Infinity` (last ascending, first descending, as in Elasticsearch).
    GeoDistance(GeoDistanceSort),
    /// `_script` with `type: number`.
    Script(ScriptSort),
}

/// A sorted hit.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldDoc {
    pub doc: u32,
    /// The score when tracked (`NaN` otherwise).
    pub score: f32,
    pub values: Vec<SortValue>,
}

/// Results of a sorted search.
#[derive(Clone, Debug, PartialEq)]
pub struct TopFieldDocs {
    pub total_hits: u64,
    pub relation: TotalRelation,
    pub hits: Vec<FieldDoc>,
    /// Best score among matches when scores are tracked.
    pub max_score: Option<f32>,
}

/// A key value as compared inside one segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Native {
    /// Missing.
    None,
    I(i64),
    F(f64),
    /// Keyword ordinal (doubled; odd values sit between ordinals: a translated bottom).
    Ord(i64),
    Score(f32),
    Doc(u32),
}

/// Per-segment value source of one field key.
enum Source {
    None,
    Numeric(search_codec::NumericDocValues),
    SortedNumeric(search_codec::SortedNumericDocValues),
    Sorted(search_codec::SortedDocValues),
    SortedSet(SortedSetDocValues),
    /// Distances from geo points (sortable longs of doubles).
    Geo(search_codec::SortedNumericDocValues, Arc<GeoDistanceSort>),
    Script(Box<dyn LeafSortScript>),
}

struct NestedLeaf {
    parents: Arc<FixedBitSet>,
    children: Arc<FixedBitSet>,
    max_children: Option<usize>,
}

/// One field key bound to a segment.
pub(crate) struct LeafKey {
    sort: FieldSort,
    src: Source,
    nested: Option<NestedLeaf>,
    scratch: Vec<i64>,
    /// Single-valued numeric doc values read a block at a time (the common sort).
    fast: Option<Box<FastValues>>,
}

/// Sequential reads of a single-valued numeric field: values are decoded 64 at a time, so a
/// doc costs an index computation and a load.
pub(crate) struct FastValues {
    values: search_codec::packed::NumericValues,
    docs: search_codec::DocSet,
    dense: bool,
    total: u64,
    start: u64,
    len: usize,
    buf: [i64; 64],
}

impl FastValues {
    fn of(src: &Source) -> Option<Box<FastValues>> {
        let Source::SortedNumeric(v) = src else { return None };
        let values = v.singleton_values()?.clone();
        Some(Box::new(FastValues {
            total: values.len(),
            values,
            docs: v.docs().clone(),
            dense: v.docs().is_all(),
            start: 0,
            len: 0,
            buf: [0; 64],
        }))
    }

    #[inline]
    fn get(&mut self, doc: u32) -> Option<i64> {
        let idx = if self.dense { doc as u64 } else { self.docs.rank(doc)? as u64 };
        if idx >= self.total {
            return None;
        }
        if idx < self.start || idx >= self.start + self.len as u64 {
            // Decode a block only for sequential access (the next block); jumps (skipping by
            // pruning, sparse filters) read the one value.
            if idx != self.start + self.len as u64 {
                // The next index counts as sequential.
                self.start = idx + 1;
                self.len = 0;
                return Some(self.values.get(idx));
            }
            self.start = idx;
            self.len = (self.total - idx).min(64) as usize;
            self.values.get_range(self.start, &mut self.buf[..self.len]);
        }
        Some(self.buf[(idx - self.start) as usize])
    }
}

fn decode_kind(kind: SortType, v: i64) -> f64 {
    match kind {
        SortType::Double => encode::sortable_f64(v),
        SortType::Float => encode::sortable_f32(v as i32) as f64,
        SortType::HalfFloat => encode::sortable_f16(v as i16) as f64,
        _ => v as f64,
    }
}

impl LeafKey {
    fn decode(&self, v: i64) -> f64 {
        decode_kind(self.sort.kind, v)
    }

    /// Collects raw values of one doc into `scratch`.
    fn values_of(&mut self, doc: u32) {
        match &mut self.src {
            Source::None => {}
            Source::Numeric(v) => {
                if let Some(x) = v.get(doc) {
                    self.scratch.push(x);
                }
            }
            Source::SortedNumeric(v) => self.scratch.extend(v.values(doc)),
            Source::Sorted(v) => {
                if let Some(o) = v.ord(doc) {
                    self.scratch.push(o as i64);
                }
            }
            Source::SortedSet(v) => self.scratch.extend(v.ords(doc).map(|o| o as i64)),
            Source::Geo(v, g) => {
                let start = self.scratch.len();
                for p in v.values(doc) {
                    let (lat, lon) = crate::util::geo::decode_geo_dv(p);
                    for &(olat, olon) in &g.points {
                        let m = match g.distance_type {
                            GeoDistanceType::Arc => crate::util::geo::haversin_meters(olat, olon, lat, lon),
                            GeoDistanceType::Plane => crate::util::geo::plane_meters(olat, olon, lat, lon),
                        };
                        let d = if g.unit_meters == 1.0 { m } else { m / g.unit_meters };
                        self.scratch.push(encode::f64_sortable(d));
                    }
                }
                // Each doc's distances in order, as Elasticsearch sorts them before selecting.
                self.scratch[start..].sort_unstable();
            }
            Source::Script(sc) => match sc.number(doc) {
                Ok(v) => self.scratch.push(encode::f64_sortable(v)),
                Err(e) => crate::search::defer_error(e),
            },
        }
    }

    #[inline]
    fn native(&mut self, doc: u32) -> Native {
        if let Some(f) = self.fast.as_mut() {
            // One value: every mode selects it.
            return match f.get(doc) {
                Some(v) => self.num(v),
                None => Native::None,
            };
        }
        self.scratch.clear();
        if let Some(n) = &self.nested {
            // Children of this parent: docs after the previous parent, up to this one.
            let start = if doc == 0 { 0 } else { n.parents.prev_set_bit(doc - 1).map_or(0, |p| p + 1) };
            let mut seen = 0usize;
            let children = n.children.clone();
            let max = n.max_children;
            let mut c = children.next_set_bit(start);
            while c < doc {
                if max.is_some_and(|m| seen >= m) {
                    break;
                }
                seen += 1;
                self.values_of(c);
                c = children.next_set_bit(c + 1);
            }
        } else {
            self.values_of(doc);
        }
        if self.scratch.is_empty() {
            return Native::None;
        }
        let keyword = self.sort.kind == SortType::Keyword;
        let kind = self.sort.kind;
        let s = &mut self.scratch;
        match self.sort.mode {
            SortMode::Min => {
                let m = *s.iter().min().unwrap();
                if keyword { Native::Ord(m * 2) } else { self.num(m) }
            }
            SortMode::Max => {
                let m = *s.iter().max().unwrap();
                if keyword { Native::Ord(m * 2) } else { self.num(m) }
            }
            SortMode::Sum | SortMode::Avg | SortMode::Median if keyword => {
                let m = if self.sort.reverse { *s.iter().max().unwrap() } else { *s.iter().min().unwrap() };
                Native::Ord(m * 2)
            }
            SortMode::Sum => {
                if self.sort.kind.is_integral() {
                    Native::I(s.iter().fold(0i64, |a, &b| a.wrapping_add(b)))
                } else {
                    let mut t = 0f64;
                    for &v in s.iter() {
                        t += decode_kind(kind, v);
                    }
                    float_key(kind, t)
                }
            }
            SortMode::Avg => {
                let n = s.len();
                if self.sort.kind.is_integral() {
                    let t = s.iter().fold(0i64, |a, &b| a.wrapping_add(b));
                    Native::I(if n > 1 { java_round(t as f64 / n as f64) } else { t })
                } else {
                    let mut t = 0f64;
                    for &v in s.iter() {
                        t += decode_kind(kind, v);
                    }
                    float_key(kind, t / n as f64)
                }
            }
            SortMode::Median => {
                s.sort_unstable();
                let n = s.len();
                if self.sort.kind.is_integral() {
                    if n.is_multiple_of(2) {
                        Native::I(java_round((s[n / 2 - 1] as f64 + s[n / 2] as f64) / 2.0))
                    } else {
                        Native::I(s[n / 2])
                    }
                } else if n.is_multiple_of(2) {
                    float_key(kind, (decode_kind(kind, s[n / 2 - 1]) + decode_kind(kind, s[n / 2])) / 2.0)
                } else {
                    Native::F(decode_kind(kind, s[n / 2]))
                }
            }
        }
    }

    fn num(&self, v: i64) -> Native {
        match self.sort.kind {
            SortType::Long | SortType::Int => Native::I(v),
            _ => Native::F(self.decode(v)),
        }
    }

    /// The output value of a native value.
    fn value(&self, n: Native) -> SortValue {
        match n {
            Native::None => match &self.sort.missing {
                Missing::Value(v) => v.clone(),
                m => missing_bound(self.sort.kind, matches!(m, Missing::First), self.sort.reverse).unwrap_or(SortValue::Null),
            },
            Native::I(v) => SortValue::Long(v),
            Native::F(v) => match self.sort.kind {
                SortType::Float | SortType::HalfFloat => SortValue::Float(v as f32),
                _ => SortValue::Double(v),
            },
            Native::Ord(o) => {
                let mut b = Vec::new();
                let ord = (o / 2) as u64;
                match &self.src {
                    Source::Sorted(v) => v.lookup_ord(ord, &mut b),
                    Source::SortedSet(v) => v.lookup_ord(ord, &mut b),
                    _ => {}
                }
                SortValue::Bytes(b)
            }
            Native::Score(s) => SortValue::Float(s),
            Native::Doc(d) => SortValue::Long(d as i64),
        }
    }

    /// A sort value in this segment's native space (for comparing against the queue bottom).
    fn to_native(&self, v: &SortValue) -> Native {
        match v {
            SortValue::Null => Native::None,
            SortValue::Long(x) => match self.sort.kind {
                SortType::Long | SortType::Int => Native::I(*x),
                _ => Native::F(*x as f64),
            },
            SortValue::Double(x) => match self.sort.kind {
                SortType::Long | SortType::Int => Native::F(*x),
                _ => Native::F(*x),
            },
            SortValue::Float(x) => Native::F(*x as f64),
            SortValue::Bytes(b) => {
                let r = match &self.src {
                    Source::Sorted(v) => v.lookup_term(b),
                    Source::SortedSet(v) => v.lookup_term(b),
                    _ => Err(0),
                };
                match r {
                    Ok(o) => Native::Ord(o as i64 * 2),
                    Err(ins) => Native::Ord(ins as i64 * 2 - 1),
                }
            }
        }
    }
}

/// A computed (sum, average, median) key of a floating-point field: Elasticsearch computes in
/// double and hands float fields' comparators `(float)` of it, so float keys tie at float
/// precision.
fn float_key(kind: SortType, v: f64) -> Native {
    match kind {
        SortType::Float | SortType::HalfFloat => Native::F(v as f32 as f64),
        _ => Native::F(v),
    }
}

/// Java's `Math.round(double)`: `floor(x + 0.5)`.
fn java_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

impl SortType {
    fn is_integral(self) -> bool {
        matches!(self, SortType::Long | SortType::Int)
    }
}

/// The value Elasticsearch substitutes for a missing numeric sort value with `_first` /
/// `_last` (the type's extreme in the direction that puts it there); `None` for keywords.
fn missing_bound(kind: SortType, first: bool, reverse: bool) -> Option<SortValue> {
    let min = first ^ reverse;
    Some(match kind {
        SortType::Long => SortValue::Long(if min { i64::MIN } else { i64::MAX }),
        SortType::Int => SortValue::Long(if min { i32::MIN as i64 } else { i32::MAX as i64 }),
        SortType::Double => SortValue::Double(if min { f64::NEG_INFINITY } else { f64::INFINITY }),
        SortType::Float | SortType::HalfFloat => SortValue::Float(if min { f32::NEG_INFINITY } else { f32::INFINITY }),
        SortType::Keyword => return None,
    })
}

/// Compares natives of one key: ascending order before `reverse`, missing per `missing`.
fn cmp_native(a: Native, b: Native, sort: &FieldSort, missing_native: Option<Native>) -> Ordering {
    let (a, b) = match missing_native {
        Some(m) => (if a == Native::None { m } else { a }, if b == Native::None { m } else { b }),
        None => (a, b),
    };
    let first = matches!(sort.missing, Missing::First);
    let o = match (a, b) {
        (Native::None, Native::None) => return Ordering::Equal,
        // Elasticsearch's _first/_last refer to the final order, whatever `reverse` is.
        (Native::None, _) => return if first { Ordering::Less } else { Ordering::Greater },
        (_, Native::None) => return if first { Ordering::Greater } else { Ordering::Less },
        (Native::I(x), Native::I(y)) => x.cmp(&y),
        (Native::F(x), Native::F(y)) => x.total_cmp(&y),
        (Native::I(x), Native::F(y)) => (x as f64).total_cmp(&y),
        (Native::F(x), Native::I(y)) => x.total_cmp(&(y as f64)),
        (Native::Ord(x), Native::Ord(y)) => x.cmp(&y),
        _ => Ordering::Equal,
    };
    if sort.reverse { o.reverse() } else { o }
}

/// An entry of the queue (global doc, score, output values, natives of its segment).
#[derive(Clone, Debug)]
struct Entry {
    doc: u32,
    score: f32,
    values: Vec<SortValue>,
}

/// Collects the top `n` hits by `keys`.
pub struct TopFieldCollector {
    keys: Vec<SortKey>,
    n: usize,
    threshold: u64,
    track_scores: bool,
    searcher: Option<Searcher>,
    /// Sorted best first; at most `n` entries.
    queue: Vec<Entry>,
    total: u64,
    relation: TotalRelation,
    max_score: f32,
    after: Option<(Vec<SortValue>, Option<u32>)>,
    early_terminate: bool,
    /// The hit count of the whole search passed `threshold` (pruning and early termination may
    /// start; the total becomes a lower bound).
    passed: bool,
    /// Hit count shared with the other collectors of a concurrent search.
    shared: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// Part of `total` already added to `shared`.
    flushed: u64,
}

impl TopFieldCollector {
    /// `threshold`: hits counted exactly (`u64::MAX`: all). `searcher` is needed for nested
    /// sorts (their filters are queries).
    pub fn new(keys: Vec<SortKey>, n: usize, threshold: u64, track_scores: bool) -> TopFieldCollector {
        TopFieldCollector {
            keys,
            n,
            threshold,
            track_scores,
            searcher: None,
            queue: Vec::new(),
            total: 0,
            relation: TotalRelation::EqualTo,
            max_score: f32::NEG_INFINITY,
            after: None,
            early_terminate: false,
            passed: false,
            shared: None,
            flushed: 0,
        }
    }

    /// Counts hits against the threshold together with the other collectors of a concurrent
    /// search (Lucene's `HitsThresholdChecker`).
    pub fn with_shared_hits(mut self, hits: Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.shared = Some(hits);
        self
    }

    fn flush_hits(&mut self) {
        if let Some(sh) = &self.shared {
            sh.fetch_add(self.total - self.flushed, std::sync::atomic::Ordering::Relaxed);
            self.flushed = self.total;
        }
    }

    /// Accounts the hit just counted; true exactly when the search's count passes the
    /// threshold.
    #[inline]
    fn note_hit(&mut self) -> bool {
        if self.passed {
            return false;
        }
        let global = if self.shared.is_some() {
            if self.total - self.flushed >= 64 {
                self.flush_hits();
            }
            let others = self.shared.as_ref().map_or(0, |sh| sh.load(std::sync::atomic::Ordering::Relaxed));
            others + (self.total - self.flushed)
        } else {
            self.total
        };
        if global > self.threshold {
            self.passed = true;
            return true;
        }
        false
    }

    pub fn with_searcher(mut self, s: &Searcher) -> Self {
        self.early_terminate = can_early_terminate(&self.keys, s.schema().index_sort.as_ref());
        self.searcher = Some(s.clone());
        self
    }

    /// `search_after`: only hits after these values (and after `doc` on ties; `None`: ties
    /// are excluded, as Elasticsearch does).
    pub fn with_after(mut self, values: Vec<SortValue>, doc: Option<u32>) -> Self {
        self.after = Some((values, doc));
        self
    }

    fn needs_scores(&self) -> bool {
        self.track_scores || self.keys.iter().any(|k| matches!(k, SortKey::Score { .. }))
    }

    pub fn top_docs(mut self) -> TopFieldDocs {
        self.flush_hits();
        let max_score = (self.needs_scores() && self.total > 0).then_some(self.max_score);
        TopFieldDocs {
            total_hits: self.total,
            relation: self.relation,
            hits: self
                .queue
                .into_iter()
                .map(|e| FieldDoc { doc: e.doc, score: if e.score.is_nan() { f32::NAN } else { e.score }, values: e.values })
                .collect(),
            max_score,
        }
    }
}

/// Whether `keys` is a prefix of the index sort (same fields, directions and missing).
pub fn can_early_terminate(keys: &[SortKey], sort: Option<&IndexSort>) -> bool {
    let Some(sort) = sort else { return false };
    if keys.is_empty() || keys.len() > sort.fields.len() {
        return false;
    }
    keys.iter().zip(&sort.fields).all(|(k, f)| match k {
        SortKey::Field(fs) => {
            fs.nested.is_none()
                && fs.field == f.field
                && fs.reverse == f.reverse
                && match fs.missing {
                    Missing::First => f.missing == SortMissing::First,
                    Missing::Last => f.missing == SortMissing::Last,
                    Missing::Value(_) => false,
                }
                && matches!((fs.mode, f.selector), (SortMode::Min, search_codec::SortSelector::Min) | (SortMode::Max, search_codec::SortSelector::Max))
        }
        _ => false,
    })
}

pub(crate) enum LeafKeyKind {
    Score(bool),
    Doc(bool),
    Field(Box<LeafKey>, Option<Native>),
}

struct TopFieldLeaf<'a> {
    c: &'a mut TopFieldCollector,
    base: u32,
    keys: Vec<LeafKeyKind>,
    /// Natives of the worst queued entry in this segment's space (when the queue is full).
    bottom: Option<Vec<Native>>,
    after: Option<Vec<Native>>,
    natives: Vec<Native>,
    collected: u64,
    pruner: Option<Pruner>,
    /// `TopFieldCollector::needs_scores`, computed once.
    scores: bool,
    /// The sort is one numeric field key: compare natives directly.
    single_numeric: Option<(bool, Option<Native>)>,
}

const MIN_SKIP_INTERVAL: u32 = 32;
const MAX_SKIP_INTERVAL: u32 = 8192;

/// Lucene's numeric sort optimization (`NumericComparator` with points): once the hit count
/// passed the threshold and the queue is full, the docs whose value of the first sort key can
/// still beat the queue's worst entry (and come after `search_after`) are read from the
/// field's BKD tree, when that cuts the docs to visit at least eightfold; the bulk scorer then
/// jumps over the others. Recomputed as the worst entry improves, sampled once updates get
/// frequent.
struct Pruner {
    field: u32,
    reader: search_codec::SegmentReader,
    /// Bytes per point value: 8 (long, double) or 4 (int, float).
    width: usize,
    reverse: bool,
    /// The bound `search_after` puts on the other side, in the doc values' integer space.
    after: Option<i64>,
    /// Whether docs without a value can compete (then nothing can be skipped).
    missing_competitive: bool,
    /// Competitive docs, when materialized.
    set: Option<crate::query::basic::SetScorer>,
    cost: u64,
    last: Option<i64>,
    calls: u32,
    interval: u32,
    fails: u32,
}

/// A native value of a numeric key in its doc values' integer space.
fn native_int(kind: SortType, n: Native) -> Option<i64> {
    match (kind, n) {
        (SortType::Long | SortType::Int, Native::I(v)) => Some(v),
        (SortType::Double, Native::F(v)) => Some(encode::f64_sortable(v)),
        (SortType::Float, Native::F(v)) => Some(encode::f32_sortable(v as f32) as i64),
        _ => None,
    }
}

impl Pruner {
    /// A pruner for the first key when it can use the field's points.
    fn new(c: &TopFieldCollector, seg: &SegmentContext, keys: &[LeafKeyKind], after: Option<&[Native]>) -> Option<Pruner> {
        if c.threshold == u64::MAX || c.n == 0 || c.early_terminate {
            return None;
        }
        let SortKey::Field(fs) = c.keys.first()? else { return None };
        if fs.nested.is_some() || !matches!(fs.mode, SortMode::Min | SortMode::Max) {
            return None;
        }
        let width = match fs.kind {
            SortType::Long | SortType::Double => 8,
            SortType::Int | SortType::Float => 4,
            _ => return None,
        };
        let points = seg.reader.points(fs.field)?;
        if points.spec().dims != 1 || points.spec().bytes_per_dim as usize != width {
            return None;
        }
        // Docs without a value sort by the missing value: when it can beat values, they must
        // be visited.
        let missing_competitive = points.doc_count() < seg.reader.max_doc()
            && match &fs.missing {
                Missing::First => true,
                Missing::Last => false,
                Missing::Value(_) => true,
            };
        if missing_competitive {
            return None;
        }
        let after = match (after, keys.first()) {
            (Some(a), Some(LeafKeyKind::Field(..))) => Some(native_int(fs.kind, a[0])?),
            _ => None,
        };
        Some(Pruner {
            field: fs.field,
            reader: seg.reader.clone(),
            width,
            reverse: fs.reverse,
            after,
            missing_competitive,
            set: None,
            cost: seg.reader.max_doc() as u64,
            last: None,
            calls: 0,
            interval: MIN_SKIP_INTERVAL,
            fails: 0,
        })
    }

    fn update_interval(&mut self, success: bool) {
        if self.calls > 256 {
            if success {
                self.interval = (self.interval / 2).max(MIN_SKIP_INTERVAL);
                self.fails = 0;
            } else if self.fails >= 3 {
                self.interval = (self.interval * 2).min(MAX_SKIP_INTERVAL);
                self.fails = 0;
            } else {
                self.fails += 1;
            }
        }
    }

    /// The worst queued value changed to `bottom`; docs from `from` on are still to come.
    fn update(&mut self, bottom: i64, from: u32) {
        if self.missing_competitive || self.last == Some(bottom) {
            return;
        }
        self.calls += 1;
        if self.calls > 256 && (self.calls & (self.interval - 1)) != self.interval - 1 {
            return;
        }
        self.last = Some(bottom);
        // Competitive values: at least the bottom (descending) or at most it (ascending);
        // equal values stay, since ties may break on doc ids still to come in other segments.
        let (lo, hi) = if self.reverse { (bottom, self.after.unwrap_or(i64::MAX)) } else { (self.after.unwrap_or(i64::MIN), bottom) };
        let Some(points) = self.reader.points(self.field) else { return };
        let mut v = RangeDocs { lo, hi, width: self.width, from, b: crate::util::bitset::DocIdSetBuilder::new(self.reader.max_doc()) };
        if points.estimate_point_count(&mut v) >= self.cost >> 3 {
            self.update_interval(false);
            return;
        }
        points.intersect(&mut v);
        let set = v.b.build();
        self.cost = set.cost();
        self.set = Some(crate::query::basic::SetScorer::new(Arc::new(set), 0.0));
        self.update_interval(true);
    }

    #[inline]
    fn next_from(&mut self, doc: u32) -> u32 {
        use crate::search::Scorer;
        match self.set.as_mut() {
            None => doc,
            Some(s) => {
                if s.doc() != u32::MAX && s.doc() >= doc {
                    s.doc()
                } else {
                    s.advance(doc)
                }
            }
        }
    }
}

/// Collects the docs from `from` on whose point lies in `[lo, hi]`.
struct RangeDocs {
    lo: i64,
    hi: i64,
    width: usize,
    from: u32,
    b: crate::util::bitset::DocIdSetBuilder,
}

impl RangeDocs {
    #[inline]
    fn value(&self, packed: &[u8]) -> i64 {
        if self.width == 8 { encode::i64_from(&packed[..8]) } else { encode::i32_from(&packed[..4]) as i64 }
    }
}

impl search_codec::PointVisitor for RangeDocs {
    fn compare(&mut self, min: &[u8], max: &[u8]) -> search_codec::Relation {
        let (a, b) = (self.value(min), self.value(max));
        if a > self.hi || b < self.lo {
            search_codec::Relation::CellOutsideQuery
        } else if a < self.lo || b > self.hi {
            search_codec::Relation::CellCrossesQuery
        } else {
            search_codec::Relation::CellInsideQuery
        }
    }
    fn visit(&mut self, doc: u32) {
        if doc >= self.from {
            self.b.add(doc);
        }
    }
    fn visit_value(&mut self, doc: u32, packed: &[u8]) {
        let v = self.value(packed);
        if doc >= self.from && v >= self.lo && v <= self.hi {
            self.b.add(doc);
        }
    }
    fn visit_doc_range(&mut self, start: u32, end: u32) {
        let start = start.max(self.from);
        if start < end {
            self.b.add_range(start, end);
        }
    }
    fn grow(&mut self, n: usize) {
        self.b.grow(n);
    }
}

/// Computes the natives of `doc` for every key.
pub(crate) fn compute_natives(keys: &mut [LeafKeyKind], base: u32, doc: u32, score: f32, out: &mut Vec<Native>) {
    out.clear();
    for k in keys.iter_mut() {
        let n = match k {
            LeafKeyKind::Score(_) => Native::Score(score),
            LeafKeyKind::Doc(_) => Native::Doc(base + doc),
            LeafKeyKind::Field(lk, _) => lk.native(doc),
        };
        out.push(n);
    }
}

/// Compares natives key by key (`Less`: `a` sorts first).
pub(crate) fn cmp_natives(keys: &[LeafKeyKind], a: &[Native], b: &[Native]) -> Ordering {
    for (i, k) in keys.iter().enumerate() {
        let o = match (k, a[i], b[i]) {
            (LeafKeyKind::Score(rev), Native::Score(x), Native::Score(y)) => {
                let o = y.total_cmp(&x);
                if *rev { o.reverse() } else { o }
            }
            (LeafKeyKind::Doc(rev), Native::Doc(x), Native::Doc(y)) => {
                let o = x.cmp(&y);
                if *rev { o.reverse() } else { o }
            }
            (LeafKeyKind::Field(lk, m), x, y) => cmp_native(x, y, &lk.sort, *m),
            _ => Ordering::Equal,
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// Output values in this segment's native space.
pub(crate) fn natives_of(keys: &[LeafKeyKind], values: &[SortValue], doc: u32, score: f32) -> Vec<Native> {
    keys.iter()
        .zip(values)
        .map(|(k, v)| match k {
            LeafKeyKind::Score(_) => Native::Score(match v {
                SortValue::Float(f) => *f,
                SortValue::Double(d) => *d as f32,
                _ => score,
            }),
            LeafKeyKind::Doc(_) => Native::Doc(match v {
                SortValue::Long(d) => *d as u32,
                _ => doc,
            }),
            LeafKeyKind::Field(lk, _) => lk.to_native(v),
        })
        .collect()
}

/// The output values of natives.
pub(crate) fn output_values(keys: &[LeafKeyKind], natives: &[Native], score: f32, gdoc: u32) -> Vec<SortValue> {
    keys.iter()
        .zip(natives)
        .map(|(k, n)| match k {
            LeafKeyKind::Score(_) => SortValue::Float(score),
            LeafKeyKind::Doc(_) => SortValue::Long(gdoc as i64),
            LeafKeyKind::Field(lk, _) => lk.value(*n),
        })
        .collect()
}

impl TopFieldLeaf<'_> {
    fn compute(&mut self, doc: u32, score: f32) {
        compute_natives(&mut self.keys, self.base, doc, score, &mut self.natives);
    }

    fn cmp(&self, a: &[Native], b: &[Native]) -> Ordering {
        cmp_natives(&self.keys, a, b)
    }

    fn natives_of(&self, values: &[SortValue], doc: u32, score: f32) -> Vec<Native> {
        natives_of(&self.keys, values, doc, score)
    }

    /// Narrows the competitive docs to the queue's worst value (after `doc`).
    /// Re-derives the competitive docs; `from` is the first doc still to be collected.
    fn prune(&mut self, from: u32) {
        if !self.c.passed {
            return;
        }
        let (Some(p), Some(b)) = (self.pruner.as_mut(), self.bottom.as_ref()) else { return };
        let SortKey::Field(fs) = &self.c.keys[0] else { return };
        if let Some(v) = native_int(fs.kind, b[0]) {
            p.update(v, from);
        }
    }

    fn refresh_bottom(&mut self) {
        if self.c.queue.len() >= self.c.n && self.c.n > 0 {
            let w = self.c.queue.last().unwrap();
            let (values, doc, score) = (w.values.clone(), w.doc, w.score);
            self.bottom = Some(self.natives_of(&values, doc, score));
        }
    }
}

impl LeafCollector for TopFieldLeaf<'_> {
    fn skips(&self) -> bool {
        self.pruner.is_some()
    }

    #[inline]
    fn competitive_from(&mut self, doc: u32) -> u32 {
        match self.pruner.as_mut() {
            Some(p) => p.next_from(doc),
            None => doc,
        }
    }

    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> Result<()> {
        self.c.total += 1;
        let just_passed = self.c.note_hit();
        if self.c.passed {
            self.c.relation = TotalRelation::GreaterThanOrEqualTo;
        }
        let score = if self.scores { scorer.score() } else { f32::NAN };
        if self.scores && score > self.c.max_score {
            self.c.max_score = score;
        }
        if self.c.n == 0 {
            return Ok(());
        }
        // One numeric key without search_after: a non-competitive doc costs one value read
        // and one comparison.
        if let (Some((reverse, missing)), Some(b), None) = (self.single_numeric, self.bottom.as_ref(), self.after.as_ref())
            && !just_passed
        {
            let LeafKeyKind::Field(lk, _) = &mut self.keys[0] else { unreachable!() };
            let n = lk.native(doc);
            let n = if n == Native::None { missing.unwrap_or(Native::None) } else { n };
            let worse = match (n, b[0]) {
                (Native::I(x), Native::I(y)) => if reverse { x <= y } else { x >= y },
                (Native::F(x), Native::F(y)) => {
                    let o = x.total_cmp(&y);
                    if reverse { o != Ordering::Greater } else { o != Ordering::Less }
                }
                _ => false,
            };
            if worse {
                self.collected += 1;
                if self.c.early_terminate && self.c.passed {
                    return Err(Error::CollectionTerminated);
                }
                return Ok(());
            }
            self.natives.clear();
            self.natives.push(n);
            return self.insert(doc, score);
        }
        self.compute(doc, score);
        let gdoc = self.base + doc;
        if let Some(a) = &self.after {
            match self.cmp(&self.natives, a) {
                Ordering::Less => return Ok(()),
                Ordering::Equal => match self.c.after.as_ref().and_then(|x| x.1) {
                    Some(ad) if gdoc > ad => {}
                    _ => return Ok(()),
                },
                Ordering::Greater => {}
            }
        }
        if just_passed {
            // The hit count just passed the threshold: pruning may start.
            self.prune(doc + 1);
        }
        if let Some(b) = &self.bottom {
            // Not better than the worst queued hit (ties lose: later doc ids).
            if self.cmp(&self.natives, b) != Ordering::Less {
                self.collected += 1;
                if self.c.early_terminate && self.c.passed {
                    return Err(Error::CollectionTerminated);
                }
                return Ok(());
            }
        }
        self.insert(doc, score)
    }
}

impl TopFieldLeaf<'_> {
    /// Queues `doc` (its natives are in `self.natives`), dropping the worst entry when full.
    fn insert(&mut self, doc: u32, score: f32) -> Result<()> {
        let gdoc = self.base + doc;
        let values = output_values(&self.keys, &self.natives, score, gdoc);
        let e = Entry { doc: gdoc, score, values };
        // Insert in order (the queue is small; binary search on the values).
        let natives = self.natives.clone();
        let pos = {
            let mut lo = 0;
            let mut hi = self.c.queue.len();
            while lo < hi {
                let mid = (lo + hi) / 2;
                let q = &self.c.queue[mid];
                let qn = self.natives_of(&q.values, q.doc, q.score);
                let o = self.cmp(&natives, &qn).then(gdoc.cmp(&q.doc));
                if o == Ordering::Less {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            lo
        };
        self.c.queue.insert(pos, e);
        if self.c.queue.len() > self.c.n {
            self.c.queue.pop();
        }
        self.refresh_bottom();
        self.prune(doc + 1);
        self.collected += 1;
        if self.c.early_terminate && self.collected >= self.c.n as u64 && self.c.passed {
            return Err(Error::CollectionTerminated);
        }
        Ok(())
    }
}

impl Collector for TopFieldCollector {
    fn score_mode(&self) -> ScoreMode {
        if self.needs_scores() { ScoreMode::Complete } else { ScoreMode::CompleteNoScores }
    }

    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'a>> {
        let keys = build_leaf_keys(&self.keys, seg, self.searcher.as_ref())?;
        let base = seg.base;
        let scores = self.needs_scores();
        let single_numeric = match keys.as_slice() {
            [LeafKeyKind::Field(lk, missing)] if lk.fast.is_some() => Some((lk.sort.reverse, *missing)),
            _ => None,
        };
        let mut leaf = TopFieldLeaf {
            c: self,
            base,
            keys,
            bottom: None,
            after: None,
            natives: Vec::new(),
            collected: 0,
            pruner: None,
            scores,
            single_numeric,
        };
        if let Some((vals, _)) = leaf.c.after.clone() {
            if vals.len() != leaf.keys.len() {
                return Err(Error::illegal(format!(
                    "search_after has {} value(s) but sort has {}.",
                    vals.len(),
                    leaf.keys.len()
                )));
            }
            leaf.after = Some(leaf.natives_of(&vals, u32::MAX, f32::NAN));
        }
        leaf.pruner = Pruner::new(leaf.c, seg, &leaf.keys, leaf.after.as_deref());
        leaf.refresh_bottom();
        leaf.prune(0);
        Ok(Box::new(leaf))
    }
}

/// Orders two hits of the same sort (as the collector does; for merging slices or shards):
/// key by key, then by doc id. `Null` values follow their key's `missing` (`_last` unless
/// `_first`), whatever the direction.
pub fn compare_field_docs(keys: &[SortKey], a: &FieldDoc, b: &FieldDoc) -> Ordering {
    for (i, k) in keys.iter().enumerate() {
        let (x, y) = (a.values.get(i), b.values.get(i));
        let o = match k {
            SortKey::Score { reverse } => {
                let (sa, sb) = (score_of(x, a.score), score_of(y, b.score));
                let o = sb.total_cmp(&sa);
                if *reverse { o.reverse() } else { o }
            }
            SortKey::Doc { reverse } => {
                let o = a.doc.cmp(&b.doc);
                if *reverse { o.reverse() } else { o }
            }
            SortKey::GeoDistance(GeoDistanceSort { reverse, .. }) | SortKey::Script(ScriptSort { reverse, .. }) => {
                let (x, y) = (x.unwrap_or(&SortValue::Null), y.unwrap_or(&SortValue::Null));
                let o = x.cmp_same(y);
                if *reverse { o.reverse() } else { o }
            }
            SortKey::Field(fs) => {
                let (x, y) = (x.unwrap_or(&SortValue::Null), y.unwrap_or(&SortValue::Null));
                let first = matches!(fs.missing, Missing::First);
                match (x, y) {
                    (SortValue::Null, SortValue::Null) => Ordering::Equal,
                    (SortValue::Null, _) => {
                        if first {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        }
                    }
                    (_, SortValue::Null) => {
                        if first {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                    _ => {
                        let o = x.cmp_same(y);
                        if fs.reverse { o.reverse() } else { o }
                    }
                }
            }
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.doc.cmp(&b.doc)
}

fn score_of(v: Option<&SortValue>, score: f32) -> f32 {
    match v {
        Some(SortValue::Float(f)) => *f,
        Some(SortValue::Double(d)) => *d as f32,
        _ => score,
    }
}

/// Binds sort keys to a segment.
pub(crate) fn build_leaf_keys(sort: &[SortKey], seg: &SegmentContext, searcher: Option<&Searcher>) -> Result<Vec<LeafKeyKind>> {
    let mut keys = Vec::with_capacity(sort.len());
    for k in sort {
        keys.push(match k {
            SortKey::Score { reverse } => LeafKeyKind::Score(*reverse),
            SortKey::Doc { reverse } => LeafKeyKind::Doc(*reverse),
            SortKey::Field(fs) => {
                let src = match seg.reader.doc_values(fs.field) {
                    None => Source::None,
                    Some(DocValuesReader::Numeric(v)) => Source::Numeric(v.clone()),
                    Some(DocValuesReader::SortedNumeric(v)) => Source::SortedNumeric(v.clone()),
                    Some(DocValuesReader::Sorted(v)) => Source::Sorted(v.clone()),
                    Some(DocValuesReader::SortedSet(v)) => Source::SortedSet(v.clone()),
                    Some(DocValuesReader::Binary(_)) => {
                        return Err(Error::illegal(format!("cannot sort on binary doc values field [{}]", fs.field)));
                    }
                };
                let nested = match &fs.nested {
                    None => None,
                    Some(n) => {
                        let s = searcher.ok_or_else(|| Error::illegal_state("nested sort needs the searcher"))?;
                        Some(NestedLeaf {
                            parents: crate::query::nested::segment_bits(s, seg, &n.parent_filter)?,
                            children: crate::query::nested::segment_bits(s, seg, &n.child_filter)?,
                            max_children: n.max_children,
                        })
                    }
                };
                let fast = if nested.is_none() { FastValues::of(&src) } else { None };
                let mut lk = LeafKey { sort: fs.clone(), src, nested, scratch: Vec::new(), fast };
                let missing = match &fs.missing {
                    Missing::Value(v) => Some(lk.to_native(v)),
                    m => missing_bound(fs.kind, matches!(m, Missing::First), fs.reverse).map(|v| lk.to_native(&v)),
                };
                // A keyword missing value between ordinals must stay comparable.
                lk.scratch.reserve(4);
                LeafKeyKind::Field(Box::new(lk), missing)
            }
            SortKey::GeoDistance(g) => {
                let src = match seg.reader.doc_values(g.field) {
                    Some(DocValuesReader::SortedNumeric(v)) => Source::Geo(v.clone(), Arc::new(g.clone())),
                    _ => Source::None,
                };
                let mode = g.mode.unwrap_or(if g.reverse { SortMode::Max } else { SortMode::Min });
                derived_key(g.field, g.reverse, mode, g.nested.as_ref(), src, seg, searcher)?
            }
            SortKey::Script(sc) => {
                let src = Source::Script(sc.script.leaf(seg)?);
                derived_key(u32::MAX, sc.reverse, sc.mode, sc.nested.as_ref(), src, seg, searcher)?
            }
        });
    }
    Ok(keys)
}

/// A double-valued key computed from doc values or a script (missing: `Infinity`).
fn derived_key(
    field: u32,
    reverse: bool,
    mode: SortMode,
    nested: Option<&NestedSort>,
    src: Source,
    seg: &SegmentContext,
    searcher: Option<&Searcher>,
) -> Result<LeafKeyKind> {
    let nested_leaf = match nested {
        None => None,
        Some(n) => {
            let s = searcher.ok_or_else(|| Error::illegal_state("nested sort needs the searcher"))?;
            Some(NestedLeaf {
                parents: crate::query::nested::segment_bits(s, seg, &n.parent_filter)?,
                children: crate::query::nested::segment_bits(s, seg, &n.child_filter)?,
                max_children: n.max_children,
            })
        }
    };
    let sort = FieldSort {
        field,
        kind: SortType::Double,
        reverse,
        missing: Missing::Value(SortValue::Double(f64::INFINITY)),
        mode,
        nested: nested.cloned(),
    };
    let lk = LeafKey { sort, src, nested: nested_leaf, scratch: Vec::new(), fast: None };
    let missing = Some(lk.to_native(&SortValue::Double(f64::INFINITY)));
    Ok(LeafKeyKind::Field(Box::new(lk), missing))
}

impl PartialOrd for SortValue {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp_same(o))
    }
}
