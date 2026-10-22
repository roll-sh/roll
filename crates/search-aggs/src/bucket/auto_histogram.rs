//! `auto_date_histogram`: a date histogram that picks its interval so that at most `buckets`
//! buckets come out.
//!
//! Shards start at the finest rounding (second, or `minimum_interval`) and move to coarser ones
//! while collecting, exactly as Elasticsearch does — `FromSingle` (one owning bucket: exact
//! bucket counts, rebucketing on every promotion) or `FromMany` (under a multi-bucket parent:
//! estimated counts, a "wasted buckets" budget before rebucketing, a final rebucket). Merging
//! buckets sums their doc counts, so a document with several values whose buckets later merge
//! counts once per value, like in Elasticsearch. Sub-aggregations are deferred: the collected
//! `(doc, bucket)` pairs are rewritten on every merge and replayed into the final buckets.
//!
//! The reduce takes the coarsest shard rounding, merges by rounded key, re-promotes while there
//! are too many buckets, and in the final reduce adds empty buckets, then merges runs of
//! consecutive buckets with the rounding's inner intervals (`interval: "7d"`, `"3M"`, …).
//! The rounding the reduce settles on depends on the shards' roundings, so the shard-side
//! promotion logic (estimates included) is reproduced operation by operation.

use std::sync::Arc;

use search_index::{Searcher, SegmentContext};

use crate::collect::{Aggregator, Batch, BindCtx, Subs};
use crate::error::{AggError, Result};
use crate::format::{DocValueFormat, TimeZone};
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, multi_bucket_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::rounding::{DateUnit, Rounding, RoundingKind};
use crate::util::{LucenePq, OrdMap};
use crate::values::{BoundVs, DocCounter, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

/// A parsed `auto_date_histogram`.
#[derive(Clone, Debug)]
pub struct AutoDateHistogramSpec {
    pub vs: VsSpec,
    pub buckets: i32,
    pub minimum_interval: Option<String>,
}

/// `ALLOWED_INTERVALS.values()` as Elasticsearch 9.5.5 prints them (an immutable `Map.of`).
const ALLOWED: &str = "second, year, hour, day, month, minute";

/// `search.max_buckets` (the node setting the bucket ceiling is derived from).
const MAX_BUCKETS: i32 = 65_536;

pub fn parse(p: &mut XParser<'_>, name: &str) -> Result<AutoDateHistogramSpec> {
    let opts = VsOpts::new(true, true, true);
    let mut fields = VsSpec::fields(opts);
    fields.push(Field::new("buckets", Tokens::INT));
    fields.push(Field::new("minimum_interval", Tokens::STRING_OR_NULL));
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut s = AutoDateHistogramSpec { vs: VsSpec::default(), buckets: 10, minimum_interval: None };
    p.object("auto_date_histogram", &fields, |p, i| {
        let f = names[i];
        if s.vs.parse_field(p, f)? {
            return Ok(());
        }
        match f {
            "buckets" => {
                let n = p.int_value()?;
                if n <= 0 {
                    return Err(AggError::illegal_argument(format!("buckets must be greater than 0 for [{name}]")));
                }
                s.buckets = n;
            }
            _ => {
                let v = p.text_or_null()?;
                if let Some(v) = &v
                    && !ALLOWED.split(", ").any(|a| a == v)
                {
                    return Err(AggError::illegal_argument(format!("minimum_interval must be one of [[{ALLOWED}]]")));
                }
                s.minimum_interval = v;
            }
        }
        Ok(())
    })?;
    s.vs.check_required(opts)?;
    Ok(s)
}

/// `RoundingInfo`: a rounding and the multiples of it the histogram may use.
#[derive(Debug)]
pub struct RoundingInfo {
    pub rounding: Rounding,
    pub inner: &'static [i32],
    pub rough_millis: i64,
    pub abbreviation: &'static str,
}

impl RoundingInfo {
    fn max_inner(&self) -> i32 {
        self.inner[self.inner.len() - 1]
    }

    fn max_rough_millis(&self) -> i64 {
        self.rough_millis * self.max_inner() as i64
    }
}

/// `buildRoundings(timeZone, minimumInterval)`.
fn build_roundings(zone: &TimeZone, minimum: Option<&str>) -> Result<Vec<RoundingInfo>> {
    const ALL: [(DateUnit, &str, i64, &str, &[i32]); 6] = [
        (DateUnit::Second, "second", 1000, "s", &[1, 5, 10, 30]),
        (DateUnit::Minute, "minute", 60 * 1000, "m", &[1, 5, 10, 30]),
        (DateUnit::Hour, "hour", 60 * 60 * 1000, "h", &[1, 3, 12]),
        (DateUnit::Day, "day", 24 * 60 * 60 * 1000, "d", &[1, 7]),
        (DateUnit::Month, "month", 30 * 24 * 60 * 60 * 1000, "M", &[1, 3]),
        (DateUnit::Year, "year", 365 * 24 * 60 * 60 * 1000, "y", &[1, 5, 10, 20, 50, 100]),
    ];
    let from = minimum.and_then(|m| ALL.iter().position(|a| a.1 == m)).unwrap_or(0);
    ALL[from..]
        .iter()
        .map(|&(unit, _, rough_millis, abbreviation, inner)| {
            Ok(RoundingInfo { rounding: Rounding::new(RoundingKind::Unit(unit), zone.clone(), 0)?, inner, rough_millis, abbreviation })
        })
        .collect()
}

/// Binds `auto_date_histogram`.
pub fn create(def: &AggDef, spec: &AutoDateHistogramSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::Date, true)?;
    if vs.is_counter() {
        return Err(vs.unsupported("auto_date_histogram"));
    }
    if !vs.vs_type.is_numeric() && !vs.unmapped {
        return Err(vs.unsupported("auto_date_histogram"));
    }
    let zone = vs.time_zone.clone().unwrap_or_default();
    let roundings = build_roundings(&zone, spec.minimum_interval.as_deref())?;
    // The largest inner interval of every rounding but the last.
    let Some(max_inner) = roundings[..roundings.len() - 1].iter().flat_map(|r| r.inner.iter().copied()).max() else {
        return Err(AggError::new(500, "no_such_element_exception", "No value present"));
    };
    let ceiling = MAX_BUCKETS / max_inner;
    if spec.buckets > ceiling {
        return Err(AggError::illegal_argument(format!("buckets must be less than {ceiling}")));
    }
    let many = !ctx.cardinality.is_one();
    let subs = Subs::bind(def, ctx)?;
    let n = roundings.len();
    let format = vs.format.clone();
    let has_values = !vs.unmapped || vs.missing.is_some();
    Ok(Box::new(AutoAgg {
        vs,
        has_values,
        reader: None,
        roundings: Arc::new(roundings),
        target: spec.buckets,
        format,
        many,
        ords: OrdMap::new(),
        counts: Vec::new(),
        single: SingleState { idx: 0, min: i64::MAX, max: i64::MIN },
        multi: ManyState { idx: Vec::new(), mins: Vec::new(), maxes: Vec::new(), live: Vec::new(), wasted: 0, next_rebucket_at: 1000 },
        record: !subs.is_empty(),
        segments: Vec::new(),
        cur_seg: None,
        searcher: ctx.searcher.clone(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
        finished: false,
        rounding_count: n,
    }))
}

/// `FromSingle` state.
struct SingleState {
    idx: usize,
    min: i64,
    max: i64,
}

/// `FromMany` state, per owning ordinal.
struct ManyState {
    idx: Vec<u8>,
    mins: Vec<i64>,
    maxes: Vec<i64>,
    live: Vec<i32>,
    wasted: i64,
    next_rebucket_at: i64,
}

/// The `(doc, bucket)` pairs collected in one segment (the deferring collector's record).
struct SegEntries {
    seg: usize,
    docs: Vec<u32>,
    ords: Vec<u32>,
    scores: Vec<f32>,
}

struct AutoAgg {
    vs: BoundVs,
    has_values: bool,
    reader: Option<NumReader>,
    roundings: Arc<Vec<RoundingInfo>>,
    target: i32,
    format: DocValueFormat,
    many: bool,
    ords: OrdMap,
    counts: Vec<u64>,
    single: SingleState,
    multi: ManyState,
    record: bool,
    segments: Vec<SegEntries>,
    cur_seg: Option<usize>,
    searcher: Searcher,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
    finished: bool,
    rounding_count: usize,
}

impl AutoAgg {
    fn rounding_index_for(&self, owning: u32) -> usize {
        if self.many { self.multi.idx.get(owning as usize).copied().unwrap_or(0) as usize } else { self.single.idx }
    }

    /// `collectBucket` / `collectExistingBucket`: count and record the pair.
    #[inline]
    fn collect_bucket(&mut self, doc: u32, ord: u32, score: f32, inc: u64) -> Result<()> {
        let o = ord as usize;
        if self.counts.len() <= o {
            self.counts.resize(o + 1, 0);
            if self.counts.len() * 8 > self.tracker.reserved() as usize {
                self.tracker.ensure(self.counts.len() * 8 + self.ords.ram_bytes())?;
            }
        }
        self.counts[o] += inc;
        if let Some(s) = self.cur_seg {
            let e = &mut self.segments[s];
            e.docs.push(doc);
            e.ords.push(ord);
            if self.subs.needs_scores() {
                e.scores.push(score);
            }
            if e.docs.len().is_power_of_two() {
                let total: usize = self.segments.iter().map(|s| s.docs.capacity() * 8 + s.scores.capacity() * 4).sum();
                self.tracker.ensure(total + self.counts.len() * 8 + self.ords.ram_bytes())?;
            }
        }
        Ok(())
    }

    /// `merge(mergeMap, newNumBuckets)`: doc counts summed into the new ordinals and recorded
    /// pairs rewritten.
    fn merge(&mut self, map: &[u32], new_len: usize) {
        let mut counts = vec![0u64; new_len];
        for (old, &new) in map.iter().enumerate() {
            counts[new as usize] += self.counts.get(old).copied().unwrap_or(0);
        }
        self.counts = counts;
        for s in &mut self.segments {
            for o in &mut s.ords {
                *o = map[*o as usize];
            }
        }
    }

    /// Re-rounds every bucket of every owning ordinal with its current rounding.
    fn rebucket_with(&mut self, rounding_of: impl Fn(&Self, u32) -> usize) -> Vec<u32> {
        let old = std::mem::take(&mut self.ords);
        let mut by_owning: Vec<Vec<u32>> = Vec::new();
        for (o, &(owning, _)) in old.keys().iter().enumerate() {
            let w = owning as usize;
            if by_owning.len() <= w {
                by_owning.resize_with(w + 1, Vec::new);
            }
            by_owning[w].push(o as u32);
        }
        let mut map = vec![0u32; old.len()];
        for (owning, list) in by_owning.iter().enumerate() {
            let r = &self.roundings[rounding_of(self, owning as u32)].rounding;
            for &o in list {
                let key = old.key(o).1 as i64;
                map[o as usize] = self.ords.add(owning as u32, r.round(key) as u64).0;
            }
        }
        map
    }

    /// `FromSingle.collectValue`.
    fn collect_single(&mut self, owning: u32, doc: u32, rounded: i64, score: f32, inc: u64) -> Result<()> {
        // `owning` is always 0 here (one owning bucket).
        let (ord, new) = self.ords.add(owning, rounded as u64);
        self.collect_bucket(doc, ord, score, inc)?;
        if new {
            self.increase_single(rounded);
        }
        Ok(())
    }

    /// `FromSingle.increaseRoundingIfNeeded`.
    fn increase_single(&mut self, rounded: i64) {
        let last = self.rounding_count - 1;
        if self.single.idx >= last {
            return;
        }
        self.single.min = self.single.min.min(rounded);
        self.single.max = self.single.max.max(rounded);
        let target = self.target as i64;
        let fits = |s: &Self| {
            let r = &s.roundings[s.single.idx];
            s.ords.len() as i64 <= target * r.max_inner() as i64 && s.single.max - s.single.min <= target * r.max_rough_millis()
        };
        if fits(self) {
            return;
        }
        loop {
            self.single.idx += 1;
            let map = self.rebucket_with(|s, _| s.single.idx);
            let n = self.ords.len();
            self.merge(&map, n);
            if self.single.idx >= last || fits(self) {
                break;
            }
        }
    }

    /// `FromMany.collectValue`; returns the owning ordinal's (possibly new) rounding index.
    fn collect_many(&mut self, owning: u32, idx: usize, doc: u32, rounded: i64, score: f32, inc: u64) -> Result<usize> {
        let (ord, new) = self.ords.add(owning, rounded as u64);
        self.collect_bucket(doc, ord, score, inc)?;
        if !new {
            return Ok(idx);
        }
        let w = owning as usize;
        if self.multi.live.len() <= w {
            self.multi.live.resize(w + 1, 0);
        }
        self.multi.live[w] += 1;
        let estimate = self.multi.live[w];
        Ok(self.increase_many(owning, estimate, rounded, idx))
    }

    /// `FromMany.increaseRoundingIfNeeded`.
    fn increase_many(&mut self, owning: u32, old_estimate: i32, new_key: i64, old: usize) -> usize {
        let last = self.rounding_count - 1;
        if old >= last {
            return old;
        }
        let w = owning as usize;
        if self.multi.mins.len() <= w {
            self.multi.mins.resize(w + 1, i64::MAX);
            self.multi.maxes.resize(w + 1, i64::MIN);
        }
        let min = self.multi.mins[w].min(new_key);
        self.multi.mins[w] = min;
        let max = self.multi.maxes[w].max(new_key);
        self.multi.maxes[w] = max;
        let target = self.target as i64;
        let r = &self.roundings;
        if old_estimate as i64 <= target * r[old].max_inner() as i64 && max - min <= target * r[old].max_rough_millis() {
            return old;
        }
        let old_rough = r[old].rough_millis;
        let mut new = old;
        let mut new_estimate;
        loop {
            new += 1;
            let ratio = old_rough as f64 / r[new].rough_millis as f64;
            new_estimate = (old_estimate as f64 * ratio).ceil() as i32;
            if !(new < last && (new_estimate as i64 > target * r[new].max_inner() as i64 || max - min > target * r[new].max_rough_millis())) {
                break;
            }
        }
        if self.multi.idx.len() <= w {
            self.multi.idx.resize(w + 1, 0);
        }
        self.multi.idx[w] = new as u8;
        let nr = &self.roundings[new].rounding;
        self.multi.mins[w] = nr.round(self.multi.mins[w]);
        self.multi.maxes[w] = nr.round(self.multi.maxes[w]);
        self.multi.wasted += (old_estimate - new_estimate) as i64;
        if self.multi.wasted > self.multi.next_rebucket_at {
            self.rebucket_many();
            self.multi.wasted = 0;
            self.multi.next_rebucket_at *= 2;
        } else {
            self.multi.live[w] = new_estimate;
        }
        new
    }

    /// `FromMany.rebucket()`.
    fn rebucket_many(&mut self) {
        let max_owning = self.ords.keys().iter().map(|k| k.0 as i64).max().unwrap_or(-1);
        let map = self.rebucket_with(|s, o| s.rounding_index_for(o));
        let n = self.ords.len();
        self.merge(&map, n);
        if max_owning >= 0 {
            let mut per = vec![0i32; max_owning as usize + 1];
            for &(o, _) in self.ords.keys() {
                per[o as usize] += 1;
            }
            if self.multi.live.len() < per.len() {
                self.multi.live.resize(per.len(), 0);
            }
            self.multi.live[..per.len()].copy_from_slice(&per);
        }
    }

    /// Replays the recorded pairs into the sub-aggregations (`prepareSelectedBuckets`).
    fn replay(&mut self) -> Result<()> {
        let segs: Vec<SegmentContext> = self.searcher.segments().to_vec();
        let recorded = std::mem::take(&mut self.segments);
        for e in &recorded {
            self.subs.set_segment(&segs[e.seg])?;
            for i in 0..e.docs.len() {
                let score = e.scores.get(i).copied().unwrap_or(f32::NAN);
                self.subs.push(e.docs[i], e.ords[i], score)?;
            }
            self.subs.flush()?;
            self.subs.finish_segment()?;
        }
        Ok(())
    }
}

impl Aggregator for AutoAgg {
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
        let mut vals: smallvec::SmallVec<[i64; 4]> = smallvec::SmallVec::new();
        let res = (|| {
            for i in 0..b.len() {
                let doc = b.docs[i];
                let score = b.score(i);
                vals.clear();
                vals.extend_from_slice(r.longs(doc, score)?);
                if vals.is_empty() {
                    continue;
                }
                let inc = self.doc_counter.count(doc);
                let owning = b.ords[i];
                // Successive values of a document use the rounding in force at that point.
                let mut prev = i64::MIN;
                if self.many {
                    let mut idx = self.rounding_index_for(owning);
                    for &v in &vals {
                        let rounded = self.roundings[idx].rounding.round(v);
                        if rounded == prev {
                            continue;
                        }
                        idx = self.collect_many(owning, idx, doc, rounded, score, inc)?;
                        prev = rounded;
                    }
                } else {
                    for &v in &vals {
                        let rounded = self.roundings[self.single.idx].rounding.round(v);
                        if rounded == prev {
                            continue;
                        }
                        self.collect_single(owning, doc, rounded, score, inc)?;
                        prev = rounded;
                    }
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
            if self.many {
                // `buildAggregations` rebuckets first.
                self.rebucket_many();
            }
            if self.record {
                self.replay()?;
            }
        }
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let mut by_owning: hashbrown::HashMap<u32, Vec<u32>> = hashbrown::HashMap::new();
        for (o, k) in self.ords.keys().iter().enumerate() {
            by_owning.entry(k.0).or_default().push(o as u32);
        }
        let mut per: Vec<Vec<u32>> = Vec::with_capacity(owning.len());
        for &own in owning {
            let mut ords = by_owning.get(&own).cloned().unwrap_or_default();
            ords.sort_by_key(|&o| self.ords.key(o).1 as i64);
            per.push(ords);
        }
        let all: Vec<u32> = per.iter().flatten().copied().collect();
        let mut aggs = self.subs.build(&all)?.into_iter();
        let empty_aggs = self.subs.build_empty();
        let mut out = Vec::with_capacity(owning.len());
        for (i, ords) in per.into_iter().enumerate() {
            let buckets = ords
                .iter()
                .map(|&o| AutoBucket { key: self.ords.key(o).1 as i64, doc_count: self.counts.get(o as usize).copied().unwrap_or(0), aggs: aggs.next().unwrap_or_default() })
                .collect();
            out.push(AggValue::AutoDateHistogram(Box::new(AutoDateHistogram {
                buckets,
                target: self.target,
                rounding_idx: self.rounding_index_for(owning[i]),
                roundings: self.roundings.clone(),
                format: self.format.clone(),
                empty_aggs: empty_aggs.clone(),
                inner_interval: 1,
            })));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        AggValue::AutoDateHistogram(Box::new(AutoDateHistogram {
            buckets: Vec::new(),
            target: self.target,
            rounding_idx: 0,
            roundings: self.roundings.clone(),
            format: self.format.clone(),
            empty_aggs: self.subs.build_empty(),
            inner_interval: 1,
        }))
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

/// An `auto_date_histogram` bucket.
#[derive(Clone, Debug)]
pub struct AutoBucket {
    pub key: i64,
    pub doc_count: u64,
    pub aggs: Aggs,
}

/// `InternalAutoDateHistogram`.
#[derive(Clone, Debug)]
pub struct AutoDateHistogram {
    pub buckets: Vec<AutoBucket>,
    pub target: i32,
    pub rounding_idx: usize,
    pub roundings: Arc<Vec<RoundingInfo>>,
    pub format: DocValueFormat,
    /// `BucketInfo.emptySubAggregations`.
    pub empty_aggs: Aggs,
    /// How many rounding units one bucket spans (`bucketInnerInterval`).
    pub inner_interval: i64,
}

/// `reduceBucket`: the first bucket's key, summed counts, sub-aggregations reduced together.
fn reduce_bucket(group: Vec<AutoBucket>, ctx: &mut ReduceCtx<'_>) -> Result<AutoBucket> {
    let key = group[0].key;
    let doc_count = group.iter().map(|b| b.doc_count).sum();
    let aggs = Aggs::reduce_hashed(group.into_iter().map(|b| b.aggs).collect(), ctx)?;
    Ok(AutoBucket { key, doc_count, aggs })
}

/// `mergeBuckets`: buckets whose keys round together are reduced into one (keyed by the rounded
/// key).
fn merge_buckets(list: Vec<AutoBucket>, rounding: &Rounding, ctx: &mut ReduceCtx<'_>) -> Result<Vec<AutoBucket>> {
    let mut out = Vec::new();
    let mut same: Vec<AutoBucket> = Vec::new();
    let mut key: Option<i64> = None;
    for b in list {
        let rounded = rounding.round(b.key);
        if key.is_some_and(|k| k != rounded) {
            out.push(reduce_bucket(std::mem::take(&mut same), ctx)?);
        }
        key = Some(rounded);
        same.push(AutoBucket { key: rounded, ..b });
    }
    if !same.is_empty() {
        out.push(reduce_bucket(same, ctx)?);
    }
    Ok(out)
}

/// `getAppropriateRounding(minKey, maxKey, roundingIdx, roundings, targetBuckets)`.
fn appropriate_rounding(min_key: i64, max_key: i64, rounding_idx: usize, roundings: &[RoundingInfo], target: i32) -> usize {
    if rounding_idx == roundings.len() - 1 {
        return rounding_idx;
    }
    let target = target as i64;
    let mut current = rounding_idx;
    for (i, r) in roundings.iter().enumerate().skip(rounding_idx + 1) {
        let duration = max_key - min_key;
        let rough = duration / r.rough_millis;
        if rough < target * r.max_inner() as i64 {
            current = i - 1;
            break;
        }
    }
    loop {
        let rounding = &roundings[current].rounding;
        let mut key = min_key;
        let mut required: i64 = 0;
        while key < max_key {
            required += 1;
            key = rounding.next_rounding_value(key);
        }
        current += 1;
        if !(required > target * roundings[current - 1].max_inner() as i64 && current < roundings.len()) {
            break;
        }
    }
    current - 1
}

impl AutoDateHistogram {
    fn shell(&self) -> AutoDateHistogram {
        AutoDateHistogram { buckets: Vec::new(), empty_aggs: self.empty_aggs.clone(), roundings: self.roundings.clone(), format: self.format.clone(), ..*self }
    }

    /// `mergeBucketsIfNeeded`.
    fn merge_if_needed(&self, mut buckets: Vec<AutoBucket>, mut idx: usize, ctx: &mut ReduceCtx<'_>) -> Result<(Vec<AutoBucket>, usize)> {
        let r = &self.roundings;
        while buckets.len() as i64 > self.target as i64 * r[idx].max_inner() as i64 && idx < r.len() - 1 {
            idx += 1;
            buckets = merge_buckets(buckets, &r[idx].rounding, ctx)?;
        }
        Ok((buckets, idx))
    }

    /// `addEmptyBuckets`.
    fn add_empty(&self, list: Vec<AutoBucket>, idx: usize, ctx: &mut ReduceCtx<'_>) -> Result<(Vec<AutoBucket>, usize)> {
        if list.is_empty() {
            return Ok((list, idx));
        }
        let idx = appropriate_rounding(list[0].key, list[list.len() - 1].key, idx, &self.roundings, self.target);
        let rounding = &self.roundings[idx].rounding;
        let list = merge_buckets(list, rounding, ctx)?;
        let empty = Aggs::reduce_lists(vec![self.empty_aggs.clone()], ctx)?;
        let mut out = Vec::with_capacity(list.len());
        let mut last: Option<i64> = None;
        for b in list {
            if let Some(l) = last {
                let mut key = rounding.next_rounding_value(l);
                while key < b.key {
                    out.push(AutoBucket { key, doc_count: 0, aggs: empty.clone() });
                    key = rounding.next_rounding_value(key);
                }
            }
            last = Some(b.key);
            out.push(b);
        }
        Ok((out, idx))
    }

    /// `maybeMergeConsecutiveBuckets`: `(buckets, inner interval)`.
    fn merge_consecutive(&self, buckets: Vec<AutoBucket>, idx: usize, ctx: &mut ReduceCtx<'_>) -> Result<(Vec<AutoBucket>, i64)> {
        let len = buckets.len();
        if len <= self.target as usize {
            return Ok((buckets, 1));
        }
        let info = &self.roundings[idx];
        let Some(&interval) = info.inner.iter().find(|&&i| len.div_ceil(i as usize) <= self.target as usize) else {
            return Ok((buckets, 1));
        };
        let interval = interval as usize;
        let rounding = &info.rounding;
        let mut out = Vec::new();
        let mut same: Vec<AutoBucket> = Vec::new();
        let mut key = rounding.round(buckets[0].key);
        for (i, b) in buckets.into_iter().enumerate() {
            if i % interval == 0 && !same.is_empty() {
                out.push(reduce_bucket(std::mem::take(&mut same), ctx)?);
                key = rounding.round(b.key);
            }
            same.push(AutoBucket { key, ..b });
        }
        if !same.is_empty() {
            out.push(reduce_bucket(same, ctx)?);
        }
        Ok((out, interval as i64))
    }
}

impl Internal for AutoDateHistogram {
    fn type_name(&self) -> &'static str {
        "auto_date_histogram"
    }
    fn class_name(&self) -> &'static str {
        "InternalAutoDateHistogram"
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let first = list[0].shell();
        let mut idx = 0;
        // `reduceBuckets`: a priority queue over the shards' key-ordered lists, grouped by the
        // key under the coarsest shard rounding.
        struct Cursor {
            cur: AutoBucket,
            rest: std::vec::IntoIter<AutoBucket>,
        }
        let less = |a: &Cursor, b: &Cursor| a.cur.key < b.cur.key;
        let mut pq = LucenePq::new(list.len());
        for h in list {
            idx = idx.max(h.rounding_idx);
            let mut rest = h.buckets.into_iter();
            if let Some(cur) = rest.next() {
                pq.add(Cursor { cur, rest }, &less);
            }
        }
        let rounding = &first.roundings[idx].rounding;
        let mut buckets = Vec::new();
        let mut group: Vec<AutoBucket> = Vec::new();
        let mut key = None;
        while let Some(top) = pq.top_mut() {
            let rounded = rounding.round(top.cur.key);
            if key.is_some_and(|k| k != rounded) {
                buckets.push(reduce_bucket(std::mem::take(&mut group), ctx)?);
            }
            key = Some(rounded);
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
        }
        let (mut buckets, mut idx) = first.merge_if_needed(buckets, idx, ctx)?;
        let mut inner = 1;
        if ctx.is_final() {
            (buckets, idx) = first.add_empty(buckets, idx, ctx)?;
            (buckets, idx) = first.merge_if_needed(buckets, idx, ctx)?;
            (buckets, inner) = first.merge_consecutive(buckets, idx, ctx)?;
        }
        ctx.consume_buckets(buckets.len())?;
        Ok(AutoDateHistogram { buckets, rounding_idx: idx, inner_interval: inner, ..first })
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        let f = &self.format;
        let buckets = self
            .buckets
            .iter()
            .map(|b| {
                let mut o = Json::obj();
                if !f.is_raw() {
                    o.push("key_as_string", Json::Str(f.long_string(b.key)));
                }
                o.push("key", Json::Int(b.key));
                o.push("doc_count", Json::from(b.doc_count));
                b.aggs.render_into(&mut o, rc);
                o
            })
            .collect();
        out.push("buckets", Json::Arr(buckets));
        out.push("interval", Json::Str(format!("{}{}", self.inner_interval, self.roundings[self.rounding_idx].abbreviation)));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }
    fn ram_bytes(&self) -> usize {
        self.buckets.iter().map(|b| 48 + b.aggs.ram_bytes()).sum::<usize>() + 128
    }
}

impl MultiBuckets for AutoDateHistogram {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        Json::Int(self.buckets[i].key)
    }
    fn key_string(&self, i: usize) -> String {
        self.format.long_string(self.buckets[i].key)
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
        let mut slots: Vec<Option<AutoBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
    fn numeric_key(&self, i: usize) -> Option<f64> {
        Some(self.buckets[i].key as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appropriate_rounding_counts_buckets() {
        let r = build_roundings(&TimeZone::default(), None).unwrap();
        // One day of data, 10 buckets: hours (12 * 10 >= 24 buckets).
        assert_eq!(appropriate_rounding(0, 86_400_000, 0, &r, 10), 2);
        // Ten years: years.
        assert_eq!(appropriate_rounding(0, 10 * 365 * 86_400_000, 0, &r, 10), 5);
        let m = build_roundings(&TimeZone::default(), Some("month")).unwrap();
        assert_eq!(m.len(), 2);
    }
}
