//! `random_sampler`: a top-level single-bucket aggregation that feeds its sub-aggregations a
//! random sample of the matching documents, then scales their counts back up.
//!
//! The sample is Elasticsearch's `RandomSamplingQuery`: per segment, a `SplittableRandom`
//! seeded with `BitMixer.mix(shardSeed ^ seed)` drives `FastGeometric` gaps between sampled doc
//! ids (a table-driven fast log2 in `float` arithmetic), intersected with the query's matches —
//! reproduced bit for bit, so a given `seed` and `shard_seed` sample the same documents as
//! Elasticsearch. `doc_count` is the sampled count; at the final reduce every sub-aggregation is
//! `finalizeSampling`-scaled (counts `Math.round(v / p)`, sums `v / p`, means and extrema kept).
//!
//! Without `shard_seed`, the shard's seed is `ShardId.hashCode()`: the index name, the index
//! UUID ([`FieldLookup::index_uuid`](crate::FieldLookup::index_uuid)) and the shard number.

use std::sync::OnceLock;

use search_codec::NO_MORE_DOCS;
use search_index::{QueryRef, ScoreMode, Searcher, SegmentContext};

use crate::collect::{Aggregator, Batch, BindCtx, Subs, single_bucket_validate_sort};
use crate::error::{AggError, Result};
use crate::format::java_round;
use crate::internal::{AggValue, Aggs};
use crate::json::java_double;
use crate::request::{AggDef, PathElement};
use crate::util::java_string_hash_code;
use crate::values::DocCounter;
use crate::xparser::{Field, Tokens, XParser};

use super::single::{SingleBucket, SingleKind};

/// The settings a `random_sampler` result carries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SamplerInfo {
    pub seed: i32,
    pub shard_seed: Option<i32>,
    pub probability: f64,
}

/// `SamplingContext` of a sampling `random_sampler` (probability below 1), as aggregations
/// below it see it: what their own background counts must sample too.
#[derive(Clone, Copy, Debug)]
pub struct Sampling {
    pub probability: f64,
    pub seed: i32,
    /// `RandomSamplingQuery.hash`: the shard seed.
    pub hash: i32,
}

impl Sampling {
    /// `searcher.count(query ∩ RandomSamplingQuery)`: live matches that the sampler keeps.
    pub fn count(&self, searcher: &Searcher, query: &QueryRef) -> Result<u64> {
        let weight = searcher.create_weight(query, ScoreMode::CompleteNoScores)?;
        let mut n = 0u64;
        for seg in searcher.segments() {
            let Some(mut sc) = weight.scorer(seg)? else { continue };
            let mut it = SamplingIter::new(seg.max_doc() as i32, self.probability, self.hash ^ self.seed);
            loop {
                let d = sc.next();
                if d == NO_MORE_DOCS {
                    break;
                }
                if seg.is_live(d) && it.advance(d as i32) == d as i32 {
                    n += 1;
                }
            }
        }
        Ok(n)
    }
}

/// A parsed `random_sampler`.
#[derive(Clone, Debug)]
pub struct RandomSamplerSpec {
    /// `0` when not set.
    pub probability: f64,
    pub seed: i32,
    pub shard_seed: Option<i32>,
}

const FIELDS: &[Field] = &[Field::new("seed", Tokens::INT), Field::new("shard_seed", Tokens::INT), Field::new("probability", Tokens::FLOAT)];

/// A random `int` (`Randomness.get().nextInt()`, the default seed).
fn random_seed() -> i32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64));
    h.finish() as i32
}

pub fn parse(p: &mut XParser<'_>) -> Result<RandomSamplerSpec> {
    let mut spec = RandomSamplerSpec { probability: 0.0, seed: random_seed(), shard_seed: None };
    p.object("random_sampler", FIELDS, |p, i| {
        match i {
            0 => spec.seed = p.int_value()?,
            1 => spec.shard_seed = Some(p.int_value()?),
            _ => {
                let v = p.double_value()?;
                if v <= 0.0 {
                    return Err(AggError::illegal_argument(format!("[probability] must be greater than 0.0, was [{}]", java_double(v))));
                }
                if v > 0.5 && v != 1.0 {
                    return Err(AggError::illegal_argument(format!(
                        "[probability] must be between 0.0 and 0.5 or exactly 1.0, was [{}]",
                        java_double(v)
                    )));
                }
                spec.probability = v;
            }
        }
        Ok(())
    })?;
    Ok(spec)
}

/// Aggregations whose builders don't `supportsSampling()`.
fn supports_sampling(type_name: &str) -> bool {
    !matches!(
        type_name,
        "boxplot"
            | "string_stats"
            | "variable_width_histogram"
            | "missing"
            | "sampler"
            | "diversified_sampler"
            | "global"
            | "ip_prefix"
            | "nested"
            | "reverse_nested"
            | "children"
            | "parent"
            | "random_sampler"
            | "significant_text"
            | "geo_line"
            | "time_series"
    )
}

/// `recursivelyCheckSubAggs`: depth first, each aggregation before its own sub-aggregations.
fn check_subs(name: &str, subs: &[std::sync::Arc<AggDef>]) -> Result<()> {
    for d in subs {
        if !supports_sampling(d.type_name) {
            return Err(AggError::illegal_argument(format!(
                "[random_sampler] aggregation [{name}] does not support sampling [{}] aggregation [{}]",
                d.type_name, d.name
            )));
        }
        check_subs(name, &d.subs)?;
    }
    Ok(())
}

/// `ShardId.hashCode()`: `31 * (31 * name.hashCode() + uuid.hashCode()) + shard` (the UUID term
/// is left out when the index has none).
fn shard_seed(index: &str, uuid: Option<&str>, shard: u32) -> i32 {
    let mut h = java_string_hash_code(index);
    if let Some(u) = uuid {
        h = h.wrapping_mul(31).wrapping_add(java_string_hash_code(u));
    }
    h.wrapping_mul(31).wrapping_add(shard as i32)
}

/// Binds `random_sampler`.
pub fn create(def: &AggDef, spec: &RandomSamplerSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let name = &def.name;
    if !ctx.is_top_level() {
        return Err(AggError::illegal_argument(format!("[random_sampler] aggregation [{name}] cannot have a parent aggregation")));
    }
    if def.subs.is_empty() {
        return Err(AggError::illegal_argument(format!("[random_sampler] aggregation [{name}] must have sub-aggregations")));
    }
    if spec.probability == 0.0 {
        return Err(AggError::illegal_argument(format!("[random_sampler] aggregation [{name}] must have [probability] set")));
    }
    check_subs(name, &def.subs)?;
    let hash = spec.shard_seed.unwrap_or_else(|| {
        let (shard, _) = ctx.lookup.shard_id();
        shard_seed(ctx.lookup.index_name(), ctx.lookup.index_uuid(), shard)
    });
    let sampling = (spec.probability < 1.0).then_some(Sampling { probability: spec.probability, seed: spec.seed, hash });
    let saved = std::mem::replace(&mut ctx.sampling, sampling);
    let subs = Subs::bind(def, ctx);
    ctx.sampling = saved;
    let subs = subs?;
    Ok(Box::new(SamplerAgg {
        info: SamplerInfo { seed: spec.seed, shard_seed: spec.shard_seed, probability: spec.probability },
        hash,
        sampler: None,
        count: 0,
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
    }))
}

struct SamplerAgg {
    info: SamplerInfo,
    /// `RandomSamplingQuery.hash`: the shard seed.
    hash: i32,
    /// The current segment's sampled-doc iterator (`None`: every doc is sampled).
    sampler: Option<SamplingIter>,
    count: u64,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
}

impl Aggregator for SamplerAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        self.sampler = (self.info.probability < 1.0).then(|| SamplingIter::new(seg.max_doc() as i32, self.info.probability, self.hash ^ self.info.seed));
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        for i in 0..b.len() {
            let doc = b.docs[i];
            if let Some(s) = &mut self.sampler
                && s.advance(doc as i32) != doc as i32
            {
                continue;
            }
            self.count += self.doc_counter.count(doc);
            self.subs.push(doc, 0, b.score(i))?;
        }
        Ok(())
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let aggs = self.subs.build(owning)?;
        Ok(owning
            .iter()
            .zip(aggs)
            .map(|(&o, aggs)| {
                AggValue::Single(Box::new(SingleBucket {
                    sampler: Some(self.info),
                    kind: SingleKind::RandomSampler,
                    doc_count: if o == 0 { self.count } else { 0 },
                    aggs,
                }))
            })
            .collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::Single(Box::new(SingleBucket { sampler: Some(self.info), kind: SingleKind::RandomSampler, doc_count: 0, aggs: self.subs.build_empty() }))
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores()
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        single_bucket_validate_sort(name, &self.subs, key, rest)
    }
}

/// HPPC `BitMixer.mix32`.
fn mix32_hppc(k: i32) -> i32 {
    let mut k = k as u32;
    k = (k ^ (k >> 16)).wrapping_mul(0x85eb_ca6b);
    k = (k ^ (k >> 13)).wrapping_mul(0xc2b2_ae35);
    (k ^ (k >> 16)) as i32
}

/// `java.util.SplittableRandom` (`nextInt` only).
struct SplittableRandom {
    seed: u64,
}

impl SplittableRandom {
    const GOLDEN_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

    fn new(seed: i64) -> SplittableRandom {
        SplittableRandom { seed: seed as u64 }
    }

    fn next_int(&mut self) -> i32 {
        self.seed = self.seed.wrapping_add(Self::GOLDEN_GAMMA);
        let mut z = self.seed;
        z = (z ^ (z >> 33)).wrapping_mul(0x62a9_d9ed_7997_05f5);
        ((z ^ (z >> 28)).wrapping_mul(0xcb24_d0a5_c88c_35b3) >> 32) as i32
    }
}

const LOG_BITS: u32 = 13;
const LOG_SHIFT: u32 = 23 - LOG_BITS;

/// `FastGeometric.LOOKUP_TABLE_FOR_FAST_LOG_2`.
fn log2_table() -> &'static [f32] {
    static TABLE: OnceLock<Vec<f32>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let n = 1usize << (LOG_BITS + 1);
        let dx = 1u32 << LOG_SHIFT;
        let mut x = 0x3F80_0000u32 + dx / 2;
        let log2 = std::f64::consts::LN_2 as f32;
        let mut t = Vec::with_capacity(n);
        for _ in 0..n {
            t.push((f32::from_bits(x) as f64).ln() as f32 / log2);
            x = x.wrapping_add(dx);
        }
        t
    })
}

/// `FastGeometric.fastLog2`.
#[inline]
fn fast_log2(x: f32) -> f32 {
    let bits = x.to_bits();
    let log2x = ((bits >> 23) & 0xFF) as i32 - 127;
    let mantissa = bits & 0x7F_FFFF;
    log2_table()[(mantissa >> LOG_SHIFT) as usize] + log2x as f32
}

/// `FastGeometric`: geometric gaps from 8, 16 or 32 random bits each.
struct FastGeometric {
    rng: SplittableRandom,
    current: usize,
    samples: [i32; 4],
    norm: f32,
    rng_range: f32,
    log2_rng_range: f32,
    bias: f32,
    mask: u32,
    bits_per_sample: u32,
    need: usize,
}

impl FastGeometric {
    fn new(rng: SplittableRandom, p: f64) -> FastGeometric {
        const BITS: [i32; 3] = [8, 16, 32];
        const RANGES: [f32; 3] = [256.0, 65536.0, 4_294_967_296.0];
        // -0.49294917f / 256, 4.7501511f / 65536, 31.557305f / 2^32.
        let biases: [f32; 3] =
            [f32::from_bits(0xbefc_63d5) / 256.0, f32::from_bits(0x4098_013d) / 65536.0, f32::from_bits(0x41fc_755c) / 4_294_967_296.0];
        let bits = (1.5 * ((0.01f64.ln() / (1.0 - p).ln()) - 1.0).ln() / 2f64.ln()) as i32;
        let pos = match BITS.binary_search(&bits) {
            Ok(i) => i,
            Err(i) => i.min(BITS.len() - 1),
        };
        let bits_per_sample = BITS[pos] as u32;
        let need = (32 / bits_per_sample) as usize;
        // Java masks the shift to 5 bits: `>>> -1` is `>>> 31`.
        let mask = (i32::MAX as u32).wrapping_shr(31u32.wrapping_sub(bits_per_sample));
        FastGeometric {
            rng,
            current: need - 1,
            samples: [0; 4],
            norm: (2f64.ln() / (1.0 - p).ln()) as f32,
            rng_range: RANGES[pos],
            log2_rng_range: fast_log2(RANGES[pos]),
            bias: biases[pos],
            mask,
            bits_per_sample,
            need,
        }
    }

    #[inline]
    fn next(&mut self) -> i32 {
        self.current += 1;
        if self.current == self.need {
            self.generate();
            self.current = 0;
        }
        self.samples[self.current] + 1
    }

    fn generate(&mut self) {
        let rn = self.rng.next_int();
        if self.need == 1 {
            let u = 0.5f32 * self.rng_range + rn as f32 + 0.5f32;
            self.samples[0] = (self.norm * (fast_log2(u) - self.log2_rng_range)) as i32;
        } else {
            let mut shift = self.bits_per_sample;
            let mut i = 0;
            while shift <= 32 {
                // Java's `>>>` takes the shift modulo 32.
                let u = ((rn as u32).wrapping_shr(shift) & self.mask) as f32 + 0.5f32;
                self.samples[i] = (self.norm * (fast_log2(u) - self.log2_rng_range + self.bias)) as i32;
                i += 1;
                shift += self.bits_per_sample;
            }
        }
    }
}

/// `RandomSamplingQuery.RandomSamplingIterator` of one segment.
struct SamplingIter {
    max_doc: i32,
    doc: i32,
    dist: FastGeometric,
}

impl SamplingIter {
    fn new(max_doc: i32, p: f64, mixed_seed: i32) -> SamplingIter {
        let rng = SplittableRandom::new(mix32_hppc(mixed_seed) as i64);
        SamplingIter { max_doc, doc: -1, dist: FastGeometric::new(rng, p) }
    }

    /// The first sampled doc at or after `target` (`i32::MAX` when exhausted).
    #[inline]
    fn advance(&mut self, target: i32) -> i32 {
        while self.doc < target && self.doc < self.max_doc {
            self.doc = self.doc.wrapping_add(self.dist.next());
        }
        if self.doc >= self.max_doc {
            self.doc = i32::MAX;
        }
        self.doc
    }
}

/// `SamplingContext.scaleUp(long)`.
#[inline]
fn scale_count(v: u64, p: f64) -> u64 {
    java_round(v as f64 * (1.0 / p)).max(0) as u64
}

#[inline]
fn scale_long(v: i64, p: f64) -> i64 {
    java_round(v as f64 * (1.0 / p))
}

/// `InternalAggregations.finalizeSampling`.
pub fn finalize_aggs(aggs: &mut Aggs, p: f64) -> Result<()> {
    if p >= 1.0 {
        return Ok(());
    }
    for a in &mut aggs.list {
        finalize_value(&a.name, &mut a.value, p)?;
    }
    Ok(())
}

/// `InternalAggregation.finalizeSampling` of each result type.
fn finalize_value(name: &str, v: &mut AggValue, p: f64) -> Result<()> {
    match v {
        AggValue::Sum(s) => s.sum /= p,
        AggValue::ValueCount(c) => c.count = scale_count(c.count, p),
        AggValue::Stats(s) => {
            s.count = scale_count(s.count, p);
            s.sum /= p;
        }
        AggValue::ExtendedStats(s) => {
            s.count = scale_count(s.count, p);
            s.sum /= p;
            s.sum_of_sqrs /= p;
        }
        AggValue::Rate(r) => r.sum /= p,
        AggValue::GeoCentroid(c) => c.count = scale_count(c.count, p),
        AggValue::CartesianCentroid(c) => c.count = scale_count(c.count, p),
        AggValue::MatrixStats(m) => m.finalize_sampling(p),
        AggValue::Terms(t) => {
            if matches!(t.kind, crate::bucket::terms::TermsKind::Unmapped) {
                return Ok(());
            }
            let show = t.cfg.show_term_doc_count_error;
            for b in &mut t.buckets {
                b.doc_count = scale_count(b.doc_count, p);
                b.doc_count_error = if show { scale_long(b.doc_count_error, p) } else { 0 };
                finalize_aggs(&mut b.aggs, p)?;
            }
            t.doc_count_error = t.doc_count_error.map(|e| scale_long(e, p));
            t.other_doc_count = scale_long(t.other_doc_count, p);
        }
        AggValue::RareTerms(t) => {
            for b in &mut t.buckets {
                b.doc_count = scale_count(b.doc_count, p);
                finalize_aggs(&mut b.aggs, p)?;
            }
        }
        AggValue::SignificantTerms(t) => {
            t.subset_size = scale_count(t.subset_size, p);
            t.superset_size = scale_count(t.superset_size, p);
            for b in &mut t.buckets {
                b.subset_df = scale_count(b.subset_df, p);
                b.superset_df = scale_count(b.superset_df, p);
                finalize_aggs(&mut b.aggs, p)?;
            }
        }
        AggValue::Histogram(h) => {
            for b in &mut h.buckets {
                b.doc_count = scale_count(b.doc_count, p);
                finalize_aggs(&mut b.aggs, p)?;
            }
        }
        AggValue::DateHistogram(h) => {
            for b in &mut h.buckets {
                b.doc_count = scale_count(b.doc_count, p);
                finalize_aggs(&mut b.aggs, p)?;
            }
        }
        AggValue::AutoDateHistogram(h) => {
            for b in &mut h.buckets {
                b.doc_count = scale_count(b.doc_count, p);
                finalize_aggs(&mut b.aggs, p)?;
            }
        }
        AggValue::Range(r) => {
            for b in &mut r.buckets {
                b.doc_count = scale_count(b.doc_count, p);
                finalize_aggs(&mut b.aggs, p)?;
            }
        }
        AggValue::Filters(f) => {
            for b in &mut f.buckets {
                b.doc_count = scale_count(b.doc_count, p);
                finalize_aggs(&mut b.aggs, p)?;
            }
        }
        AggValue::AdjacencyMatrix(f) => {
            for b in &mut f.buckets {
                b.doc_count = scale_count(b.doc_count, p);
                finalize_aggs(&mut b.aggs, p)?;
            }
        }
        AggValue::Composite(c) => {
            for b in &mut c.buckets {
                b.doc_count = scale_count(b.doc_count, p);
                finalize_aggs(&mut b.aggs, p)?;
            }
        }
        AggValue::GeoGrid(g) => {
            for b in &mut g.buckets {
                b.doc_count = scale_count(b.doc_count, p);
                finalize_aggs(&mut b.aggs, p)?;
            }
        }
        AggValue::Single(s) if s.kind == SingleKind::Filter => {
            s.doc_count = scale_count(s.doc_count, p);
            finalize_aggs(&mut s.aggs, p)?;
        }
        // Means, extrema, sketches and hits are kept as they are.
        AggValue::Avg(_)
        | AggValue::Min(_)
        | AggValue::Max(_)
        | AggValue::WeightedAvg(_)
        | AggValue::TTest(_)
        | AggValue::Cardinality(_)
        | AggValue::Percentiles(_)
        | AggValue::MedianAbsoluteDeviation(_)
        | AggValue::TopHits(_)
        | AggValue::TopMetrics(_)
        | AggValue::GeoBounds(_)
        | AggValue::CartesianBounds(_)
        | AggValue::ScriptedMetric(_)
        | AggValue::SimpleValue(_)
        | AggValue::SimpleLongValue(_)
        | AggValue::Derivative(_)
        | AggValue::BucketMetricValue(_)
        | AggValue::StatsBucket(_)
        | AggValue::ExtendedStatsBucket(_)
        | AggValue::PercentilesBucket(_) => {}
        other => {
            return Err(AggError::new(
                500,
                "unsupported_operation_exception",
                format!("{} aggregation [{name}] does not support sampling", other.type_name()),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `new ShardId("logs-2024", uuid, 3).hashCode()`.
    #[test]
    fn shard_seed_is_shard_id_hash() {
        assert_eq!(shard_seed("logs-2024", Some("4uC0Zo7DSq2LUXlmkz2Bzg"), 3), -1776928279);
        assert_eq!(shard_seed("logs-2024", None, 3), 927909221);
    }

    /// Doc ids sampled out of 100000 (`RandomSamplingIterator` with `SplittableRandom` and
    /// HPPC's mixer), from Elasticsearch 9.5.5's own classes.
    #[test]
    fn samples_like_elasticsearch() {
        assert_eq!(mix32_hppc(12345), 1011272156);
        assert_eq!(mix32_hppc(-7), 214441827);
        let mut r = SplittableRandom::new(99);
        assert_eq!([r.next_int(), r.next_int(), r.next_int()], [-801679754, -1217134426, -81515647]);
        let cases: [(f64, i32, i32, usize, i64, [i32; 6]); 6] = [
            (0.5, 42, 7, 50078, 2503614662, [1, 2, 6, 7, 12, 13]),
            (0.1, -3, 1000, 10141, 504853024, [10, 15, 38, 39, 47, 49]),
            (0.01, 0, 0, 986, 50126379, [164, 312, 407, 409, 460, 660]),
            (0.001, 42, 7, 88, 4088746, [534, 1782, 2439, 2811, 3708, 4635]),
            (0.3, -3, 1000, 29946, 1494110091, [3, 5, 12, 13, 16, 17]),
            (0.05, 0, 0, 4966, 247614666, [32, 61, 80, 81, 91, 131]),
        ];
        for (p, seed, hash, n, sum, first) in cases {
            let mut it = SamplingIter::new(100_000, p, hash ^ seed);
            let mut docs = Vec::new();
            let mut d = it.advance(0);
            while d != i32::MAX {
                docs.push(d);
                d = it.advance(d + 1);
            }
            assert_eq!(docs.len(), n, "p={p}");
            assert_eq!(docs.iter().map(|&d| d as i64).sum::<i64>(), sum, "p={p}");
            assert_eq!(&docs[..6], &first, "p={p}");
        }
    }
}
