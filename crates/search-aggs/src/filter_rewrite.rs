//! OpenSearch's filter rewrite optimization as its profile reports it
//! (`FilterRewriteOptimizationContext`, `optimized_segments` / `unoptimized_segments` /
//! `leaf_visited` / `inner_visited` in an aggregation's `debug`).
//!
//! OpenSearch counts the buckets of top-level `date_histogram`, `auto_date_histogram`,
//! single-source date `composite` and `range` aggregations straight from a segment's BKD tree
//! when it can: the buckets become a sorted list of `[lower, upper)` ranges and one walk over
//! the point tree counts each range (`PointTreeTraversal.multiRangesTraverse`). roll collects
//! those aggregations its own way (the results are the same), so this module only answers the
//! question the profile asks — would OpenSearch have taken that path for this segment, and
//! how many tree nodes would its walk have visited — and it only runs for profiled requests of
//! the opensearch dialect.
//!
//! [`FilterRewrite::counting`] is the same rewrite for real, in every dialect: a top-level date
//! histogram without sub-aggregations, under a `match_all`, an `exists` on its field or a range
//! on its field, counts each bucket's range from the points tree on every segment it can
//! ([`FilterRewrite::count_segment`]: no deletions, one point per document) and needs none of
//! that segment's documents. Elasticsearch gets there by rewriting the histogram into a `range`
//! aggregation run filter by filter (`DateHistogramAggregator.FromDateRange`). A top-level
//! `auto_date_histogram` on a `date` field uses the same counts when Elasticsearch's choice of
//! rounding can be told from them (`bucket/auto_histogram.rs`, `count_from_points`): every
//! segment counted, its answer is the one `FromSingle` collects.
//!
//! The walk is replayed exactly: Lucene's one-dimensional BKD writer cuts the sorted points of
//! a segment into leaves of 512 in order and splits the leaves with
//! `BKDWriter.getNumLeftLeafNodes`; an inner node's cell is bounded by the first value of its
//! right subtree. Over that tree the replay applies OpenSearch's visitor (`compare`,
//! `visitPoints`, the range iteration and its early termination) and counts what it counts.

use search_codec::points::{PointVisitor, Relation, encode};
use search_index::query::basic::{ConstantScoreQuery, FieldExistsQuery};
use search_index::query::range::{IndexOrDocValuesQuery, PointRangeQuery};
use search_index::search::{QueryRef, ScoreMode, Searcher, SegmentContext, Weight};

use crate::collect::BindCtx;
use crate::rounding::{Rounding, RoundingKind};
use crate::values::BoundVs;

/// Lucene's `BKDConfig.DEFAULT_MAX_POINTS_IN_LEAF_NODE`.
const LEAF_POINTS: usize = 512;
/// `search.max_aggregation_rewrite_filters`.
const MAX_REWRITE_FILTERS: usize = 3000;
/// `search.aggregation_rewrite_filters.segment_threshold.docs_per_bucket`.
const SEGMENT_THRESHOLD: u32 = 1000;

/// How a point's bytes compare: every encoding is turned into a sortable `i64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Long,
    Int,
    Double,
    Float,
    HalfFloat,
}

impl Encoding {
    /// The encoding of a numeric field type, if OpenSearch's `NumericPointEncoder` covers it.
    pub fn of(type_name: &str) -> Option<Encoding> {
        Some(match type_name {
            "long" | "date" => Encoding::Long,
            "integer" | "short" | "byte" => Encoding::Int,
            "double" => Encoding::Double,
            "float" => Encoding::Float,
            "half_float" => Encoding::HalfFloat,
            _ => return None,
        })
    }

    fn decode(self, b: &[u8]) -> i64 {
        use search_codec::points::encode;
        match self {
            Encoding::Long | Encoding::Double => encode::i64_from(b),
            Encoding::Int | Encoding::Float => encode::i32_from(b) as i64,
            Encoding::HalfFloat => (u16::from_be_bytes([b[0], b[1]]) ^ 0x8000) as i16 as i64,
        }
    }

    /// `NumericPointEncoder.encodePoint(double)` as a sortable key (Java's saturating casts).
    pub fn encode(self, v: f64) -> i64 {
        use search_codec::points::encode;
        match self {
            Encoding::Long => v as i64,
            Encoding::Int => v as i32 as i64,
            Encoding::Double => encode::f64_sortable(v),
            Encoding::Float => encode::f32_sortable(v as f32) as i64,
            Encoding::HalfFloat => encode::f16_sortable(v as f32) as i64,
        }
    }
}

/// Where the ranges come from.
enum Source {
    /// Fixed ranges (`range`), built once.
    Fixed(Vec<(i64, i64)>),
    /// Date buckets from bounds (`DateHistogramAggregatorBridge.buildRanges`).
    Date(DateBuckets),
}

/// A date aggregation's rounding, for building its ranges from bounds.
pub struct DateBuckets {
    pub rounding: Rounding,
    /// `hard_bounds` (max exclusive).
    pub hard: (Option<i64>, Option<i64>),
    /// A `composite` `after` key.
    pub after: Option<i64>,
    /// `date_nanos` points (rounded in millis).
    pub nanos: bool,
    /// `auto_date_histogram`: its roundings with the longest duration each may cover
    /// (`innerIntervals` last × rough duration), and its target bucket count; the bounds pick
    /// the rounding (`getRounding(low, high)`).
    pub auto: Vec<(Rounding, i64)>,
    pub target: i64,
}

impl DateBuckets {
    /// `Rounding.getInterval`: the step that always reaches the next bucket.
    fn interval(r: &Rounding) -> i64 {
        match r.kind {
            RoundingKind::Unit(u) => u.extra_local_offset_lookup(),
            RoundingKind::Interval(i) => i,
        }
    }

    /// The rounding for bounds `[low, high]`.
    fn rounding_for(&self, low: i64, high: i64) -> &Rounding {
        if self.auto.is_empty() {
            return &self.rounding;
        }
        let best = (high - low) / self.target.max(1);
        let i = self.auto[..self.auto.len() - 1].iter().position(|(_, max)| best <= *max).unwrap_or(self.auto.len() - 1);
        &self.auto[i].0
    }

    /// `buildRanges(bounds)`: `None` when OpenSearch gives up (bounds outside the hard
    /// bounds, too many buckets).
    fn ranges(&self, mut low: i64, mut high: i64) -> Option<Vec<(i64, i64)>> {
        // `low` and `high` are in the field's units (nanoseconds for `date_nanos`); hard bounds
        // and rounding are in milliseconds.
        let ms = |v: i64| if self.nanos { v.div_euclid(1_000_000) } else { v };
        let ns = |v: i64| if self.nanos { v.saturating_mul(1_000_000) } else { v };
        if let Some(min) = self.hard.0.map(ns)
            && min > low
        {
            low = min;
        }
        if let Some(max) = self.hard.1.map(ns)
            && max - 1 < high
        {
            high = max - 1;
        }
        if low > high {
            return None;
        }
        let r = self.rounding_for(ms(low), ms(high));
        let interval = Self::interval(r);
        if let Some(a) = self.after {
            low = a + interval;
        }
        let mut keys = Vec::new();
        let mut key = r.round(ms(low));
        let mut prev = key;
        while key <= ms(high) {
            keys.push(key);
            if keys.len() > MAX_REWRITE_FILTERS {
                return None;
            }
            key = r.round(key.saturating_add(interval));
            if key == prev {
                break;
            }
            prev = key;
        }
        let n = keys.len();
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let lower = if i == 0 { low } else { ns(keys[i]) };
            let upper = if i + 1 == n { high + 1 } else { ns(keys[i + 1]) };
            out.push((lower, upper));
        }
        (!out.is_empty()).then_some(out)
    }
}

/// The optimization's state for one aggregator on one shard.
pub struct FilterRewrite {
    field: u32,
    enc: Encoding,
    source: Source,
    /// Ranges built from the shard (`preparedAtShardLevel`); `None` = per segment, when the
    /// segment matches all its documents.
    shard_ranges: Option<Option<Vec<(i64, i64)>>>,
    has_sub: bool,
    /// `getSize()`: buckets a walk may fill before it stops (composite's `size`).
    size: usize,
    /// `range` only tries segments the query matches entirely.
    match_all_only: bool,
    matches_all: bool,
    weight: Option<Box<dyn Weight>>,
    /// [`FilterRewrite::counting`]: the `_doc_count` field (a segment with values is collected).
    doc_count_field: Option<u32>,
    /// [`FilterRewrite::counting`]: the query's bounds on the field clipped to the shard's
    /// points (`None` inside: no value matches).
    bounds: Option<Option<(i64, i64)>>,
    pub segments: u32,
    pub optimized: u32,
    pub leaf_visited: u32,
    pub inner_visited: u32,
}

impl FilterRewrite {
    /// Whether the request is one this module reports on: profiled, opensearch dialect,
    /// a top-level aggregation over an indexed field without script or `missing`.
    fn eligible(ctx: &BindCtx<'_>, vs: &BoundVs) -> bool {
        crate::dialect::opensearch()
            && ctx.profiler.is_some()
            && ctx.parents.is_empty()
            && vs.script.is_none()
            && vs.missing.is_none()
            && vs.field.as_ref().is_some_and(|f| f.points)
    }

    /// A date aggregation (`date_histogram`, `auto_date_histogram`, `composite`): UTC
    /// roundings without offset over a `date` / `date_nanos` field.
    pub fn date(ctx: &BindCtx<'_>, vs: &BoundVs, buckets: DateBuckets, has_sub: bool, size: usize) -> Option<FilterRewrite> {
        if !Self::eligible(ctx, vs) || buckets.rounding.offset != 0 || !is_utc(&buckets.rounding) {
            return None;
        }
        let f = vs.field.as_ref()?;
        if f.type_name != "date" && f.type_name != "date_nanos" {
            return None;
        }
        let field = f.number;
        let mut fr = FilterRewrite::new(ctx, field, Encoding::Long, Source::Date(buckets), has_sub, size, false);
        fr.weight = query_weight(ctx);
        // `Helper.getDateHistoAggBounds`: the shard's bounds under a `match_all`, an `exists`
        // on the field, or a range on the field (clipped to it).
        let bounds = shard_bounds(ctx.searcher, field);
        let shard = if ctx.matches_all {
            Some(bounds)
        } else {
            match ctx.query.as_ref().map(unwrap) {
                Some(q) if q.as_any().downcast_ref::<FieldExistsQuery>().is_some_and(|e| e.field == field) => Some(bounds),
                Some(q) => match q.as_any().downcast_ref::<PointRangeQuery>() {
                    Some(p) if p.field == field && p.lower.len() == 8 => {
                        let lo = search_codec::points::encode::i64_from(&p.lower);
                        let hi = search_codec::points::encode::i64_from(&p.upper);
                        Some(bounds.and_then(|(min, max)| {
                            let (l, u) = (lo.max(min), hi.min(max));
                            (l <= u).then_some((l, u))
                        }))
                    }
                    _ => None,
                },
                None => None,
            }
        };
        fr.shard_ranges = shard.map(|b| match (&fr.source, b) {
            (Source::Date(d), Some((l, h))) => d.ranges(l, h),
            _ => None,
        });
        Some(fr)
    }

    /// [`FilterRewrite::date`] for counting buckets (see the module docs): top-level, one
    /// owning bucket, no sub-aggregations, no `terminate_after`, no script or `missing`, not a
    /// `time_series` request, any dialect. A segment with `_doc_count` values is collected doc
    /// by doc, and so is a profiled request: its profile reports the collection Elasticsearch
    /// does without the rewrite (roll doesn't render `FromDateRange`'s delegate debug).
    pub fn counting(ctx: &BindCtx<'_>, vs: &BoundVs, buckets: DateBuckets, has_sub: bool) -> Option<FilterRewrite> {
        let ok = !has_sub
            && ctx.profiler.is_none()
            && !ctx.terminate_after
            && ctx.parents.is_empty()
            && ctx.cardinality.is_one()
            && !ctx.time_series_request
            && vs.script.is_none()
            && vs.missing.is_none()
            && vs.field.as_ref().is_some_and(|f| f.points);
        if !ok || buckets.rounding.offset != 0 || !is_utc(&buckets.rounding) {
            return None;
        }
        let f = vs.field.as_ref()?;
        if f.type_name != "date" && f.type_name != "date_nanos" {
            return None;
        }
        let field = f.number;
        let mut fr = FilterRewrite::new(ctx, field, Encoding::Long, Source::Date(buckets), false, usize::MAX, false);
        fr.doc_count_field = ctx.doc_count_field;
        let bounds = shard_bounds(ctx.searcher, field);
        let shard = if ctx.matches_all {
            Some(bounds)
        } else {
            match ctx.query.as_ref().map(unwrap) {
                Some(q) if q.as_any().downcast_ref::<FieldExistsQuery>().is_some_and(|e| e.field == field) => Some(bounds),
                Some(q) => match q.as_any().downcast_ref::<PointRangeQuery>() {
                    Some(p) if p.field == field && p.lower.len() == 8 => {
                        let lo = search_codec::points::encode::i64_from(&p.lower);
                        let hi = search_codec::points::encode::i64_from(&p.upper);
                        Some(bounds.and_then(|(min, max)| {
                            let (l, u) = (lo.max(min), hi.min(max));
                            (l <= u).then_some((l, u))
                        }))
                    }
                    _ => None,
                },
                None => None,
            }
        };
        // Only a query whose matches are exactly the buckets' ranges (clipped to it) is counted.
        let shard = shard?;
        fr.bounds = Some(shard);
        fr.shard_ranges = Some(match (&fr.source, shard) {
            (Source::Date(d), Some((l, h))) => d.ranges(l, h),
            // No values in range anywhere: nothing to count, every segment is done.
            (_, None) => Some(Vec::new()),
            _ => None,
        });
        Some(fr)
    }

    /// The documents of each bucket range on `seg`, counted from the points tree (`None`: the
    /// segment must be collected doc by doc). Ranges are `[lower, upper)` in the field's units.
    pub fn count_segment(&self, seg: &SegmentContext) -> Option<Vec<((i64, i64), u64)>> {
        let ranges = self.shard_ranges.as_ref()?.as_ref()?;
        if !self.countable(seg) {
            return None;
        }
        let ranges: Vec<(i64, i64)> = ranges.iter().copied().filter(|&(lo, hi)| hi > lo).collect();
        let counts = self.count_ranges(seg, &ranges);
        Some(ranges.into_iter().zip(counts).filter(|(_, n)| *n > 0).collect())
    }

    /// [`FilterRewrite::counting`]'s query bounds on the field, clipped to the shard's points
    /// (`Some(None)`: no value matches).
    pub fn bounds(&self) -> Option<Option<(i64, i64)>> {
        self.bounds
    }

    /// Whether `seg` can be counted from its points: no deletions, no `_doc_count` values, one
    /// 8-byte point per document (or no points at all).
    pub fn countable(&self, seg: &SegmentContext) -> bool {
        if seg.live.is_some() || self.doc_count_field.is_some_and(|f| seg.reader.doc_values(f).is_some()) {
            return false;
        }
        seg.reader.points(self.field).is_none_or(|p| p.doc_count() as u64 == p.size() && p.spec().packed_len() == 8)
    }

    /// The points of a [countable](Self::countable) segment in each of `ranges` (`[lower,
    /// upper)`, sorted, disjoint), in one walk of its tree: cells inside one range count
    /// whole, crossing leaves are bucketed value by value.
    pub fn count_ranges(&self, seg: &SegmentContext, ranges: &[(i64, i64)]) -> Vec<u64> {
        let Some(points) = seg.reader.points(self.field) else { return vec![0; ranges.len()] };
        let mut v = MultiCount { counts: vec![0; ranges.len()], ranges, last_inside: None };
        let mut inside = vec![0u64; ranges.len()];
        points.count_cells(&mut v, &mut |min, n| {
            if let Some(i) = find_range(ranges, encode::i64_from(min)) {
                inside[i] += n;
            }
        });
        v.counts.iter().zip(&inside).map(|(a, b)| a + b).collect()
    }

    /// The smallest (`max` false) or largest value of a [countable](Self::countable)
    /// segment's points in `[lo, hi]`: a binary search over values, each step a range count
    /// from the tree (inner cells whole, at most two leaves read).
    pub fn extreme(&self, seg: &SegmentContext, lo: i64, hi: i64, max: bool) -> Option<i64> {
        seg.reader.points(self.field)?;
        // Whether a point lies in `[a, b]` (the ranges are half-open: `i64::MAX` itself is
        // never a date's value).
        let any = |a: i64, b: i64| a <= b && self.count_ranges(seg, &[(a, b.saturating_add(1))])[0] > 0;
        if !any(lo, hi) {
            return None;
        }
        // The answer is in `[a, b]`.
        let (mut a, mut b) = (lo, hi);
        while a < b {
            let half = ((b as i128 - a as i128) / 2) as i64;
            if max {
                let m = b - half;
                if any(m, b) { a = m } else { b = m - 1 }
            } else {
                let m = a + half;
                if any(a, m) { b = m } else { a = m + 1 }
            }
        }
        Some(a)
    }

    /// A `range` aggregation: non-overlapping ranges (sorted by `from`, then `to`) over a
    /// numeric field.
    pub fn range(ctx: &BindCtx<'_>, vs: &BoundVs, ranges: &[(f64, f64)], has_sub: bool) -> Option<FilterRewrite> {
        if !Self::eligible(ctx, vs) || ranges.is_empty() {
            return None;
        }
        let f = vs.field.as_ref()?;
        // Dates aggregate through `date_range`, which this replay doesn't cover.
        let enc = Encoding::of(&f.type_name).filter(|_| f.type_name != "date")?;
        let mut prev_to = ranges[0].1;
        for r in &ranges[1..] {
            if prev_to > r.0 {
                return None;
            }
            prev_to = r.1;
        }
        let fixed: Vec<(i64, i64)> = ranges.iter().map(|&(from, to)| (enc.encode(from), enc.encode(to))).collect();
        let mut fr = FilterRewrite::new(ctx, f.number, enc, Source::Fixed(fixed.clone()), has_sub, usize::MAX, true);
        fr.weight = query_weight(ctx);
        fr.shard_ranges = Some(Some(fixed));
        Some(fr)
    }

    fn new(ctx: &BindCtx<'_>, field: u32, enc: Encoding, source: Source, has_sub: bool, size: usize, match_all_only: bool) -> FilterRewrite {
        FilterRewrite {
            field,
            enc,
            source,
            shard_ranges: None,
            has_sub,
            size,
            match_all_only,
            matches_all: ctx.matches_all,
            weight: None,
            doc_count_field: None,
            bounds: None,
            segments: 0,
            optimized: 0,
            leaf_visited: 0,
            inner_visited: 0,
        }
    }

    /// `segmentMatchAll`: the query's count on the segment is its live doc count.
    fn segment_match_all(&self, seg: &SegmentContext) -> bool {
        if self.matches_all {
            return true;
        }
        match &self.weight {
            Some(w) => w.count(seg).ok().flatten() == Some(seg.num_docs),
            None => false,
        }
    }

    /// `tryOptimize` for a segment: whether OpenSearch would count it from the tree (its
    /// walk's visits are added to the totals).
    pub fn try_segment(&mut self, seg: &SegmentContext) -> bool {
        let match_all = self.segment_match_all(seg);
        if self.match_all_only && !match_all {
            return false;
        }
        self.segments += 1;
        if seg.live.is_some() {
            return false;
        }
        let Some(points) = seg.reader.points(self.field) else { return false };
        if points.doc_count() as u64 != points.size() {
            return false;
        }
        let ranges = match &self.shard_ranges {
            Some(r) => r.clone(),
            None if match_all => {
                let min = self.enc.decode(points.min_packed_value());
                let max = self.enc.decode(points.max_packed_value());
                match &self.source {
                    Source::Date(d) => d.ranges(min, max),
                    Source::Fixed(f) => Some(f.clone()),
                }
            }
            None => None,
        };
        let Some(ranges) = ranges else { return false };
        if self.has_sub && SEGMENT_THRESHOLD > seg.max_doc() / ranges.len() as u32 {
            return false;
        }
        let mut values: Vec<(i64, u32)> = Vec::with_capacity(points.size() as usize);
        let enc = self.enc;
        points.visit_all_values(&mut |doc, packed| values.push((enc.decode(packed), doc)));
        values.sort_unstable();
        let keys: Vec<i64> = values.into_iter().map(|(v, _)| v).collect();
        let (leaf, inner) = walk(&keys, &ranges, self.size, self.has_sub);
        self.leaf_visited += leaf;
        self.inner_visited += inner;
        self.optimized += 1;
        true
    }

    /// `populateDebugInfo`: the four counts, once a segment was optimized.
    pub fn debug_into(&self, out: &mut crate::json::Json) {
        use crate::json::Json;
        if self.optimized > 0 {
            out.push("optimized_segments", Json::from(self.optimized as u64));
            out.push("unoptimized_segments", Json::from((self.segments - self.optimized) as u64));
            out.push("leaf_visited", Json::from(self.leaf_visited as u64));
            out.push("inner_visited", Json::from(self.inner_visited as u64));
        }
    }
}

/// `Rounding.isUTC`.
fn is_utc(r: &Rounding) -> bool {
    match &r.zone {
        crate::format::TimeZone::Fixed { offset_secs, .. } => *offset_secs == 0,
        crate::format::TimeZone::Named(tz) => matches!(tz.name(), "UTC" | "Etc/UTC" | "Etc/UCT" | "UCT" | "Zulu" | "Etc/Zulu" | "Universal" | "Etc/Universal"),
    }
}

/// The range of `ranges` (sorted, disjoint, `[lo, hi)`) holding `v`.
#[inline]
fn find_range(ranges: &[(i64, i64)], v: i64) -> Option<usize> {
    let i = ranges.partition_point(|r| r.1 <= v);
    (i < ranges.len() && ranges[i].0 <= v).then_some(i)
}

/// Counts the points of a segment in each of several ranges in one walk of its tree
/// ([`FilterRewrite::count_segment`]).
struct MultiCount<'a> {
    ranges: &'a [(i64, i64)],
    /// Points of crossing leaves, per range.
    counts: Vec<u64>,
    /// The range of the cell `compare` last called inside (a leaf re-checked inside is
    /// reported doc by doc).
    last_inside: Option<usize>,
}

impl PointVisitor for MultiCount<'_> {
    fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
        let (lo, hi) = (encode::i64_from(min), encode::i64_from(max));
        let (first, last) = (self.ranges.first().map_or(i64::MAX, |r| r.0), self.ranges.last().map_or(i64::MIN, |r| r.1));
        if hi < first || lo >= last {
            return Relation::CellOutsideQuery;
        }
        let (a, b) = (find_range(self.ranges, lo), find_range(self.ranges, hi));
        if a.is_some() && a == b {
            self.last_inside = a;
            return Relation::CellInsideQuery;
        }
        if a.is_none() && b.is_none() && self.ranges.partition_point(|r| r.1 <= lo) == self.ranges.partition_point(|r| r.1 <= hi) {
            // Inside one gap between ranges.
            return Relation::CellOutsideQuery;
        }
        Relation::CellCrossesQuery
    }
    fn visit(&mut self, _doc: u32) {
        if let Some(i) = self.last_inside {
            self.counts[i] += 1;
        }
    }
    fn visit_docs(&mut self, docs: &[u32]) {
        if let Some(i) = self.last_inside {
            self.counts[i] += docs.len() as u64;
        }
    }
    fn visit_doc_range(&mut self, start: u32, end: u32) {
        if let Some(i) = self.last_inside {
            self.counts[i] += u64::from(end.saturating_sub(start));
        }
    }
    fn visit_value(&mut self, _doc: u32, packed: &[u8]) {
        if let Some(i) = find_range(self.ranges, encode::i64_from(packed)) {
            self.counts[i] += 1;
        }
    }
    fn visit_value_docs(&mut self, docs: &[u32], packed: &[u8]) {
        if let Some(i) = find_range(self.ranges, encode::i64_from(packed)) {
            self.counts[i] += docs.len() as u64;
        }
    }
}

/// The query's weight, for the profile's `segmentMatchAll` (`None` under a `match_all`).
fn query_weight(ctx: &BindCtx<'_>) -> Option<Box<dyn Weight>> {
    if ctx.matches_all {
        return None;
    }
    ctx.query.as_ref().and_then(|q| ctx.searcher.create_weight(q, ScoreMode::CompleteNoScores).ok())
}

/// `Helper.unwrapIntoConcreteQuery`: through constant-score and index-or-doc-values wrappers.
fn unwrap(q: &QueryRef) -> QueryRef {
    let mut q = q.clone();
    for _ in 0..16 {
        let next = if let Some(c) = q.as_any().downcast_ref::<ConstantScoreQuery>() {
            c.query.clone()
        } else if let Some(i) = q.as_any().downcast_ref::<IndexOrDocValuesQuery>() {
            i.index.clone()
        } else {
            break;
        };
        q = next;
    }
    q
}

/// `Helper.getShardBounds`: min and max of the field's points over the shard's segments.
fn shard_bounds(searcher: &Searcher, field: u32) -> Option<(i64, i64)> {
    let (mut min, mut max) = (i64::MAX, i64::MIN);
    for seg in searcher.segments() {
        if let Some(p) = seg.reader.points(field)
            && p.min_packed_value().len() >= 8
        {
            min = min.min(Encoding::Long.decode(p.min_packed_value()));
            max = max.max(Encoding::Long.decode(p.max_packed_value()));
        }
    }
    (min != i64::MAX && max != i64::MIN).then_some((min, max))
}

/// `BKDWriter.getNumLeftLeafNodes`.
fn num_left_leaves(leaves: usize) -> usize {
    let last_full_level = usize::BITS - 1 - leaves.leading_zeros();
    let full = 1usize << last_full_level;
    let left = full / 2;
    left + (leaves - full).min(left)
}

/// The replayed walk's state (`AbstractRangeCollector` + the visitor).
struct Walk<'a> {
    keys: &'a [i64],
    ranges: &'a [(i64, i64)],
    active: usize,
    visited_ranges: usize,
    max_ranges: usize,
    has_sub: bool,
    leaf: u32,
    inner: u32,
}

/// The walk stopped (`CollectionTerminatedException`).
struct Terminated;

impl Walk<'_> {
    fn below_upper(&self, v: i64) -> bool {
        v < self.ranges[self.active].1
    }

    fn above_lower(&self, v: i64) -> bool {
        v >= self.ranges[self.active].0
    }

    /// `iterateRangeEnd`: moves to the first range whose upper bound is above `v`; whether
    /// the walk is over.
    fn iterate_range_end(&mut self, v: i64) -> bool {
        while !self.below_upper(v) {
            self.active += 1;
            if self.active >= self.ranges.len() {
                return true;
            }
        }
        self.visited_ranges += 1;
        self.visited_ranges > self.max_ranges
    }

    fn advance(&mut self, v: i64) -> Result<(), Terminated> {
        if !self.below_upper(v) && self.iterate_range_end(v) {
            return Err(Terminated);
        }
        Ok(())
    }

    /// `intersectWithRanges` over the leaves `[lo, hi)` with cell `[min, max]`.
    fn node(&mut self, lo: usize, hi: usize, min: i64, max: i64) -> Result<(), Terminated> {
        // `compare`.
        self.advance(min)?;
        if !self.above_lower(max) {
            return Ok(());
        }
        let inside = self.above_lower(min) && self.below_upper(min) && self.above_lower(max) && self.below_upper(max);
        if inside {
            if !self.has_sub {
                self.inner += 1;
            }
            return Ok(());
        }
        if hi - lo > 1 {
            let mid = lo + num_left_leaves(hi - lo);
            let split = self.keys[mid * LEAF_POINTS];
            self.node(lo, mid, min, split)?;
            return self.node(mid, hi, split, max);
        }
        let end = ((lo + 1) * LEAF_POINTS).min(self.keys.len());
        for i in lo * LEAF_POINTS..end {
            self.advance(self.keys[i])?;
        }
        self.leaf += 1;
        Ok(())
    }
}

/// Replays `multiRangesTraverse` over sorted `keys`: (leaf nodes, inner nodes) visited.
fn walk(keys: &[i64], ranges: &[(i64, i64)], max_ranges: usize, has_sub: bool) -> (u32, u32) {
    let (Some(&min), Some(&max)) = (keys.first(), keys.last()) else { return (0, 0) };
    // `Ranges.firstRangeIndex`.
    if ranges[0].0 > max {
        return (0, 0);
    }
    let Some(first) = ranges.iter().position(|r| r.1 > min) else { return (0, 0) };
    let mut w = Walk { keys, ranges, active: first, visited_ranges: 0, max_ranges, has_sub, leaf: 0, inner: 0 };
    let leaves = keys.len().div_ceil(LEAF_POINTS);
    let _ = w.node(0, leaves, min, max);
    (w.leaf, w.inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::TimeZone;
    use crate::rounding::DateUnit;

    const DAY: i64 = 86_400_000;

    fn utc() -> TimeZone {
        TimeZone::Fixed { offset_secs: 0, id: "Z".into() }
    }

    fn ms(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
    }

    fn month_buckets() -> DateBuckets {
        DateBuckets { rounding: Rounding::new(RoundingKind::Unit(DateUnit::Month), utc(), 0).unwrap(), hard: (None, None), after: None, nanos: false, auto: Vec::new(), target: 1 }
    }

    #[test]
    fn date_ranges_cover_the_bounds_bucket_by_bucket() {
        let b = month_buckets();
        let r = b.ranges(ms("2016-01-01T00:00:00Z"), ms("2016-03-01T00:00:00Z")).unwrap();
        assert_eq!(
            r,
            vec![
                (ms("2016-01-01T00:00:00Z"), ms("2016-02-01T00:00:00Z")),
                (ms("2016-02-01T00:00:00Z"), ms("2016-03-01T00:00:00Z")),
                (ms("2016-03-01T00:00:00Z"), ms("2016-03-01T00:00:00Z") + 1),
            ]
        );
        // The first range starts at the low bound itself, not its rounding.
        let r = b.ranges(ms("2024-10-31T00:00:00Z"), ms("2024-12-31T23:59:59.999Z")).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].0, ms("2024-10-31T00:00:00Z"));
        assert_eq!(r[2].1, ms("2025-01-01T00:00:00Z"));
        // Hard bounds clip, and refuse when nothing is left.
        let h = DateBuckets { hard: (Some(ms("2017-01-01T00:00:00Z")), None), ..month_buckets() };
        assert!(h.ranges(ms("2016-01-01T00:00:00Z"), ms("2016-03-01T00:00:00Z")).is_none());
        // Too many filters.
        let d = DateBuckets { rounding: Rounding::new(RoundingKind::Unit(DateUnit::Day), utc(), 0).unwrap(), ..month_buckets() };
        assert!(d.ranges(0, 3001 * DAY).is_none());
        assert_eq!(d.ranges(0, 2999 * DAY).unwrap().len(), 3000);
    }

    #[test]
    fn left_leaves_follow_lucene() {
        assert_eq!(num_left_leaves(2), 1);
        assert_eq!(num_left_leaves(3), 2);
        assert_eq!(num_left_leaves(4), 2);
        assert_eq!(num_left_leaves(5), 3);
        assert_eq!(num_left_leaves(6), 4);
        assert_eq!(num_left_leaves(7), 4);
        assert_eq!(num_left_leaves(8), 4);
    }

    #[test]
    fn walks_match_opensearch_profiles() {
        // OpenSearch's 10_histogram: four dates over three months, one leaf.
        let keys = [ms("2016-01-01T00:00:00Z"), ms("2016-01-02T00:00:00Z"), ms("2016-02-01T00:00:00Z"), ms("2016-03-01T00:00:00Z")];
        let r = month_buckets().ranges(keys[0], keys[3]).unwrap();
        assert_eq!(walk(&keys, &r, usize::MAX, false), (1, 0));
        // 360_date_histogram: a range query clips the buckets; the walk stops in the leaf at
        // the first value past them, so the leaf is never counted.
        let keys = ["2024-10-31", "2024-11-11", "2024-11-28", "2024-12-25", "2025-01-01", "2025-02-14"].map(|d| ms(&format!("{d}T00:00:00Z")));
        let r = month_buckets().ranges(keys[0], ms("2024-12-31T23:59:59.999Z")).unwrap();
        assert_eq!(walk(&keys, &r, usize::MAX, false), (0, 0));
        // 40_range: doubles 42, 100, 50 against [*, 50), [50, 150), [150, *).
        let e = Encoding::Double;
        let mut keys: Vec<i64> = [42.0, 100.0, 50.0].iter().map(|&v| e.encode(v)).collect();
        keys.sort();
        let r: Vec<(i64, i64)> = [(f64::NEG_INFINITY, 50.0), (50.0, 150.0), (150.0, f64::INFINITY)].iter().map(|&(a, b)| (e.encode(a), e.encode(b))).collect();
        assert_eq!(walk(&keys, &r, usize::MAX, false), (1, 0));
        // A cell inside one range is counted without visiting its leaf.
        let keys: Vec<i64> = (0..10).collect();
        assert_eq!(walk(&keys, &[(0, 100)], usize::MAX, false), (0, 1));
        assert_eq!(walk(&keys, &[(0, 100)], usize::MAX, true), (0, 0));
        // Ranges past the data: nothing to walk.
        assert_eq!(walk(&keys, &[(100, 200)], usize::MAX, false), (0, 0));
    }

    #[test]
    fn multi_leaf_walk_descends_the_tree() {
        // 1100 points in three leaves (512, 512, 76): the root crosses, its left subtree (two
        // leaves) is inside the first range, the right leaf crosses into the second.
        let keys: Vec<i64> = (0..1100).collect();
        assert_eq!(walk(&keys, &[(0, 1050), (1050, 2000)], usize::MAX, false), (1, 1));
        // `size` 1: the walk stops at the second range.
        assert_eq!(walk(&keys, &[(0, 1050), (1050, 2000)], 0, false), (0, 1));
    }

    #[test]
    fn encodings_saturate_like_java() {
        assert_eq!(Encoding::Int.encode(f64::NEG_INFINITY), i32::MIN as i64);
        assert_eq!(Encoding::Long.encode(f64::INFINITY), i64::MAX);
        assert!(Encoding::Double.encode(-1.0) < Encoding::Double.encode(0.5));
        assert_eq!(Encoding::Long.decode(&search_codec::points::encode::i64_bytes(-5)), -5);
        assert_eq!(Encoding::Int.decode(&search_codec::points::encode::i32_bytes(-5)), -5);
        let h = |v: f32| Encoding::HalfFloat.decode(&search_codec::points::encode::f16_bytes(v));
        assert!(h(-1.0) < h(0.0) && h(0.0) < h(2.5));
        assert_eq!(h(2.5), Encoding::HalfFloat.encode(2.5));
    }
}
