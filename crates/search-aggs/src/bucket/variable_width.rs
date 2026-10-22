//! `variable_width_histogram`: one-pass clustering of numeric values into about `buckets`
//! buckets of varying width.
//!
//! Shard side (Elasticsearch's `VariableWidthHistogramAggregator`): the first `initial_buffer`
//! values each get their own bucket; then the buffer is sorted and cut into `shard_size * 3/4`
//! equal-count clusters, and every later value joins the nearest cluster (by centroid) unless it
//! is more than twice the average centroid distance away and fewer than `shard_size` clusters
//! exist, in which case it starts a new cluster. Bucket ordinals are rewritten on every change
//! (and so are the deferred `(doc, bucket)` pairs replayed into sub-aggregations), including
//! Elasticsearch's quirk of mapping buffered buckets by their buffer position rather than their
//! sorted position — so `doc_count`s follow Elasticsearch even where they disagree with the
//! cluster bounds.
//!
//! Reduce: buckets with equal centroids merge, then the two closest centroids merge until
//! `buckets` remain; the final reduce merges buckets with equal minimums and splits overlapping
//! bounds at their midpoint.

use std::cmp::Ordering;

use search_index::{Searcher, SegmentContext};

use crate::bucket::terms::count_inner_buckets;
use crate::collect::{Aggregator, Batch, BindCtx, Subs};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, multi_bucket_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::metrics::simple::{java_max, java_min};
use crate::request::{AggDef, PathElement, java_double_compare};
use crate::util::LucenePq;
use crate::values::{BoundVs, DocCounter, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

/// A parsed `variable_width_histogram`.
#[derive(Clone, Debug)]
pub struct VariableWidthSpec {
    pub vs: VsSpec,
    pub buckets: i32,
    /// `-1`: `buckets * 50`.
    pub shard_size: i32,
    /// `-1`: `min(10 * shard_size, 50000)`.
    pub initial_buffer: i32,
}

impl VariableWidthSpec {
    fn shard_size(&self) -> i32 {
        if self.shard_size == -1 { self.buckets.saturating_mul(50) } else { self.shard_size }
    }

    fn initial_buffer(&self) -> i32 {
        if self.initial_buffer == -1 { self.shard_size().saturating_mul(10).min(50_000) } else { self.initial_buffer }
    }
}

pub fn parse(p: &mut XParser<'_>, name: &str) -> Result<VariableWidthSpec> {
    let opts = VsOpts::new(true, true, true);
    let mut fields = VsSpec::fields(opts);
    fields.push(Field::new("buckets", Tokens::INT));
    fields.push(Field::new("shard_size", Tokens::INT));
    fields.push(Field::new("initial_buffer", Tokens::INT));
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut s = VariableWidthSpec { vs: VsSpec::default(), buckets: 10, shard_size: -1, initial_buffer: -1 };
    p.object("variable_width_histogram", &fields, |p, i| {
        let f = names[i];
        if s.vs.parse_field(p, f)? {
            return Ok(());
        }
        let n = p.int_value()?;
        match f {
            "buckets" => {
                if n <= 0 {
                    return Err(AggError::illegal_argument(format!("buckets must be greater than [0] for [{name}]")));
                }
                s.buckets = n;
            }
            "shard_size" => {
                if n <= 1 {
                    return Err(AggError::illegal_argument(format!("shard_size must be greater than [1] for [{name}]")));
                }
                s.shard_size = n;
            }
            _ => {
                if n <= 0 {
                    return Err(AggError::illegal_argument(format!("initial_buffer must be greater than [0] for [{name}]")));
                }
                s.initial_buffer = n;
            }
        }
        Ok(())
    })?;
    s.vs.check_required(opts)?;
    Ok(s)
}

/// `mergePhaseInitialBucketCount(shardSize)`.
fn merge_phase_initial(shard_size: i32) -> i32 {
    (shard_size as i64 * 3 / 4) as i32
}

/// `search.max_buckets`.
const MAX_BUCKETS: i32 = 65_536;

/// Binds `variable_width_histogram`.
pub fn create(def: &AggDef, spec: &VariableWidthSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let name = &def.name;
    if spec.buckets > MAX_BUCKETS {
        return Err(AggError::illegal_argument(format!("buckets must be less than {MAX_BUCKETS}")));
    }
    let initial_buffer = spec.initial_buffer();
    let shard_size = spec.shard_size();
    if initial_buffer < spec.buckets {
        return Err(AggError::illegal_argument(format!(
            "initial_buffer must be at least buckets but was [{initial_buffer}<{}] for [{name}]",
            spec.buckets
        )));
    }
    let merge_init = merge_phase_initial(shard_size);
    if merge_init < spec.buckets {
        return Err(AggError::illegal_argument(format!("3/4 of shard_size must be at least buckets but was [{merge_init}<{}] for [{name}]", spec.buckets)));
    }
    let vs = bind(&spec.vs, ctx.lookup, VsType::Numeric, false)?;
    if !vs.unmapped && vs.vs_type != VsType::Numeric {
        return Err(vs.unsupported("variable_width_histogram"));
    }
    let has_values = !vs.unmapped || vs.missing.is_some();
    if has_values && !ctx.cardinality.is_one() {
        return Err(AggError::illegal_argument(
            "[variable_width_histogram] cannot be nested inside an aggregation that collects more than a single bucket.",
        ));
    }
    let subs = Subs::bind(def, ctx)?;
    // Sub-aggregations are always deferred, and nested docs' scores can't be replayed.
    if let (Some(nested), Some(scoring)) = (&ctx.nested_agg, subs.nodes.iter().find(|n| n.agg.needs_scores())) {
        return Err(AggError::new(
            500,
            "illegal_state_exception",
            format!(
                "VariableWidthHistogram agg [{}] is the child of the nested agg [{nested}], and also has a scoring child agg [{}].  This combination is not supported because it requires executing in [depth_first] mode, which the VariableWidthHistogram agg cannot do.",
                def.name, scoring.def.name
            ),
        ));
    }
    Ok(Box::new(VwAgg {
        format: vs.format.clone(),
        vs,
        has_values,
        reader: None,
        target: spec.buckets,
        shard_size,
        buffer_limit: initial_buffer as usize,
        merging: false,
        buffer: Vec::new(),
        clusters: Clusters::default(),
        counts: Vec::new(),
        record: !subs.is_empty(),
        segments: Vec::new(),
        cur_seg: None,
        searcher: ctx.searcher.clone(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
        finished: false,
    }))
}

/// Intermediate buckets, sorted by centroid.
#[derive(Default)]
struct Clusters {
    mins: Vec<f64>,
    maxes: Vec<f64>,
    centroids: Vec<f64>,
    sizes: Vec<f64>,
    avg_distance: f64,
}

impl Clusters {
    fn len(&self) -> usize {
        self.centroids.len()
    }

    fn push(&mut self, v: f64) {
        self.mins.push(v);
        self.maxes.push(v);
        self.centroids.push(v);
        self.sizes.push(1.0);
    }

    fn add(&mut self, i: usize, v: f64) {
        self.maxes[i] = java_max(self.maxes[i], v);
        self.mins[i] = java_min(self.mins[i], v);
        let size = self.sizes[i];
        self.centroids[i] = ((self.centroids[i] * size) + v) / (size + 1.0);
        self.sizes[i] += 1.0;
    }

    fn update_avg_distance(&mut self) {
        let n = self.len();
        self.avg_distance = (self.centroids[n - 1] - self.centroids[0]) / (n as i32 - 1) as f64;
    }

    /// `BigArrays.DoubleBinarySearcher.search(0, n - 1, value)`: the index of the closest
    /// centroid.
    fn nearest(&self, value: f64) -> usize {
        let c = &self.centroids;
        let cmp = |i: usize| java_double_compare(c[i], value);
        let dist = |i: usize| (c[i] - value).abs();
        let closest = |a: usize, b: usize| if dist(a) < dist(b) { a } else { b };
        let (mut from, mut to) = (0i64, c.len() as i64 - 1);
        while from < to {
            let mid = ((from + to) as u64 >> 1) as i64;
            match cmp(mid as usize) {
                Ordering::Equal => return mid as usize,
                Ordering::Less => {
                    if mid < to {
                        if cmp(mid as usize + 1) == Ordering::Greater {
                            return closest(mid as usize, mid as usize + 1);
                        }
                    } else if mid == to {
                        return mid as usize;
                    }
                    from = mid + 1;
                }
                Ordering::Greater => {
                    if mid > from {
                        if cmp(mid as usize - 1) == Ordering::Less {
                            return closest(mid as usize, mid as usize - 1);
                        }
                    } else if mid == 0 {
                        return mid as usize;
                    }
                    to = mid - 1;
                }
            }
        }
        from as usize
    }
}

/// The `(doc, bucket)` pairs collected in one segment.
struct SegEntries {
    seg: usize,
    docs: Vec<u32>,
    ords: Vec<u32>,
    scores: Vec<f32>,
}

struct VwAgg {
    vs: BoundVs,
    has_values: bool,
    reader: Option<NumReader>,
    format: DocValueFormat,
    target: i32,
    shard_size: i32,
    buffer_limit: usize,
    /// Past the buffer phase.
    merging: bool,
    buffer: Vec<f64>,
    clusters: Clusters,
    /// Doc count per bucket ordinal.
    counts: Vec<u64>,
    record: bool,
    segments: Vec<SegEntries>,
    cur_seg: Option<usize>,
    searcher: Searcher,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
    finished: bool,
}

impl VwAgg {
    fn collect_bucket(&mut self, doc: u32, ord: usize, score: f32, inc: u64) -> Result<()> {
        if self.counts.len() <= ord {
            self.counts.resize(ord + 1, 0);
            if self.counts.len().is_power_of_two() {
                self.track()?;
            }
        }
        self.counts[ord] += inc;
        if let Some(s) = self.cur_seg {
            let e = &mut self.segments[s];
            e.docs.push(doc);
            e.ords.push(ord as u32);
            if self.subs.needs_scores() {
                e.scores.push(score);
            }
            if e.docs.len().is_power_of_two() {
                self.track()?;
            }
        }
        Ok(())
    }

    fn track(&mut self) -> Result<()> {
        let entries: usize = self.segments.iter().map(|s| s.docs.capacity() * 8 + s.scores.capacity() * 4).sum();
        self.tracker.ensure(entries + self.counts.capacity() * 8 + self.buffer.capacity() * 8 + self.clusters.len() * 32)
    }

    /// `rewriteBuckets(newNumBuckets, mergeMap)` for the doc counts and the recorded pairs.
    fn rewrite(&mut self, new_len: usize, map: impl Fn(usize) -> usize) {
        let mut counts = vec![0u64; new_len];
        for (i, &c) in self.counts.iter().enumerate() {
            if c != 0 {
                counts[map(i)] += c;
            }
        }
        self.counts = counts;
        for s in &mut self.segments {
            for o in &mut s.ords {
                *o = map(*o as usize) as u32;
            }
        }
    }

    /// `MergeBucketsPhase(buffer, bufferSize)`: `bucketBufferedDocs`, then the average distance.
    fn start_merge(&mut self) {
        self.merging = true;
        let buffer = std::mem::take(&mut self.buffer);
        let len = buffer.len();
        let n = merge_phase_initial(self.shard_size) as usize;
        let mut sorted: Vec<usize> = (0..len).collect();
        sorted.sort_by(|&a, &b| java_double_compare(buffer[a], buffer[b]));
        let per = (len as f64 / n as f64).ceil() as usize;
        let mut map = vec![0usize; len];
        let mut bucket = 0usize;
        for i in 0..len {
            let v = buffer[sorted[i]];
            // Elasticsearch maps by the doc's buffer position, not its sorted position.
            map[i] = sorted[i] / per;
            if bucket == self.clusters.len() {
                self.clusters.push(v);
            } else {
                self.clusters.add(bucket, v);
            }
            if (i + 1) % per == 0 {
                bucket += 1;
            }
        }
        self.rewrite(bucket + 1, |b| map[b]);
        if len > 1 {
            self.clusters.update_avg_distance();
        }
    }

    /// `moveLastCluster(index)`.
    fn move_last(&mut self, index: usize) {
        let n = self.clusters.len();
        if index == n - 1 {
            return;
        }
        let c = &mut self.clusters;
        for v in [&mut c.maxes, &mut c.mins, &mut c.centroids, &mut c.sizes] {
            let last = v.pop().expect("cluster");
            v.insert(index, last);
        }
        self.rewrite(n, |i| {
            if i < index {
                i
            } else if i == n - 1 {
                index
            } else {
                i + 1
            }
        });
    }

    /// `CollectionPhase.collectValue`.
    fn collect_value(&mut self, doc: u32, v: f64, score: f32, inc: u64) -> Result<()> {
        if !self.merging {
            if self.buffer.len() < self.buffer_limit {
                self.buffer.push(v);
                self.collect_bucket(doc, self.buffer.len() - 1, score, inc)?;
            }
            if self.buffer.len() == self.buffer_limit {
                self.start_merge();
            }
            return Ok(());
        }
        let mut ord = self.clusters.nearest(v);
        let distance = (self.clusters.centroids[ord] - v).abs();
        if distance > 2.0 * self.clusters.avg_distance && (self.clusters.len() as i64) < self.shard_size as i64 {
            self.clusters.push(v);
            let last = self.clusters.len() - 1;
            self.collect_bucket(doc, last, score, inc)?;
            if v > self.clusters.centroids[ord] {
                ord += 1;
            }
            self.move_last(ord);
            self.clusters.update_avg_distance();
        } else {
            self.clusters.add(ord, v);
            self.collect_bucket(doc, ord, score, inc)?;
            if ord == 0 || ord == self.clusters.len() - 1 {
                self.clusters.update_avg_distance();
            }
        }
        Ok(())
    }

    /// Replays the recorded pairs into the sub-aggregations.
    fn replay(&mut self) -> Result<()> {
        let segs: Vec<SegmentContext> = self.searcher.segments().to_vec();
        let recorded = std::mem::take(&mut self.segments);
        for e in &recorded {
            self.subs.set_segment(&segs[e.seg])?;
            for i in 0..e.docs.len() {
                self.subs.push(e.docs[i], e.ords[i], e.scores.get(i).copied().unwrap_or(f32::NAN))?;
            }
            self.subs.flush()?;
            self.subs.finish_segment()?;
        }
        Ok(())
    }
}

impl Aggregator for VwAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        self.reader = if self.has_values { Some(self.vs.numeric_reader(seg)?) } else { None };
        if self.record {
            let ord = self.searcher.segments().iter().position(|s| s.base == seg.base).unwrap_or(0);
            if self.segments.last().is_none_or(|s| s.seg != ord) {
                self.segments.push(SegEntries { seg: ord, docs: Vec::new(), ords: Vec::new(), scores: Vec::new() });
            }
            self.cur_seg = Some(self.segments.len() - 1);
        }
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let Some(mut r) = self.reader.take() else { return Ok(()) };
        let mut vals: smallvec::SmallVec<[f64; 4]> = smallvec::SmallVec::new();
        let res = (|| {
            for i in 0..b.len() {
                let doc = b.docs[i];
                let score = b.score(i);
                vals.clear();
                vals.extend_from_slice(r.doubles(doc, score)?);
                if vals.is_empty() {
                    continue;
                }
                let inc = self.doc_counter.count(doc);
                // Elasticsearch means to skip repeated values of a document but never updates
                // its `prevVal`: every value is collected.
                for &v in &vals {
                    self.collect_value(doc, v, score, inc)?;
                }
            }
            Ok(())
        })();
        self.reader = Some(r);
        res
    }

    fn post_collection(&mut self) -> Result<()> {
        if !self.finished {
            self.finished = true;
            if !self.merging {
                // `finalNumBuckets()` of the buffer phase builds the merge phase.
                self.start_merge();
            }
            if self.record {
                self.replay()?;
            }
        }
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        if owning.len() > 1 {
            // Elasticsearch builds a single result whatever it is asked for (an unmapped field
            // under a multi-bucket parent gets this far).
            return Err(AggError::new(500, "array_index_out_of_bounds_exception", "Index 1 out of bounds for length 1"));
        }
        let n = self.clusters.len();
        let ords: Vec<u32> = (0..n as u32).collect();
        let aggs = self.subs.build(&ords)?;
        let mut buckets: Vec<VwBucket> = aggs
            .into_iter()
            .enumerate()
            .map(|(i, aggs)| VwBucket {
                centroid: self.clusters.centroids[i],
                min: self.clusters.mins[i],
                max: self.clusters.maxes[i],
                doc_count: self.counts.get(i).copied().unwrap_or(0),
                aggs,
            })
            .collect();
        buckets.sort_by(|a, b| java_double_compare(a.centroid, b.centroid));
        let value = VariableWidthHistogram { buckets, target: self.target, format: self.format.clone() };
        Ok(owning.iter().map(|_| AggValue::VariableWidthHistogram(Box::new(value.clone()))).collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::VariableWidthHistogram(Box::new(VariableWidthHistogram { buckets: Vec::new(), target: self.target, format: self.format.clone() }))
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores() || self.vs.script.as_ref().is_some_and(|s| s.needs_score())
    }

    fn validate_sort(&self, name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(format!(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end. [{name}] is not single-bucket."
        )))
    }
}

/// A `variable_width_histogram` bucket.
#[derive(Clone, Debug)]
pub struct VwBucket {
    pub centroid: f64,
    pub min: f64,
    pub max: f64,
    pub doc_count: u64,
    pub aggs: Aggs,
}

/// `InternalVariableWidthHistogram`.
#[derive(Clone, Debug)]
pub struct VariableWidthHistogram {
    pub buckets: Vec<VwBucket>,
    pub target: i32,
    pub format: DocValueFormat,
}

/// `reduceBucket`: bounds widened, centroid weighted by doc counts (summed in list order).
fn reduce_bucket(group: Vec<VwBucket>, ctx: &mut ReduceCtx<'_>) -> Result<VwBucket> {
    let (mut min, mut max, mut sum) = (f64::INFINITY, f64::NEG_INFINITY, 0.0);
    let mut doc_count = 0u64;
    for b in &group {
        min = java_min(min, b.min);
        max = java_max(max, b.max);
        sum += b.doc_count as f64 * b.centroid;
        doc_count += b.doc_count;
    }
    let aggs = Aggs::reduce_hashed(group.into_iter().map(|b| b.aggs).collect(), ctx)?;
    Ok(VwBucket { centroid: sum / doc_count as f64, min, max, doc_count, aggs })
}

/// `BucketRange` of a merge plan.
#[derive(Clone, Copy)]
struct Range {
    start: usize,
    end: usize,
    min: f64,
    max: f64,
    centroid: f64,
    doc_count: u64,
}

impl Range {
    fn merge_with(&mut self, o: &Range) {
        self.start = self.start.min(o.start);
        self.end = self.end.max(o.end);
        if self.doc_count + o.doc_count > 0 {
            self.centroid = ((self.centroid * self.doc_count as f64) + (o.centroid * o.doc_count as f64)) / (self.doc_count + o.doc_count) as f64;
            self.doc_count += o.doc_count;
        }
        self.min = java_min(self.min, o.min);
        self.max = java_max(self.max, o.max);
    }
}

/// `mergeBucketsWithPlan`: each range's buckets are reduced (last to first) into its first slot.
fn merge_with_plan(buckets: &mut Vec<VwBucket>, plan: &[Range], ctx: &mut ReduceCtx<'_>) -> Result<()> {
    for r in plan.iter().rev() {
        if r.start == r.end {
            continue;
        }
        let mut group: Vec<VwBucket> = buckets.drain(r.start + 1..=r.end).rev().collect();
        let first = buckets[r.start].clone();
        group.push(first);
        let removed: usize = group.iter().map(|b| count_inner_buckets(&b.aggs) + 1).sum();
        ctx.release_buckets(removed);
        ctx.consume_buckets(1)?;
        buckets[r.start] = reduce_bucket(group, ctx)?;
    }
    Ok(())
}

/// `mergeBucketsIfNeeded`: repeatedly merges the two closest centroids until `target` remain.
fn merge_if_needed(buckets: &mut Vec<VwBucket>, target: i32, ctx: &mut ReduceCtx<'_>) -> Result<()> {
    let mut ranges: Vec<Range> = buckets
        .iter()
        .enumerate()
        .map(|(i, b)| Range { start: i, end: i, min: 0.0, max: 0.0, centroid: b.centroid, doc_count: b.doc_count })
        .collect();
    while ranges.len() as i64 > target as i64 {
        let mut closest = 0;
        let mut smallest = f64::INFINITY;
        for i in 0..ranges.len() - 1 {
            let d = ranges[i + 1].centroid - ranges[i].centroid;
            if d < smallest {
                closest = i;
                smallest = d;
            }
        }
        let next = ranges.remove(closest + 1);
        ranges[closest].merge_with(&next);
    }
    merge_with_plan(buckets, &ranges, ctx)
}

/// `mergeBucketsWithSameMin`.
fn merge_same_min(buckets: &mut Vec<VwBucket>, ctx: &mut ReduceCtx<'_>) -> Result<()> {
    let mut ranges: Vec<Range> =
        buckets.iter().enumerate().map(|(i, b)| Range { start: i, end: i, min: b.min, max: 0.0, centroid: 0.0, doc_count: 0 }).collect();
    let mut i = 0;
    while i + 1 < ranges.len() {
        if ranges[i].min == ranges[i + 1].min {
            let next = ranges.remove(i + 1);
            ranges[i].merge_with(&next);
        } else {
            i += 1;
        }
    }
    merge_with_plan(buckets, &ranges, ctx)
}

/// `adjustBoundsForOverlappingBuckets`.
fn adjust_bounds(buckets: &mut [VwBucket]) {
    for i in 1..buckets.len() {
        if buckets[i].min < buckets[i - 1].max {
            buckets[i].min = (buckets[i - 1].max + buckets[i].min) / 2.0;
            buckets[i - 1].max = buckets[i].min;
        }
    }
}

impl Internal for VariableWidthHistogram {
    fn type_name(&self) -> &'static str {
        "variable_width_histogram"
    }
    fn class_name(&self) -> &'static str {
        "InternalVariableWidthHistogram"
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let target = list[0].target;
        let format = list[0].format.clone();
        struct Cursor {
            cur: VwBucket,
            rest: std::vec::IntoIter<VwBucket>,
        }
        let less = |a: &Cursor, b: &Cursor| java_double_compare(a.cur.centroid, b.cur.centroid) == Ordering::Less;
        let mut pq = LucenePq::new(list.len());
        for h in list {
            let mut rest = h.buckets.into_iter();
            if let Some(cur) = rest.next() {
                pq.add(Cursor { cur, rest }, &less);
            }
        }
        let mut buckets = Vec::new();
        let mut group: Vec<VwBucket> = Vec::new();
        let mut key: Option<f64> = None;
        while let Some(top) = pq.top_mut() {
            let c = top.cur.centroid;
            if key.is_some_and(|k| java_double_compare(c, k) != Ordering::Equal) {
                buckets.push(reduce_bucket(std::mem::take(&mut group), ctx)?);
                ctx.consume_buckets(1)?;
            }
            key = Some(c);
            match top.rest.next() {
                Some(next) => {
                    group.push(std::mem::replace(&mut top.cur, next));
                    pq.update_top(&less);
                }
                None => {
                    let done = pq.pop(&less).expect("top");
                    group.push(done.cur);
                }
            }
        }
        if !group.is_empty() {
            buckets.push(reduce_bucket(group, ctx)?);
            ctx.consume_buckets(1)?;
        }
        merge_if_needed(&mut buckets, target, ctx)?;
        if ctx.is_final() {
            merge_same_min(&mut buckets, ctx)?;
            adjust_bounds(&mut buckets);
        }
        Ok(VariableWidthHistogram { buckets, target, format })
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        let f = &self.format;
        let raw = f.is_raw();
        let buckets = self
            .buckets
            .iter()
            .map(|b| {
                let mut o = Json::obj();
                o.push("min", Json::Float(b.min));
                if !raw {
                    o.push("min_as_string", f.double_json(b.min));
                }
                o.push("key", Json::Float(b.centroid));
                if !raw {
                    o.push("key_as_string", f.double_json(b.centroid));
                }
                o.push("max", Json::Float(b.max));
                if !raw {
                    o.push("max_as_string", f.double_json(b.max));
                }
                o.push("doc_count", Json::from(b.doc_count));
                b.aggs.render_into(&mut o, rc);
                o
            })
            .collect();
        out.push("buckets", Json::Arr(buckets));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }
    fn ram_bytes(&self) -> usize {
        self.buckets.iter().map(|b| 64 + b.aggs.ram_bytes()).sum::<usize>() + 64
    }
}

impl MultiBuckets for VariableWidthHistogram {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        Json::Float(self.buckets[i].centroid)
    }
    fn key_string(&self, i: usize) -> String {
        self.format.double_string(self.buckets[i].centroid)
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
            i += 1;
            keep[i - 1]
        });
    }
    fn reorder(&mut self, order: &[usize]) {
        let mut slots: Vec<Option<VwBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
    fn numeric_key(&self, i: usize) -> Option<f64> {
        Some(self.buckets[i].centroid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clusters(c: &[f64]) -> Clusters {
        Clusters { centroids: c.to_vec(), ..Default::default() }
    }

    #[test]
    fn nearest_centroid_like_binary_searcher() {
        let c = clusters(&[1.0, 5.0, 9.0, 20.0]);
        assert_eq!(c.nearest(0.0), 0);
        assert_eq!(c.nearest(3.0), 0); // a tie goes to the lower neighbour
        assert_eq!(c.nearest(6.0), 1);
        assert_eq!(c.nearest(9.0), 2);
        assert_eq!(c.nearest(14.0), 2);
        assert_eq!(c.nearest(100.0), 3);
        assert_eq!(clusters(&[4.0]).nearest(-3.0), 0);
    }
}
