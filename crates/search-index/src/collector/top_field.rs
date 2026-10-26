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
//!
//! Keyword values a segment lacks (a queued value from another segment, `search_after`, a
//! custom `missing`) land between its ordinals ([`ORD_SCALE`]). A custom missing value the
//! segment lacks gets a slot of its own in its gap, and other foreign values in that gap sort
//! below or above it by their bytes — Elasticsearch's `ReplaceMissing` inserts the missing
//! term into the segment's ordinals, so missing docs order exactly against every value.
//!
//! Queue entries keep their natives together with the segment generation they belong to; an
//! entry from an earlier segment is translated (one terms-dictionary lookup per keyword key)
//! the first time the current segment compares against it, into its own buffer. A competitive
//! doc therefore costs no allocation once the queue is full: it reuses the evicted entry's
//! buffers (Lucene's per-slot comparators).

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
    /// Unsigned longs stored sign-flipped (`u ^ 1 << 63`, so the longs order as the unsigned
    /// values): like `Long`, except that `sum`, `avg` and `median` compute in unsigned 64-bit
    /// arithmetic (OpenSearch's `unsigned_long` sort; Elasticsearch sorts them as `Long`).
    UnsignedLong,
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
            // Shards of indices mapping the field differently (`long`, `double`, `half_float`
            // across indices) merge on the widened double, as the coordinator does.
            (SortValue::Float(a), SortValue::Double(b)) => f64::from(*a).total_cmp(b),
            (SortValue::Double(a), SortValue::Float(b)) => a.total_cmp(&f64::from(*b)),
            (SortValue::Float(a), SortValue::Long(b)) => f64::from(*a).total_cmp(&(*b as f64)),
            (SortValue::Long(a), SortValue::Float(b)) => (*a as f64).total_cmp(&f64::from(*b)),
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
    /// Keyword ordinal times [`ORD_SCALE`]; values between multiples are terms the segment
    /// lacks (a translated bottom, `search_after` or missing value).
    Ord(i64),
    Score(f32),
    Doc(i64),
}

/// Keyword ordinal `x` is `Native::Ord(x * ORD_SCALE)`. A term the segment lacks, whose
/// insertion point is `i`, sits in the gap `(ORD_SCALE*(i-1), ORD_SCALE*i)`: at its middle, or
/// one below / above it when it sorts before / after a custom missing value that shares the gap
/// (the missing value takes the middle). Foreign values only ever meet the doc's own value (a
/// real ordinal or the missing value), so the three slots order every comparison exactly.
pub(crate) const ORD_SCALE: i64 = 4;

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
    /// A keyword `missing` value this segment lacks: its insertion point and bytes (see
    /// [`ORD_SCALE`]).
    missing_term: Option<(u64, Vec<u8>)>,
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
                if keyword { Native::Ord(m * ORD_SCALE) } else { self.num(m) }
            }
            SortMode::Max => {
                let m = *s.iter().max().unwrap();
                if keyword { Native::Ord(m * ORD_SCALE) } else { self.num(m) }
            }
            SortMode::Sum | SortMode::Avg | SortMode::Median if keyword => {
                let m = if self.sort.reverse { *s.iter().max().unwrap() } else { *s.iter().min().unwrap() };
                Native::Ord(m * ORD_SCALE)
            }
            SortMode::Sum | SortMode::Avg | SortMode::Median if kind == SortType::UnsignedLong => Native::I(unsigned_mode(self.sort.mode, s)),
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
            SortType::Long | SortType::UnsignedLong | SortType::Int => Native::I(v),
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
                self.term_of(o, &mut b);
                SortValue::Bytes(b)
            }
            Native::Score(s) => SortValue::Float(s),
            Native::Doc(d) => SortValue::Long(d),
        }
    }

    /// [`LeafKey::value`] into `slot`, reusing its buffers (a keyword's bytes).
    fn value_into(&self, n: Native, slot: &mut SortValue) {
        match (n, &mut *slot) {
            (Native::Ord(o), SortValue::Bytes(b)) => self.term_of(o, b),
            (Native::None, SortValue::Bytes(b)) if matches!(self.sort.missing, Missing::Value(SortValue::Bytes(_))) => {
                if let Missing::Value(SortValue::Bytes(m)) = &self.sort.missing {
                    b.clone_from(m);
                }
            }
            _ => *slot = self.value(n),
        }
    }

    /// The term of a (real) keyword native.
    fn term_of(&self, o: i64, out: &mut Vec<u8>) {
        let ord = (o / ORD_SCALE) as u64;
        match &self.src {
            Source::Sorted(v) => v.lookup_ord(ord, out),
            Source::SortedSet(v) => v.lookup_ord(ord, out),
            _ => out.clear(),
        }
    }

    /// A sort value in this segment's native space (for comparing against the queue bottom).
    fn to_native(&self, v: &SortValue) -> Native {
        match v {
            SortValue::Null => Native::None,
            SortValue::Long(x) => match self.sort.kind {
                SortType::Long | SortType::UnsignedLong | SortType::Int => Native::I(*x),
                _ => Native::F(*x as f64),
            },
            SortValue::Double(x) => match self.sort.kind {
                SortType::Long | SortType::UnsignedLong | SortType::Int => Native::F(*x),
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
                    Ok(o) => Native::Ord(o as i64 * ORD_SCALE),
                    Err(ins) => {
                        let mid = ins as i64 * ORD_SCALE - ORD_SCALE / 2;
                        match &self.missing_term {
                            Some((mi, mb)) if *mi == ins => Native::Ord(match b.as_slice().cmp(mb.as_slice()) {
                                Ordering::Less => mid - 1,
                                Ordering::Equal => mid,
                                Ordering::Greater => mid + 1,
                            }),
                            _ => Native::Ord(mid),
                        }
                    }
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
        matches!(self, SortType::Long | SortType::UnsignedLong | SortType::Int)
    }
}

/// `sum`, `avg` and `median` of sign-flipped unsigned longs in unsigned arithmetic: sums wrap
/// at 2^64 and averages round half up, as OpenSearch's unsigned comparators compute them.
/// The result is sign-flipped back. `vals` must not be empty.
fn unsigned_mode(mode: SortMode, vals: &mut [i64]) -> i64 {
    const FLIP: u64 = 1 << 63;
    let u = |v: i64| (v as u64) ^ FLIP;
    let half_up = |sum: u64, n: u64| sum / n + u64::from((sum % n) * 2 >= n);
    let out = match mode {
        SortMode::Sum => vals.iter().fold(0u64, |a, &b| a.wrapping_add(u(b))),
        SortMode::Avg => half_up(vals.iter().fold(0u64, |a, &b| a.wrapping_add(u(b))), vals.len() as u64),
        SortMode::Median => {
            // The flipped longs order as the unsigned values.
            vals.sort_unstable();
            let n = vals.len();
            if n % 2 == 1 { u(vals[n / 2]) } else { half_up(u(vals[n / 2 - 1]).wrapping_add(u(vals[n / 2])), 2) }
        }
        SortMode::Min => u(*vals.iter().min().unwrap_or(&0)),
        SortMode::Max => u(*vals.iter().max().unwrap_or(&0)),
    };
    (out ^ FLIP) as i64
}

/// The value Elasticsearch substitutes for a missing numeric sort value with `_first` /
/// `_last` (the type's extreme in the direction that puts it there); `None` for keywords.
fn missing_bound(kind: SortType, first: bool, reverse: bool) -> Option<SortValue> {
    let min = first ^ reverse;
    Some(match kind {
        SortType::Long | SortType::UnsignedLong => SortValue::Long(if min { i64::MIN } else { i64::MAX }),
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

/// An entry of the queue (global doc, score, output values, and its natives in the space of
/// segment generation `generation`).
#[derive(Clone, Debug)]
struct Entry {
    doc: u32,
    score: f32,
    values: Vec<SortValue>,
    natives: Vec<Native>,
    generation: u32,
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
    /// Counts segments: queue entries whose `generation` differs are translated into the
    /// current segment's native space before comparing.
    generation: u32,
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
            generation: 0,
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

    /// `track_scores` (or a `min_score` the caller asks scores for), or a `_score` key — which
    /// only collecting top docs reads: with `n == 0` nothing is scored.
    fn needs_scores(&self) -> bool {
        self.track_scores || (self.n > 0 && self.keys.iter().any(|k| matches!(k, SortKey::Score { .. })))
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
    /// A key is a `_script`: a script the deadline stopped leaves its doc without a value.
    scripted: bool,
    /// The first key is `_doc` ascending (a scroll's `sort=_doc`): docs arrive in sort order,
    /// so once the queue is full no later doc can enter it (Lucene's `DocComparator`).
    doc_first: bool,
}

const MIN_SKIP_INTERVAL: u32 = 32;
/// A bottom the sampling passed over is tried again after this many more docs: a queue that
/// stops changing would otherwise never prune with its final (best) bottom.
const RETRY_DOCS: u32 = 4096;
/// The most competitive terms whose postings a keyword pruner unions (Lucene's
/// `TermOrdValComparator.MAX_TERMS`).
const MAX_COMPETITIVE_TERMS: u64 = 1024;
const MAX_SKIP_INTERVAL: u32 = 8192;

/// Lucene's numeric sort optimization (`NumericComparator` with points): once the hit count
/// passed the threshold and the queue is full, the docs whose value of the first sort key can
/// still beat the queue's worst entry (and come after `search_after`) are read from the
/// field's BKD tree, when that cuts the docs to visit at least eightfold; the bulk scorer then
/// jumps over the others. Recomputed as the worst entry improves, sampled once updates get
/// frequent.
///
/// Keyword keys do the same with the field's terms (`TermOrdValComparator`'s competitive
/// iterator): the competitive ordinals of the segment's doc values, when at most
/// [`MAX_COMPETITIVE_TERMS`] remain, name the terms whose postings hold every doc that can
/// still compete (a doc's sort value is one of its values, and a keyword indexes exactly its
/// doc values).
struct Pruner {
    field: u32,
    reader: search_codec::SegmentReader,
    /// Bytes per point value: 8 (long, double) or 4 (int, float); 0: a keyword key pruned
    /// through its terms (bounds are then scaled ordinals, as [`Native::Ord`]).
    width: usize,
    reverse: bool,
    /// The bound `search_after` puts on the other side, in the doc values' integer space.
    after: Option<i64>,
    /// Whether docs without a value can compete (then nothing can be skipped).
    missing_competitive: bool,
    /// Every doc of the segment has a value: with `search_after`, the bound alone prunes
    /// before the queue is full (a doc without one would sort after the bound).
    all_valued: bool,
    /// Competitive docs, when materialized.
    set: Option<crate::query::basic::SetScorer>,
    cost: u64,
    /// The last `(bottom, strict)` the competitive docs were derived for.
    last: Option<(i64, bool)>,
    calls: u32,
    interval: u32,
    fails: u32,
    /// The sampling skipped the latest bottom (retried at doc `retry_at`, see [`RETRY_DOCS`]).
    pending: bool,
    retry_at: u32,
    /// The next update runs whatever the sampling says (a retry).
    force: bool,
    /// The segment has no value of the field: every doc sorts as missing, last.
    absent: bool,
}

/// A native value of a numeric key in its doc values' integer space.
fn native_int(kind: SortType, n: Native) -> Option<i64> {
    match (kind, n) {
        (SortType::Long | SortType::UnsignedLong | SortType::Int, Native::I(v)) => Some(v),
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
        // No value of the field in this segment (absent, or unmapped): every doc sorts as
        // missing, after any value. Once the queue is full nothing here can enter: a missing
        // doc loses to a valued bottom, and to a missing one when doc ids break the tie (Lucene's
        // comparators give such a segment an empty competitive iterator).
        if matches!(fs.missing, Missing::Last)
            && matches!(keys.first(), Some(LeafKeyKind::Field(lk, _)) if matches!(lk.src, Source::None) && lk.nested.is_none())
        {
            return Some(Pruner {
                field: fs.field,
                reader: seg.reader.clone(),
                width: 0,
                reverse: fs.reverse,
                after: None,
                missing_competitive: false,
                all_valued: false,
                set: None,
                cost: seg.reader.max_doc() as u64,
                last: None,
                calls: 0,
                interval: MIN_SKIP_INTERVAL,
                fails: 0,
                pending: false,
                retry_at: 0,
                force: false,
                absent: true,
            });
        }
        let width = match fs.kind {
            SortType::Long | SortType::UnsignedLong | SortType::Double => 8,
            SortType::Int | SortType::Float => 4,
            SortType::Keyword => return Pruner::keyword(fs, seg, keys, after),
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
            all_valued: points.doc_count() >= seg.reader.max_doc(),
            set: None,
            cost: seg.reader.max_doc() as u64,
            last: None,
            calls: 0,
            interval: MIN_SKIP_INTERVAL,
            fails: 0,
            pending: false,
            retry_at: 0,
            force: false,
            absent: false,
        })
    }

    /// A pruner for a keyword first key whose segment indexes the terms of its doc values.
    fn keyword(fs: &FieldSort, seg: &SegmentContext, keys: &[LeafKeyKind], after: Option<&[Native]>) -> Option<Pruner> {
        // Docs without a value could compete unless they sort last.
        if !matches!(fs.missing, Missing::Last) {
            return None;
        }
        let terms = seg.reader.terms(fs.field)?;
        let values = match (seg.reader.sorted(fs.field), seg.reader.sorted_set(fs.field)) {
            (Some(v), _) => v.value_count(),
            (None, Some(v)) => v.value_count(),
            _ => return None,
        };
        // The terms must be the doc values' terms (not, say, the n-grams of a wildcard field).
        if terms.stats().num_terms != values || values == 0 {
            return None;
        }
        let after = match (after, keys.first()) {
            (Some(a), Some(LeafKeyKind::Field(..))) => match a[0] {
                Native::Ord(o) => Some(o),
                _ => return None,
            },
            _ => None,
        };
        Some(Pruner {
            field: fs.field,
            reader: seg.reader.clone(),
            width: 0,
            reverse: fs.reverse,
            after,
            missing_competitive: false,
            all_valued: terms.stats().doc_count >= seg.reader.max_doc(),
            set: None,
            cost: seg.reader.max_doc() as u64,
            last: None,
            calls: 0,
            interval: MIN_SKIP_INTERVAL,
            fails: 0,
            pending: false,
            retry_at: 0,
            force: false,
            absent: false,
        })
    }

    /// The competitive docs of a keyword key below (above) the scaled-ordinal `bottom`
    /// (see [`Pruner::update`]).
    fn update_terms(&mut self, bottom: i64, from: u32, strict: bool) {
        // Ordinal `x` sits at `ORD_SCALE * x`; other bounds lie between two ordinals and
        // round toward the competitive side's inner ordinal.
        let floor = |v: i64| v.div_euclid(ORD_SCALE);
        let ceil = |v: i64| v.saturating_add(ORD_SCALE - 1).div_euclid(ORD_SCALE);
        let exact = bottom.rem_euclid(ORD_SCALE) == 0;
        let (lo, hi) = if self.reverse {
            let lo = if strict && exact { bottom / ORD_SCALE + 1 } else { ceil(bottom) };
            let hi = self.after.map_or(i64::MAX, floor);
            (lo, hi)
        } else {
            let hi = if strict && exact { bottom / ORD_SCALE - 1 } else { floor(bottom) };
            let lo = self.after.map_or(0, ceil);
            (lo, hi)
        };
        let max_doc = self.reader.max_doc();
        let (sorted, sorted_set) = (self.reader.sorted(self.field), self.reader.sorted_set(self.field));
        let count = sorted.map(|v| v.value_count()).or(sorted_set.map(|v| v.value_count())).unwrap_or(0) as i64;
        let (lo, hi) = (lo.max(0), hi.min(count - 1));
        if lo > hi {
            self.cost = 0;
            self.set = Some(crate::query::basic::SetScorer::new(Arc::new(crate::util::bitset::DocIdSetBuilder::new(max_doc).build()), 0.0));
            return;
        }
        if (hi - lo + 1) as u64 > MAX_COMPETITIVE_TERMS {
            self.update_interval(false);
            return;
        }
        let Some(terms) = self.reader.terms(self.field) else { return };
        let lookup = |ord: i64, out: &mut Vec<u8>| match (sorted, sorted_set) {
            (Some(v), _) => v.lookup_ord(ord as u64, out),
            (None, Some(v)) => v.lookup_ord(ord as u64, out),
            _ => {}
        };
        let (mut lo_term, mut hi_term) = (Vec::new(), Vec::new());
        lookup(lo, &mut lo_term);
        lookup(hi, &mut hi_term);
        // Estimate first: a union that keeps most docs is not worth building.
        let mut freq = 0u64;
        let mut it = terms.range(std::ops::Bound::Included(&lo_term), std::ops::Bound::Included(&hi_term));
        while it.next().is_some() {
            freq += u64::from(it.doc_freq());
        }
        if freq >= self.cost >> 3 {
            self.update_interval(false);
            return;
        }
        let mut b = crate::util::bitset::DocIdSetBuilder::new(max_doc);
        b.grow(freq as usize);
        let mut it = terms.range(std::ops::Bound::Included(&lo_term), std::ops::Bound::Included(&hi_term));
        let mut cursor = search_codec::postings::PostingsCursor::empty();
        while it.next().is_some() {
            it.postings_reset(search_codec::postings::PostingsFlags::DOCS, &mut cursor);
            let mut d = if from > 0 { cursor.advance(from) } else { cursor.next() };
            while d < max_doc {
                b.add(d);
                d = cursor.next();
            }
        }
        let set = b.build();
        self.cost = set.cost();
        self.set = Some(crate::query::basic::SetScorer::new(Arc::new(set), 0.0));
        self.update_interval(true);
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
    /// `strict`: a doc equal to the bottom can't compete (the bottom is the only key and every
    /// doc still to come has a higher doc id, so ties lose), Lucene's
    /// `Pruning.GREATER_THAN_OR_EQUAL_TO`; otherwise equal values stay competitive.
    fn update(&mut self, bottom: i64, from: u32, strict: bool) {
        if self.missing_competitive || self.last == Some((bottom, strict)) {
            return;
        }
        self.calls += 1;
        if !std::mem::take(&mut self.force) && self.calls > 256 && (self.calls & (self.interval - 1)) != self.interval - 1 {
            self.pending = true;
            return;
        }
        self.pending = false;
        self.last = Some((bottom, strict));
        if self.width == 0 {
            return self.update_terms(bottom, from, strict);
        }
        // Competitive values: at least the bottom (descending) or at most it (ascending),
        // strictly beyond it when ties lose. An empty range (`lo > hi`) leaves no doc.
        // (`None`: no value lies strictly beyond the bottom.)
        let bounds = match (self.reverse, strict) {
            (true, false) => Some((bottom, self.after.unwrap_or(i64::MAX))),
            (true, true) => bottom.checked_add(1).map(|lo| (lo, self.after.unwrap_or(i64::MAX))),
            (false, false) => Some((self.after.unwrap_or(i64::MIN), bottom)),
            (false, true) => bottom.checked_sub(1).map(|hi| (self.after.unwrap_or(i64::MIN), hi)),
        };
        let (lo, hi) = bounds.unwrap_or((i64::MAX, i64::MIN));
        if lo > hi {
            // Nothing can compete any more.
            self.cost = 0;
            self.set = Some(crate::query::basic::SetScorer::new(Arc::new(crate::util::bitset::DocIdSetBuilder::new(self.reader.max_doc()).build()), 0.0));
            return;
        }
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
            LeafKeyKind::Doc(_) => Native::Doc(i64::from(base + doc)),
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
            // An i64, not a doc id: `_shard_doc`'s `search_after` bound is -1 on the shards after the
            // one it names (every doc comes after it) and `i32::MAX` on those before.
            LeafKeyKind::Doc(_) => Native::Doc(match v {
                SortValue::Long(d) => *d,
                _ => i64::from(doc),
            }),
            LeafKeyKind::Field(lk, _) => lk.to_native(v),
        })
        .collect()
}

/// Brings `e`'s natives into the space of segment generation `generation` (a no-op when they
/// already are). One terms-dictionary lookup per keyword key; no allocation.
fn translate(keys: &[LeafKeyKind], generation: u32, e: &mut Entry) {
    if e.generation == generation {
        return;
    }
    e.natives.clear();
    for (k, v) in keys.iter().zip(&e.values) {
        e.natives.push(match k {
            LeafKeyKind::Score(_) => Native::Score(match v {
                SortValue::Float(f) => *f,
                SortValue::Double(d) => *d as f32,
                _ => e.score,
            }),
            LeafKeyKind::Doc(_) => Native::Doc(i64::from(e.doc)),
            LeafKeyKind::Field(lk, _) => lk.to_native(v),
        });
    }
    e.generation = generation;
}

/// [`output_values`] into `out`, reusing its buffers.
fn fill_values(keys: &[LeafKeyKind], natives: &[Native], score: f32, gdoc: u32, out: &mut Vec<SortValue>) {
    out.resize(keys.len(), SortValue::Null);
    for ((k, n), slot) in keys.iter().zip(natives).zip(out.iter_mut()) {
        match k {
            LeafKeyKind::Score(_) => *slot = SortValue::Float(score),
            LeafKeyKind::Doc(_) => *slot = SortValue::Long(gdoc as i64),
            LeafKeyKind::Field(lk, _) => lk.value_into(*n, slot),
        }
    }
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
        let Some(p) = self.pruner.as_mut() else { return };
        let Some(b) = self.bottom.as_ref() else {
            // Queue not full yet: `search_after` alone bounds the competitive values
            // (Lucene's `NumericComparator` with a top value).
            if p.after.is_some() && p.all_valued {
                p.update(if p.reverse { i64::MIN } else { i64::MAX }, from, false);
            }
            return;
        };
        let SortKey::Field(fs) = &self.c.keys[0] else { return };
        // Ties break on doc ids: with one key, a doc equal to a bottom from an earlier doc loses.
        let strict = self.c.keys.len() == 1 && self.c.queue.last().is_some_and(|w| w.doc < self.base.saturating_add(from));
        if p.absent {
            // Every doc here sorts as the missing value: competitive only if that beats the
            // bottom, or ties it while ties don't lose.
            let o = match &self.keys[0] {
                LeafKeyKind::Field(lk, m) => cmp_native(Native::None, b[0], &lk.sort, *m),
                _ => return,
            };
            if p.set.is_none() && (o == Ordering::Greater || (o == Ordering::Equal && strict)) {
                p.cost = 0;
                p.set = Some(crate::query::basic::SetScorer::new(Arc::new(crate::util::bitset::DocIdSetBuilder::new(p.reader.max_doc()).build()), 0.0));
            }
            return;
        }
        let v = match (fs.kind, b[0]) {
            (SortType::Keyword, Native::Ord(o)) => Some(o),
            (kind, n) => native_int(kind, n),
        };
        if let Some(v) = v {
            p.update(v, from, strict);
        }
    }

    /// With `_doc` ascending first, whether `gdoc` sorts after the full queue's worst entry
    /// (then it, and every later doc, cannot enter whatever the other keys say).
    #[inline]
    fn after_bottom_doc(&self, gdoc: u32) -> bool {
        self.doc_first && matches!(self.bottom.as_deref(), Some([Native::Doc(b), ..]) if *b < i64::from(gdoc))
    }

    fn refresh_bottom(&mut self) {
        if self.c.queue.len() >= self.c.n && self.c.n > 0 {
            let generation = self.c.generation;
            let w = self.c.queue.last_mut().unwrap();
            translate(&self.keys, generation, w);
            match &mut self.bottom {
                Some(b) => b.clone_from(&w.natives),
                None => self.bottom = Some(w.natives.clone()),
            }
        }
    }
}

impl LeafCollector for TopFieldLeaf<'_> {
    /// `_doc` ascending first with a full queue: the block can't change the hits; with an
    /// exact total (a scroll's first page) it only counts, past the threshold it ends the
    /// segment. Anything else collects per doc.
    fn collect_block(&mut self, docs: &[u32], score: f32) -> Result<()> {
        if let Some(&first) = docs.first()
            && self.after_bottom_doc(self.base + first)
        {
            if self.c.passed {
                return Err(Error::CollectionTerminated);
            }
            if self.c.threshold == u64::MAX {
                self.c.total += docs.len() as u64;
                self.collected += docs.len() as u64;
                if self.scores && score > self.c.max_score {
                    self.c.max_score = score;
                }
                return Ok(());
            }
        }
        let mut sv = crate::search::ScoreValue(score);
        for &d in docs {
            self.collect(d, &mut sv)?;
        }
        Ok(())
    }

    fn skips(&self) -> bool {
        self.pruner.is_some()
    }

    #[inline]
    fn competitive_from(&mut self, doc: u32) -> u32 {
        let retry = match self.pruner.as_mut() {
            Some(p) if p.pending && doc >= p.retry_at => {
                p.retry_at = doc.saturating_add(RETRY_DOCS);
                p.force = true;
                true
            }
            Some(_) => false,
            None => return doc,
        };
        if retry {
            self.prune(doc);
            if let Some(p) = self.pruner.as_mut() {
                p.force = false;
            }
        }
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
        if just_passed {
            // The hit count just passed the threshold: pruning may start, whatever this doc
            // turns out to be (one before `search_after` must not keep it from starting:
            // Lucene's `setHitsThresholdReached` runs on the count alone).
            self.prune(doc + 1);
        }
        if self.doc_first {
            let gdoc = self.base + doc;
            if self.after_bottom_doc(gdoc) {
                self.collected += 1;
                return if self.c.passed { Err(Error::CollectionTerminated) } else { Ok(()) };
            }
            // Before `search_after` on the first key (a tie goes to the general path, which
            // compares the other keys).
            if matches!(self.after.as_deref(), Some([Native::Doc(a), ..]) if i64::from(gdoc) < *a) {
                return Ok(());
            }
        }
        // One numeric key: a non-competitive doc costs one value read and one comparison.
        // A doc sorting before `search_after` is out (a tie with it is left to the general
        // path below, which breaks it on doc ids); one worse than the bottom cannot enter
        // whatever the bound.
        if let Some((reverse, missing)) = self.single_numeric
            && (self.bottom.is_some() || self.after.is_some())
        {
            let LeafKeyKind::Field(lk, _) = &mut self.keys[0] else { unreachable!() };
            let n = lk.native(doc);
            let n = if n == Native::None { missing.unwrap_or(Native::None) } else { n };
            // `n` sorts strictly before (`Less`) or after (`Greater`) `y`; `None` when the
            // natives don't compare here.
            let order = |y: Native| match (n, y) {
                (Native::I(x), Native::I(y)) => Some(if reverse { y.cmp(&x) } else { x.cmp(&y) }),
                (Native::F(x), Native::F(y)) => Some(if reverse { y.total_cmp(&x) } else { x.total_cmp(&y) }),
                _ => None,
            };
            if let Some(a) = &self.after
                && order(a[0]) == Some(Ordering::Less)
            {
                return Ok(());
            }
            if let Some(b) = self.bottom.as_ref() {
                if order(b[0]).is_some_and(|o| o != Ordering::Less) {
                    self.collected += 1;
                    if self.c.early_terminate && self.c.passed {
                        return Err(Error::CollectionTerminated);
                    }
                    return Ok(());
                }
                if self.after.is_none() {
                    self.natives.clear();
                    self.natives.push(n);
                    return self.insert(doc, score);
                }
            }
        }
        self.compute(doc, score);
        // A sort script the search's deadline (or cancellation) stopped computed nothing for
        // this doc: it must not be collected with a missing value. Collection stops here;
        // what was collected before is the partial result.
        if self.scripted
            && let Some(e) = crate::search::take_interrupted()
        {
            // The script's own failure, deferred with it, is this interruption.
            crate::search::take_deferred_error();
            return Err(e);
        }
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
    /// Callers only insert into a full queue a doc that beats its worst entry, so that entry
    /// is evicted first and its buffers reused: no allocation once the queue is full.
    fn insert(&mut self, doc: u32, score: f32) -> Result<()> {
        let gdoc = self.base + doc;
        let generation = self.c.generation;
        let mut e = if self.c.n > 0 && self.c.queue.len() >= self.c.n {
            self.c.queue.pop().unwrap()
        } else {
            Entry { doc: 0, score: 0.0, values: Vec::with_capacity(self.keys.len()), natives: Vec::with_capacity(self.keys.len()), generation }
        };
        fill_values(&self.keys, &self.natives, score, gdoc, &mut e.values);
        e.natives.clone_from(&self.natives);
        (e.doc, e.score, e.generation) = (gdoc, score, generation);
        // `_doc` first: a doc sorts after every queued one (docs arrive in increasing order).
        if self.doc_first && self.c.queue.last().is_none_or(|l| l.doc < gdoc) {
            self.c.queue.push(e);
            return self.inserted(doc);
        }
        // Insert in order (binary search; entries of earlier segments are translated into
        // this one's space once, when first probed).
        let pos = {
            let mut lo = 0;
            let mut hi = self.c.queue.len();
            while lo < hi {
                let mid = (lo + hi) / 2;
                let q = &mut self.c.queue[mid];
                translate(&self.keys, generation, q);
                let o = cmp_natives(&self.keys, &self.natives, &q.natives).then(gdoc.cmp(&q.doc));
                if o == Ordering::Less {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            lo
        };
        self.c.queue.insert(pos, e);
        self.inserted(doc)
    }

    /// Trims the queue to `n` after an insert of `doc` and updates the bottom and pruning.
    fn inserted(&mut self, doc: u32) -> Result<()> {
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
        // Natives of queued entries belong to the previous segment.
        self.generation = self.generation.wrapping_add(1);
        let single_numeric = match keys.as_slice() {
            [LeafKeyKind::Field(lk, missing)] if lk.fast.is_some() => Some((lk.sort.reverse, *missing)),
            _ => None,
        };
        let scripted = keys.iter().any(|k| matches!(k, LeafKeyKind::Field(lk, _) if matches!(lk.src, Source::Script(_))));
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
            scripted,
            doc_first: false,
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
        leaf.doc_first = matches!(leaf.keys.first(), Some(LeafKeyKind::Doc(false))) && leaf.c.n > 0;
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
                let mut lk = LeafKey { sort: fs.clone(), src, nested, scratch: Vec::new(), fast, missing_term: None };
                // A keyword missing value the segment lacks takes the middle of its gap; other
                // foreign values there order around it by their bytes (see `ORD_SCALE`).
                if let Missing::Value(SortValue::Bytes(b)) = &fs.missing
                    && fs.kind == SortType::Keyword
                {
                    let r = match &lk.src {
                        Source::Sorted(v) => v.lookup_term(b),
                        Source::SortedSet(v) => v.lookup_term(b),
                        _ => Err(0),
                    };
                    if let Err(ins) = r {
                        lk.missing_term = Some((ins, b.clone()));
                    }
                }
                let missing = match &fs.missing {
                    Missing::Value(v) => Some(lk.to_native(v)),
                    m => missing_bound(fs.kind, matches!(m, Missing::First), fs.reverse).map(|v| lk.to_native(&v)),
                };
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
    let lk = LeafKey { sort, src, nested: nested_leaf, scratch: Vec::new(), fast: None, missing_term: None };
    let missing = Some(lk.to_native(&SortValue::Double(f64::INFINITY)));
    Ok(LeafKeyKind::Field(Box::new(lk), missing))
}

impl PartialOrd for SortValue {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp_same(o))
    }
}

#[cfg(test)]
mod unsigned_tests {
    use super::*;

    fn enc(v: u64) -> i64 {
        (v ^ (1 << 63)) as i64
    }

    fn run(mode: SortMode, vals: &[u64]) -> u64 {
        let mut v: Vec<i64> = vals.iter().map(|&x| enc(x)).collect();
        (unsigned_mode(mode, &mut v) as u64) ^ (1 << 63)
    }

    // Expected values from OpenSearch 3.9.0's 260_sort_unsigned_long YAML test.
    #[test]
    fn unsigned_modes_match_opensearch() {
        const B: u64 = 13835058055282163712;
        assert_eq!(run(SortMode::Median, &[B + 10, 5, 6, 7]), 7);
        assert_eq!(run(SortMode::Median, &[B + 1, B + 2]), 4611686018427387906);
        assert_eq!(run(SortMode::Median, &[B, 1]), 6917529027641081857);
        assert_eq!(run(SortMode::Median, &[B + 8, B + 9, 3, 4]), 6917529027641081862);
        assert_eq!(run(SortMode::Median, &[B + 3, B + 4, 2]), B + 3);
        assert_eq!(run(SortMode::Sum, &[B + 10, 5, 6, 7]), 13835058055282163740);
        assert_eq!(run(SortMode::Sum, &[B + 8, B + 9, 3, 4]), 9223372036854775832);
        assert_eq!(run(SortMode::Sum, &[B + 5, B + 6, B + 7]), 4611686018427387922);
        assert_eq!(run(SortMode::Avg, &[B, 1]), 6917529027641081857);
        assert_eq!(run(SortMode::Avg, &[B + 3, B + 4, 2]), 3074457345618258606);
        assert_eq!(run(SortMode::Avg, &[B + 5, B + 6, B + 7]), 1537228672809129307);
        assert_eq!(run(SortMode::Min, &[B, 1]), 1);
        assert_eq!(run(SortMode::Max, &[B, 1]), B);
        assert_eq!(run(SortMode::Sum, &[u64::MAX]), u64::MAX);
    }
}
