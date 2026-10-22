//! Single-pass numeric metrics: `avg`, `sum`, `min`, `max`, `value_count`, `stats`,
//! `extended_stats`, `weighted_avg` and `rate`'s result type.
//!
//! One aggregator serves the first six: it keeps only the per-bucket arrays its kind needs
//! (counts, Kahan sums and their compensations, mins, maxes, sums of squares) and runs a
//! monomorphic loop per batch. Sums use Elasticsearch's `CompensatedSum` exactly as its
//! aggregators do (values of a doc added in ascending order, the compensation kept per bucket
//! and dropped when the shard result is built), so totals match to the last bit. Min and max
//! use Java's `Math.min`/`Math.max` (NaN wins).

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, grow, multi_value_validate_sort, single_value_validate_sort};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{CompensatedSum, Internal, Prop, ReduceCtx, RenderCtx, java_list, single_value_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::values::{BoundVs, BytesReader, GeoReader, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

/// Java's `Math.min` (NaN if either is NaN; -0.0 < 0.0).
#[inline]
pub fn java_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { a } else { b };
    }
    if a <= b { a } else { b }
}

/// Java's `Math.max`.
#[inline]
pub fn java_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { b } else { a };
    }
    if a >= b { a } else { b }
}

/// Which simple metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimpleKind {
    Avg,
    Sum,
    Min,
    Max,
    ValueCount,
    Stats,
    ExtendedStats,
}

impl SimpleKind {
    pub fn type_name(self) -> &'static str {
        match self {
            SimpleKind::Avg => "avg",
            SimpleKind::Sum => "sum",
            SimpleKind::Min => "min",
            SimpleKind::Max => "max",
            SimpleKind::ValueCount => "value_count",
            SimpleKind::Stats => "stats",
            SimpleKind::ExtendedStats => "extended_stats",
        }
    }
}

/// A parsed simple metric.
#[derive(Clone, Debug)]
pub struct SimpleSpec {
    pub kind: SimpleKind,
    pub vs: VsSpec,
    /// `extended_stats` `sigma` (2.0).
    pub sigma: f64,
}

/// Parses `avg`, `sum`, `min`, `max`, `value_count`, `stats`, `extended_stats`.
pub fn parse(p: &mut XParser<'_>, kind: SimpleKind, name: &str) -> Result<SimpleSpec> {
    let opts = VsOpts::new(true, kind != SimpleKind::ValueCount, false);
    let mut fields = VsSpec::fields(opts);
    if kind == SimpleKind::ExtendedStats {
        fields.push(Field::new("sigma", Tokens::FLOAT));
    }
    let mut vs = VsSpec::default();
    let mut sigma = 2.0;
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    p.object(kind.type_name(), &fields, |p, i| {
        let fname = names[i];
        if vs.parse_field(p, fname)? {
            return Ok(());
        }
        let s = p.double_value()?;
        if s < 0.0 {
            return Err(AggError::illegal_argument(format!(
                "[sigma] must be greater than or equal to 0. Found [{}] in [{name}]",
                crate::json::java_double(s)
            )));
        }
        sigma = s;
        Ok(())
    })?;
    vs.check_required(opts)?;
    Ok(SimpleSpec { kind, vs, sigma })
}

/// Binds a simple metric.
pub fn create(def: &AggDef, spec: &SimpleSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let default = if spec.kind == SimpleKind::ValueCount { VsType::Keyword } else { VsType::Numeric };
    let vs = bind(&spec.vs, ctx.lookup, default, false)?;
    let numeric = vs.vs_type.is_numeric();
    let pre_aggregated = matches!(vs.vs_type, VsType::Histogram | VsType::AggregateMetric | VsType::ExponentialHistogram) && !vs.unmapped;
    if pre_aggregated {
        // Histogram- and aggregate-metric-backed `avg`, `sum`, `min`, `max`, `value_count` only.
        if matches!(spec.kind, SimpleKind::Stats | SimpleKind::ExtendedStats) {
            return Err(vs.unsupported(spec.kind.type_name()));
        }
    } else if (spec.kind != SimpleKind::ValueCount || matches!(vs.vs_type, VsType::CartesianPoint | VsType::GeoShape | VsType::CartesianShape)) && !numeric && !vs.unmapped {
        return Err(vs.unsupported(spec.kind.type_name()));
    }
    Ok(Box::new(SimpleAgg {
        kind: spec.kind,
        sigma: spec.sigma,
        format: vs.format.clone(),
        reader: Reader::None,
        vs,
        counts: Vec::new(),
        sums: Vec::new(),
        comps: Vec::new(),
        mins: Vec::new(),
        maxs: Vec::new(),
        sqs: Vec::new(),
        sq_comps: Vec::new(),
        allocated: 0,
        whole_segments: ctx.whole_segments() && matches!(spec.kind, SimpleKind::Min | SimpleKind::Max | SimpleKind::ValueCount),
        segment_done: false,
        batch: Default::default(),
        dbuf: Vec::new(),
        tracker: ctx.tracker(def),
    }))
}

enum Reader {
    None,
    Num(NumReader),
    Bytes(BytesReader),
    Geo(GeoReader),
    Histogram(crate::histogram_field::HistogramReader),
    AggregateMetric(Box<crate::histogram_field::AggregateMetricReader>),
    ExpHistogram(Box<crate::exp_histogram::ExpHistogramReader>),
}

struct SimpleAgg {
    kind: SimpleKind,
    sigma: f64,
    format: DocValueFormat,
    vs: BoundVs,
    reader: Reader,
    counts: Vec<u64>,
    sums: Vec<f64>,
    comps: Vec<f64>,
    mins: Vec<f64>,
    maxs: Vec<f64>,
    sqs: Vec<f64>,
    sq_comps: Vec<f64>,
    /// Ordinals the arrays hold (the fast check of [`SimpleAgg::ensure`]).
    allocated: usize,
    /// Top-level under a query matching every doc: `min`/`max`/`value_count` read whole
    /// segments from the doc-values skip index.
    whole_segments: bool,
    segment_done: bool,
    /// Batch-decoded doc values and their decoded doubles.
    batch: crate::values::NumBatch,
    dbuf: Vec<f64>,
    tracker: MemTracker,
}

impl SimpleAgg {
    fn needs(&self) -> (bool, bool, bool, bool, bool) {
        // (counts, sums, mins, maxs, squares)
        match self.kind {
            SimpleKind::Avg => (true, true, false, false, false),
            SimpleKind::Sum => (false, true, false, false, false),
            SimpleKind::Min => (false, false, true, false, false),
            SimpleKind::Max => (false, false, false, true, false),
            SimpleKind::ValueCount => (true, false, false, false, false),
            SimpleKind::Stats => (true, true, true, true, false),
            SimpleKind::ExtendedStats => (true, true, true, true, true),
        }
    }

    #[inline]
    fn ensure(&mut self, ord: u32) -> Result<()> {
        if (ord as usize) < self.allocated {
            return Ok(());
        }
        self.grow_to(ord)
    }

    fn grow_to(&mut self, ord: u32) -> Result<()> {
        let (c, s, mi, ma, sq) = self.needs();
        let before = self.capacity();
        if c {
            grow(&mut self.counts, ord, 0);
        }
        if s {
            grow(&mut self.sums, ord, 0.0);
            grow(&mut self.comps, ord, 0.0);
        }
        if mi {
            grow(&mut self.mins, ord, f64::INFINITY);
        }
        if ma {
            grow(&mut self.maxs, ord, f64::NEG_INFINITY);
        }
        if sq {
            grow(&mut self.sqs, ord, 0.0);
            grow(&mut self.sq_comps, ord, 0.0);
        }
        if self.capacity() != before {
            let bytes = self.capacity() * 8;
            self.tracker.ensure(bytes)?;
        }
        self.allocated = [(c, self.counts.len()), (s, self.sums.len()), (mi, self.mins.len()), (ma, self.maxs.len()), (sq, self.sqs.len())]
            .iter()
            .filter(|x| x.0)
            .map(|x| x.1)
            .min()
            .unwrap_or(0);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.counts.len() + self.sums.len() + self.comps.len() + self.mins.len() + self.maxs.len() + self.sqs.len() + self.sq_comps.len()
    }

    /// [`add_values`](Self::add_values) of a doc with one value.
    #[inline]
    fn add_one(&mut self, ord: usize, v: f64) {
        match self.kind {
            SimpleKind::Min => self.mins[ord] = java_min(self.mins[ord], v),
            SimpleKind::Max => self.maxs[ord] = java_max(self.maxs[ord], v),
            SimpleKind::Sum => {
                let mut k = CompensatedSum::new(self.sums[ord], self.comps[ord]);
                k.add(v);
                self.sums[ord] = k.value;
                self.comps[ord] = k.delta;
            }
            SimpleKind::Avg => {
                self.counts[ord] += 1;
                let mut k = CompensatedSum::new(self.sums[ord], self.comps[ord]);
                k.add(v);
                self.sums[ord] = k.value;
                self.comps[ord] = k.delta;
            }
            _ => self.add_values(ord, &[v]),
        }
    }

    #[inline]
    fn add_values(&mut self, ord: usize, vals: &[f64]) {
        if !self.counts.is_empty() {
            self.counts[ord] += vals.len() as u64;
        }
        if !self.sums.is_empty() {
            let mut k = CompensatedSum::new(self.sums[ord], self.comps[ord]);
            for &v in vals {
                k.add(v);
            }
            self.sums[ord] = k.value;
            self.comps[ord] = k.delta;
        }
        if !self.mins.is_empty() {
            let mut m = self.mins[ord];
            if self.kind == SimpleKind::Min {
                // MultiValueMode.MIN: the doc's smallest value only.
                m = java_min(m, vals[0]);
            } else {
                for &v in vals {
                    m = java_min(m, v);
                }
            }
            self.mins[ord] = m;
        }
        if !self.maxs.is_empty() {
            let mut m = self.maxs[ord];
            if self.kind == SimpleKind::Max {
                m = java_max(m, vals[vals.len() - 1]);
            } else {
                for &v in vals {
                    m = java_max(m, v);
                }
            }
            self.maxs[ord] = m;
        }
        if !self.sqs.is_empty() {
            let mut k = CompensatedSum::new(self.sqs[ord], self.sq_comps[ord]);
            for &v in vals {
                k.add(v * v);
            }
            self.sqs[ord] = k.value;
            self.sq_comps[ord] = k.delta;
        }
    }

    /// A histogram's centroids, each weighted by its count (`HistoBacked*Aggregator`).
    fn add_centroids(&mut self, ord: usize, centroids: &[(f64, u64)]) {
        if !self.counts.is_empty() {
            for &(_, c) in centroids {
                self.counts[ord] += c;
            }
        }
        if !self.sums.is_empty() {
            let mut k = CompensatedSum::new(self.sums[ord], self.comps[ord]);
            for &(v, c) in centroids {
                k.add(v * c as f64);
            }
            self.sums[ord] = k.value;
            self.comps[ord] = k.delta;
        }
        if !self.mins.is_empty() {
            for &(v, _) in centroids {
                self.mins[ord] = java_min(self.mins[ord], v);
            }
        }
        if !self.maxs.is_empty() {
            for &(v, _) in centroids {
                self.maxs[ord] = java_max(self.maxs[ord], v);
            }
        }
    }

    /// An exponential histogram's stored summary (`ExponentialHistogram*Aggregator`: the
    /// stored sum and count; an empty histogram's min/max are `+Inf`/`-Inf`).
    fn add_exp_summary(&mut self, ord: usize, sm: &crate::exp_histogram::Summary) {
        if !self.counts.is_empty() {
            self.counts[ord] = self.counts[ord].wrapping_add(sm.value_count);
        }
        if !self.sums.is_empty() {
            let mut k = CompensatedSum::new(self.sums[ord], self.comps[ord]);
            k.add(sm.sum);
            self.sums[ord] = k.value;
            self.comps[ord] = k.delta;
        }
        if !self.mins.is_empty() {
            self.mins[ord] = java_min(self.mins[ord], if sm.min.is_nan() { f64::INFINITY } else { sm.min });
        }
        if !self.maxs.is_empty() {
            self.maxs[ord] = java_max(self.maxs[ord], if sm.max.is_nan() { f64::NEG_INFINITY } else { sm.max });
        }
    }

    /// An aggregate metric's stored metrics (`AggregateMetricBacked*Aggregator`).
    fn add_aggregate_metric(&mut self, ord: usize, r: &crate::histogram_field::AggregateMetricReader, doc: u32) {
        if !self.sums.is_empty() {
            let mut k = CompensatedSum::new(self.sums[ord], self.comps[ord]);
            r.sum(doc, |v| k.add(v));
            self.sums[ord] = k.value;
            self.comps[ord] = k.delta;
        }
        if !self.counts.is_empty() {
            let mut n = 0u64;
            r.value_count(doc, |v| n = n.wrapping_add(v as i64 as u64));
            self.counts[ord] = self.counts[ord].wrapping_add(n);
        }
        if !self.mins.is_empty() {
            // `MultiValueMode.MIN` of the doc's mins.
            let mut first = None;
            r.min(doc, |v| {
                if first.is_none() {
                    first = Some(v);
                }
            });
            if let Some(v) = first {
                self.mins[ord] = java_min(self.mins[ord], v);
            }
        }
        if !self.maxs.is_empty() {
            let mut last = None;
            r.max(doc, |v| last = Some(v));
            if let Some(v) = last {
                self.maxs[ord] = java_max(self.maxs[ord], v);
            }
        }
    }

    fn value_of(&self, ord: u32, key: Option<&str>) -> f64 {
        let o = ord as usize;
        let count = self.counts.get(o).copied().unwrap_or(0);
        let sum = self.sums.get(o).copied().unwrap_or(0.0);
        let min = self.mins.get(o).copied().unwrap_or(f64::INFINITY);
        let max = self.maxs.get(o).copied().unwrap_or(f64::NEG_INFINITY);
        match self.kind {
            SimpleKind::Avg => {
                if count == 0 {
                    f64::NAN
                } else {
                    sum / count as f64
                }
            }
            SimpleKind::Sum => sum,
            SimpleKind::Min => min,
            SimpleKind::Max => max,
            SimpleKind::ValueCount => count as f64,
            SimpleKind::Stats | SimpleKind::ExtendedStats => {
                let st = ExtendedStats {
                    count,
                    sum,
                    min,
                    max,
                    sum_of_sqrs: self.sqs.get(o).copied().unwrap_or(0.0),
                    sigma: self.sigma,
                    format: DocValueFormat::Raw,
                };
                let k = key.unwrap_or("");
                // Shard-level ordering (`metric(name, ord)`): an empty bucket orders as NaN for
                // avg, as the raw arrays for the rest.
                match k {
                    "count" => count as f64,
                    "sum" => sum,
                    "min" => {
                        if count == 0 {
                            f64::INFINITY
                        } else {
                            min
                        }
                    }
                    "max" => {
                        if count == 0 {
                            f64::NEG_INFINITY
                        } else {
                            max
                        }
                    }
                    "avg" => {
                        if count == 0 {
                            f64::NAN
                        } else {
                            sum / count as f64
                        }
                    }
                    other => {
                        if count == 0 {
                            return f64::NAN;
                        }
                        st.value(other).unwrap_or(f64::NAN)
                    }
                }
            }
        }
    }

    fn build_one(&self, ord: u32) -> crate::internal::AggValue {
        use crate::internal::AggValue;
        let o = ord as usize;
        let count = self.counts.get(o).copied().unwrap_or(0);
        let sum = self.sums.get(o).copied().unwrap_or(0.0);
        let min = self.mins.get(o).copied().unwrap_or(f64::INFINITY);
        let max = self.maxs.get(o).copied().unwrap_or(f64::NEG_INFINITY);
        let format = self.format.clone();
        match self.kind {
            SimpleKind::Avg => AggValue::Avg(Box::new(Avg { sum, count, format })),
            SimpleKind::Sum => AggValue::Sum(Box::new(Sum { sum, format })),
            SimpleKind::Min => AggValue::Min(Box::new(Min { value: min, format })),
            SimpleKind::Max => AggValue::Max(Box::new(Max { value: max, format })),
            SimpleKind::ValueCount => AggValue::ValueCount(Box::new(ValueCount { count })),
            SimpleKind::Stats => AggValue::Stats(Box::new(Stats { count, sum, min, max, format })),
            SimpleKind::ExtendedStats => AggValue::ExtendedStats(Box::new(ExtendedStats {
                count,
                sum,
                min,
                max,
                sum_of_sqrs: self.sqs.get(o).copied().unwrap_or(0.0),
                sigma: self.sigma,
                format,
            })),
        }
    }
}

impl SimpleAgg {
    /// The segment's min / max / value count from its skip index, when every doc counts.
    fn read_skipper(&mut self, seg: &SegmentContext) -> Result<bool> {
        if !self.whole_segments || seg.has_deletions() {
            return Ok(false);
        }
        let Reader::Num(r) = &self.reader else { return Ok(false) };
        let Some((dv, decode)) = r.raw() else { return Ok(false) };
        let skipper = match dv {
            crate::values::DvNum::Single(n) => n.skipper(),
            crate::values::DvNum::Multi(s) => s.skipper(),
            crate::values::DvNum::None => return Ok(true),
        };
        let Some(sk) = skipper else { return Ok(false) };
        if sk.global_doc_count() == 0 {
            return Ok(true);
        }
        let single = dv.is_single_valued();
        let (min, max, docs) = (decode.to_f64(sk.global_min_value()), decode.to_f64(sk.global_max_value()), sk.global_doc_count() as u64);
        match self.kind {
            SimpleKind::Min => {
                self.ensure(0)?;
                self.mins[0] = java_min(self.mins[0], min);
            }
            SimpleKind::Max => {
                self.ensure(0)?;
                self.maxs[0] = java_max(self.maxs[0], max);
            }
            SimpleKind::ValueCount if single => {
                self.ensure(0)?;
                self.counts[0] += docs;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

impl Aggregator for SimpleAgg {
    fn segment_done(&self) -> bool {
        self.segment_done
    }

    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.reader = if self.vs.unmapped && self.vs.missing.is_none() {
            Reader::None
        } else {
            match self.vs.vs_type {
                VsType::Numeric | VsType::Date | VsType::Boolean => Reader::Num(self.vs.numeric_reader(seg)?),
                VsType::GeoPoint => Reader::Geo(self.vs.geo_reader(seg)?),
                VsType::Histogram => Reader::Histogram(crate::histogram_field::HistogramReader::open(seg, self.vs.field.as_ref().map(|f| f.number))),
                VsType::ExponentialHistogram => {
                    Reader::ExpHistogram(Box::new(crate::exp_histogram::ExpHistogramReader::open(seg, self.vs.field.as_ref().map(|f| f.number))))
                }
                VsType::AggregateMetric => match self.vs.aggregate_metric() {
                    Some(m) => Reader::AggregateMetric(Box::new(crate::histogram_field::AggregateMetricReader::open(seg, &m))),
                    None => Reader::None,
                },
                _ => Reader::Bytes(self.vs.bytes_reader(seg)?),
            }
        };
        self.segment_done = self.read_skipper(seg)?;
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        if self.segment_done {
            return Ok(());
        }
        let mut reader = std::mem::replace(&mut self.reader, Reader::None);
        let r = (|| {
            match &mut reader {
                Reader::None => {}
                Reader::Num(r) if r.raw().is_some() => {
                    let (dv, decode) = r.raw().expect("raw");
                    let mut batch = std::mem::take(&mut self.batch);
                    let mut dbuf = std::mem::take(&mut self.dbuf);
                    dv.load_batch(b.docs, &mut batch);
                    let res: Result<()> = (|| {
                        if batch.all_single() {
                            for (k, &p) in batch.pos.iter().enumerate() {
                                let ord = b.ords[p as usize];
                                self.ensure(ord)?;
                                self.add_one(ord as usize, decode.to_f64(batch.vals[k]));
                            }
                            return Ok(());
                        }
                        for (p, raw) in batch.docs() {
                            let ord = b.ords[p];
                            self.ensure(ord)?;
                            dbuf.clear();
                            dbuf.extend(raw.iter().map(|&v| decode.to_f64(v)));
                            self.add_values(ord as usize, &dbuf);
                        }
                        Ok(())
                    })();
                    self.batch = batch;
                    self.dbuf = dbuf;
                    res?;
                }
                Reader::Num(r) => {
                    for i in 0..b.len() {
                        let vals = r.doubles(b.docs[i], b.score(i))?;
                        if vals.is_empty() {
                            continue;
                        }
                        let ord = b.ords[i];
                        self.ensure(ord)?;
                        self.add_values(ord as usize, vals);
                    }
                }
                Reader::Bytes(r) => {
                    for i in 0..b.len() {
                        let n = r.load(b.docs[i], b.score(i))?;
                        if n > 0 {
                            self.ensure(b.ords[i])?;
                            self.counts[b.ords[i] as usize] += n as u64;
                        }
                    }
                }
                Reader::Geo(r) => {
                    for i in 0..b.len() {
                        let n = r.points(b.docs[i]).len();
                        if n > 0 {
                            self.ensure(b.ords[i])?;
                            self.counts[b.ords[i] as usize] += n as u64;
                        }
                    }
                }
                Reader::Histogram(r) => {
                    for i in 0..b.len() {
                        let centroids = r.centroids(b.docs[i])?;
                        if centroids.is_empty() {
                            continue;
                        }
                        let ord = b.ords[i];
                        self.ensure(ord)?;
                        self.add_centroids(ord as usize, centroids);
                    }
                }
                Reader::AggregateMetric(r) => {
                    for i in 0..b.len() {
                        let ord = b.ords[i];
                        self.ensure(ord)?;
                        self.add_aggregate_metric(ord as usize, r, b.docs[i]);
                    }
                }
                Reader::ExpHistogram(r) => {
                    for i in 0..b.len() {
                        let Some(sm) = r.summary(b.docs[i])? else { continue };
                        let ord = b.ords[i];
                        self.ensure(ord)?;
                        self.add_exp_summary(ord as usize, &sm);
                    }
                }
            }
            Ok(())
        })();
        self.reader = reader;
        r
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<crate::internal::AggValue>> {
        Ok(owning.iter().map(|&o| self.build_one(o)).collect())
    }

    fn build_empty(&self) -> crate::internal::AggValue {
        self.build_one(u32::MAX)
    }

    fn needs_scores(&self) -> bool {
        self.vs.script.as_ref().is_some_and(|s| s.needs_score())
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        match self.kind {
            SimpleKind::Stats => multi_value_validate_sort(name, key, rest, |k| STATS_METRICS.contains(&k)),
            SimpleKind::ExtendedStats => multi_value_validate_sort(name, key, rest, |k| STATS_METRICS.contains(&k) || EXT_METRICS.contains(&k)),
            _ => single_value_validate_sort(name, key, rest),
        }
    }

    fn sort_value(&self, key: Option<&str>, _rest: &[PathElement], ord: u32) -> f64 {
        self.value_of(ord, key)
    }
}

const STATS_METRICS: &[&str] = &["count", "sum", "min", "max", "avg"];
const EXT_METRICS: &[&str] = &[
    "sum_of_squares",
    "variance",
    "variance_population",
    "variance_sampling",
    "std_deviation",
    "std_deviation_population",
    "std_deviation_sampling",
    "std_upper",
    "std_lower",
    "std_upper_population",
    "std_lower_population",
    "std_upper_sampling",
    "std_lower_sampling",
];

pub(crate) fn add_value_as_string(out: &mut Json, key: &str, format: &DocValueFormat, v: f64) {
    if !format.is_raw() {
        out.push(key, format.double_json(v));
    }
}

/// `avg`.
#[derive(Clone, Debug)]
pub struct Avg {
    pub sum: f64,
    pub count: u64,
    pub format: DocValueFormat,
}

impl Avg {
    pub fn value(&self) -> f64 {
        self.sum / self.count as f64
    }
}

impl Internal for Avg {
    fn type_name(&self) -> &'static str {
        "avg"
    }
    fn class_name(&self) -> &'static str {
        "InternalAvg"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut k = CompensatedSum::default();
        let mut count = 0;
        let format = list[0].format.clone();
        for a in &list {
            count += a.count;
            k.add(a.sum);
        }
        Ok(Avg { sum: k.value, count, format })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        out.push("value", if self.count != 0 { Json::Float(self.value()) } else { Json::Null });
        if self.count != 0 {
            add_value_as_string(out, "value_as_string", &self.format, self.value());
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.value(), path)
    }
}

/// `sum`.
#[derive(Clone, Debug)]
pub struct Sum {
    pub sum: f64,
    pub format: DocValueFormat,
}

impl Sum {
    pub fn value(&self) -> f64 {
        self.sum
    }
}

impl Internal for Sum {
    fn type_name(&self) -> &'static str {
        "sum"
    }
    fn class_name(&self) -> &'static str {
        "Sum"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut k = CompensatedSum::default();
        for a in &list {
            k.add(a.sum);
        }
        Ok(Sum { sum: k.value, format: list[0].format.clone() })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        out.push("value", Json::Float(self.sum));
        add_value_as_string(out, "value_as_string", &self.format, self.sum);
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.sum, path)
    }
}

/// `min`.
#[derive(Clone, Debug)]
pub struct Min {
    pub value: f64,
    pub format: DocValueFormat,
}

impl Internal for Min {
    fn type_name(&self) -> &'static str {
        "min"
    }
    fn class_name(&self) -> &'static str {
        "Min"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut v = f64::INFINITY;
        for a in &list {
            v = java_min(v, a.value);
        }
        Ok(Min { value: v, format: list[0].format.clone() })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let has = !self.value.is_infinite();
        out.push("value", if has { Json::Float(self.value) } else { Json::Null });
        if has {
            add_value_as_string(out, "value_as_string", &self.format, self.value);
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.value, path)
    }
}

/// `max`.
#[derive(Clone, Debug)]
pub struct Max {
    pub value: f64,
    pub format: DocValueFormat,
}

impl Internal for Max {
    fn type_name(&self) -> &'static str {
        "max"
    }
    fn class_name(&self) -> &'static str {
        "Max"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut v = f64::NEG_INFINITY;
        for a in &list {
            v = java_max(v, a.value);
        }
        Ok(Max { value: v, format: list[0].format.clone() })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let has = !self.value.is_infinite();
        out.push("value", if has { Json::Float(self.value) } else { Json::Null });
        if has {
            add_value_as_string(out, "value_as_string", &self.format, self.value);
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.value, path)
    }
}

/// `value_count`.
#[derive(Clone, Debug)]
pub struct ValueCount {
    pub count: u64,
}

impl Internal for ValueCount {
    fn type_name(&self) -> &'static str {
        "value_count"
    }
    fn class_name(&self) -> &'static str {
        "InternalValueCount"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        Ok(ValueCount { count: list.iter().map(|a| a.count).sum() })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        out.push("value", Json::from(self.count));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.count as f64, path)
    }
}

/// `stats`.
#[derive(Clone, Debug)]
pub struct Stats {
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub format: DocValueFormat,
}

impl Stats {
    pub fn avg(&self) -> f64 {
        self.sum / self.count as f64
    }

    /// `value(name)`.
    pub fn value(&self, name: &str) -> Result<f64> {
        Ok(match name {
            "min" => self.min,
            "max" => self.max,
            "avg" => self.avg(),
            "count" => self.count as f64,
            "sum" => self.sum,
            other => {
                return Err(AggError::illegal_argument(format!(
                    "No enum constant org.elasticsearch.search.aggregations.metrics.InternalStats.Metrics.{other}"
                )));
            }
        })
    }

    fn reduce_parts(list: impl Iterator<Item = (u64, f64, f64, f64)>) -> (u64, f64, f64, f64) {
        let mut count = 0;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut k = CompensatedSum::default();
        for (c, s, mi, ma) in list {
            count += c;
            min = java_min(min, mi);
            max = java_max(max, ma);
            k.add(s);
        }
        (count, k.value, min, max)
    }

    fn render_stats(&self, out: &mut Json) {
        out.push("count", Json::from(self.count));
        if self.count != 0 {
            out.push("min", Json::Float(self.min));
            out.push("max", Json::Float(self.max));
            out.push("avg", Json::Float(self.avg()));
            out.push("sum", Json::Float(self.sum));
            if !self.format.is_raw() {
                out.push("min_as_string", self.format.double_json(self.min));
                out.push("max_as_string", self.format.double_json(self.max));
                out.push("avg_as_string", self.format.double_json(self.avg()));
                out.push("sum_as_string", self.format.double_json(self.sum));
            }
        } else {
            out.push("min", Json::Null);
            out.push("max", Json::Null);
            out.push("avg", Json::Null);
            out.push("sum", Json::Float(0.0));
        }
    }
}

impl Internal for Stats {
    fn type_name(&self) -> &'static str {
        "stats"
    }
    fn class_name(&self) -> &'static str {
        "InternalStats"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let (count, sum, min, max) = Stats::reduce_parts(list.iter().map(|s| (s.count, s.sum, s.min, s.max)));
        Ok(Stats { count, sum, min, max, format: list[0].format.clone() })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        self.render_stats(out);
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_value_property(name, path, |k| self.value(k))
    }
}

/// `MultiValue.getProperty`.
pub fn multi_value_property(name: &str, path: &[String], value: impl Fn(&str) -> Result<f64>) -> Result<Prop<'static>> {
    if path.len() == 1 {
        return Ok(Prop::Num(value(&path[0])?));
    }
    Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path))))
}

/// `extended_stats`.
#[derive(Clone, Debug)]
pub struct ExtendedStats {
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub sum_of_sqrs: f64,
    pub sigma: f64,
    pub format: DocValueFormat,
}

impl ExtendedStats {
    fn stats(&self) -> Stats {
        Stats { count: self.count, sum: self.sum, min: self.min, max: self.max, format: self.format.clone() }
    }

    pub fn avg(&self) -> f64 {
        self.sum / self.count as f64
    }

    pub fn variance_population(&self) -> f64 {
        let c = self.count as f64;
        let v = (self.sum_of_sqrs - ((self.sum * self.sum) / c)) / c;
        if v < 0.0 { 0.0 } else { v }
    }

    pub fn variance_sampling(&self) -> f64 {
        let c = self.count as f64;
        let v = (self.sum_of_sqrs - ((self.sum * self.sum) / c)) / (c - 1.0);
        if v < 0.0 { 0.0 } else { v }
    }

    pub fn value(&self, name: &str) -> Result<f64> {
        let sd_pop = || self.variance_population().sqrt();
        let sd_samp = || self.variance_sampling().sqrt();
        Ok(match name {
            "sum_of_squares" => self.sum_of_sqrs,
            "variance" | "variance_population" => self.variance_population(),
            "variance_sampling" => self.variance_sampling(),
            "std_deviation" | "std_deviation_population" => sd_pop(),
            "std_deviation_sampling" => sd_samp(),
            "std_upper" | "std_upper_population" => self.avg() + sd_pop() * self.sigma,
            "std_lower" | "std_lower_population" => self.avg() - sd_pop() * self.sigma,
            "std_upper_sampling" => self.avg() + sd_samp() * self.sigma,
            "std_lower_sampling" => self.avg() - sd_samp() * self.sigma,
            other => return self.stats().value(other),
        })
    }

    /// The rendered body shared with `extended_stats_bucket`.
    pub fn render_extended(&self, out: &mut Json) {
        self.stats().render_stats(out);
        if self.count != 0 {
            let f = |k: &str| self.value(k).unwrap_or(f64::NAN);
            out.push("sum_of_squares", Json::Float(self.sum_of_sqrs));
            out.push("variance", Json::Float(f("variance")));
            out.push("variance_population", Json::Float(f("variance_population")));
            out.push("variance_sampling", Json::Float(f("variance_sampling")));
            out.push("std_deviation", Json::Float(f("std_deviation")));
            out.push("std_deviation_population", Json::Float(f("std_deviation_population")));
            out.push("std_deviation_sampling", Json::Float(f("std_deviation_sampling")));
            let bounds = |g: &dyn Fn(&str) -> Json| {
                Json::obj()
                    .with("upper", g("std_upper"))
                    .with("lower", g("std_lower"))
                    .with("upper_population", g("std_upper_population"))
                    .with("lower_population", g("std_lower_population"))
                    .with("upper_sampling", g("std_upper_sampling"))
                    .with("lower_sampling", g("std_lower_sampling"))
            };
            out.push("std_deviation_bounds", bounds(&|k| Json::Float(f(k))));
            if !self.format.is_raw() {
                let s = |k: &str| self.format.double_json(f(k));
                out.push("sum_of_squares_as_string", self.format.double_json(self.sum_of_sqrs));
                out.push("variance_as_string", s("variance"));
                out.push("variance_population_as_string", s("variance_population"));
                out.push("variance_sampling_as_string", s("variance_sampling"));
                out.push("std_deviation_as_string", s("std_deviation"));
                out.push("std_deviation_population_as_string", s("std_deviation_population"));
                out.push("std_deviation_sampling_as_string", s("std_deviation_sampling"));
                out.push("std_deviation_bounds_as_string", bounds(&s));
            }
        } else {
            for k in ["sum_of_squares", "variance", "variance_population", "variance_sampling", "std_deviation", "std_deviation_population", "std_deviation_sampling"] {
                out.push(k, Json::Null);
            }
            out.push(
                "std_deviation_bounds",
                Json::obj()
                    .with("upper", Json::Null)
                    .with("lower", Json::Null)
                    .with("upper_population", Json::Null)
                    .with("lower_population", Json::Null)
                    .with("upper_sampling", Json::Null)
                    .with("lower_sampling", Json::Null),
            );
        }
    }

    /// Reduces sums of squares with the hand-written Kahan loop `InternalExtendedStats` uses.
    pub fn reduce_squares(values: impl Iterator<Item = f64>) -> f64 {
        let mut sum = 0.0f64;
        let mut comp = 0.0f64;
        for value in values {
            if !value.is_finite() {
                sum += value;
            } else if sum.is_finite() {
                let corrected = value - comp;
                let new_sum = sum + corrected;
                comp = (new_sum - sum) - corrected;
                sum = new_sum;
            }
        }
        sum
    }
}

impl Internal for ExtendedStats {
    fn type_name(&self) -> &'static str {
        "extended_stats"
    }
    fn class_name(&self) -> &'static str {
        "InternalExtendedStats"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let sigma = list[0].sigma;
        if list.iter().any(|s| s.sigma != sigma) {
            return Err(AggError::illegal_state("Cannot reduce other stats aggregations that have a different sigma"));
        }
        let sum_of_sqrs = ExtendedStats::reduce_squares(list.iter().map(|s| s.sum_of_sqrs));
        let (count, sum, min, max) = Stats::reduce_parts(list.iter().map(|s| (s.count, s.sum, s.min, s.max)));
        Ok(ExtendedStats { count, sum, min, max, sum_of_sqrs, sigma, format: list[0].format.clone() })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        self.render_extended(out);
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_value_property(name, path, |k| self.value(k))
    }
}

/// A `weighted_avg` source (`value` / `weight`).
#[derive(Clone, Debug, Default)]
pub struct MvsField {
    pub field: Option<String>,
    pub missing: Option<Json>,
    pub script: Option<crate::script::Script>,
}

/// Parses a `field_config` object (`MultiValuesSourceFieldConfig`).
pub fn parse_field_config(p: &mut XParser<'_>, scriptable: bool) -> Result<MvsField> {
    let mut fields = vec![Field::new("field", Tokens::STRING), Field::new("missing", Tokens::VALUE)];
    if scriptable {
        fields.push(Field::new("script", Tokens::OBJECT_OR_STRING));
    }
    let mut out = MvsField::default();
    p.object("field_config", &fields, |p, i| {
        match i {
            0 => out.field = Some(p.text()?),
            1 => out.missing = p.object_text()?,
            _ => out.script = Some(crate::script::Script::parse(p)?),
        }
        Ok(())
    })?;
    out.check()?;
    Ok(out)
}

impl MvsField {
    /// `MultiValuesSourceFieldConfig.Builder.build` checks.
    pub fn check(&self) -> Result<()> {
        let empty = self.field.as_deref().is_none_or(str::is_empty);
        if empty && self.script.is_none() {
            return Err(AggError::illegal_argument("[field] and [script] cannot both be null.  Please specify one or the other."));
        }
        if !empty && self.script.is_some() {
            return Err(AggError::illegal_argument("[field] and [script] cannot both be configured.  Please specify one or the other."));
        }
        Ok(())
    }

    pub fn to_vs(&self) -> VsSpec {
        VsSpec { field: self.field.clone(), script: self.script.clone(), missing: self.missing.clone(), ..Default::default() }
    }
}

/// A parsed `weighted_avg`.
#[derive(Clone, Debug)]
pub struct WeightedAvgSpec {
    pub value: MvsField,
    pub weight: MvsField,
    pub format: Option<String>,
}

pub fn parse_weighted_avg(p: &mut XParser<'_>) -> Result<WeightedAvgSpec> {
    let fields = [
        Field::with_deprecated("value_type", &["valueType"], Tokens::STRING),
        Field::new("format", Tokens::STRING),
        Field::new("value", Tokens::OBJECT),
        Field::new("weight", Tokens::OBJECT),
    ];
    let mut value = None;
    let mut weight = None;
    let mut format = None;
    p.object("weighted_avg", &fields, |p, i| {
        match i {
            0 => {
                parse_value_type_hint(p, "weighted_avg")?;
            }
            1 => format = Some(p.text()?),
            2 => value = Some(parse_field_config(p, true)?),
            _ => weight = Some(parse_field_config(p, true)?),
        }
        Ok(())
    })?;
    let value = value.ok_or_else(|| AggError::illegal_argument("[value] must not be null: [weighted_avg]"))?;
    let weight = weight.ok_or_else(|| AggError::illegal_argument("[weight] must not be null: [weighted_avg]"))?;
    Ok(WeightedAvgSpec { value, weight, format })
}

/// `MultiValuesSourceParseHelper.declareCommon`'s `value_type` for numeric aggregations.
pub(crate) fn parse_value_type_hint(p: &XParser<'_>, agg: &str) -> Result<crate::values::ValueType> {
    use crate::values::ValueType;
    let t = p.text()?;
    let vt = ValueType::lenient_parse(&t);
    match vt {
        Some(v @ (ValueType::Long | ValueType::Double | ValueType::Date | ValueType::Boolean)) => Ok(v),
        _ => Err(p.parsing_error(format!(
            "Aggregation [{agg}] was configured with an incompatible value type [{}].  It can only work on value off type [numeric]",
            vt.map(value_type_description).unwrap_or("null")
        ))),
    }
}

/// `ValueType.toString()` (its description).
fn value_type_description(v: crate::values::ValueType) -> &'static str {
    use crate::values::ValueType::*;
    match v {
        String => "string",
        Long => "byte|short|integer|long",
        Double => "float|double",
        Date => "date",
        Ip => "ip",
        Boolean => "boolean",
    }
}

pub fn create_weighted_avg(def: &AggDef, spec: &WeightedAvgSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let mut vspec = spec.value.to_vs();
    vspec.format = spec.format.clone();
    let value = bind(&vspec, ctx.lookup, VsType::Numeric, false)?;
    let weight = bind(&spec.weight.to_vs(), ctx.lookup, VsType::Numeric, false)?;
    // `NumericMultiValuesSource`.
    for v in [&value, &weight] {
        if let Some(f) = &v.field
            && !v.vs_type.is_numeric()
        {
            return Err(AggError::illegal_argument(format!("Expected numeric type on field [{}], but got [{}]", f.name, f.type_name)));
        }
    }
    Ok(Box::new(WeightedAvgAgg {
        format: value.format.clone(),
        value,
        weight,
        readers: None,
        sums: Vec::new(),
        sum_comps: Vec::new(),
        weights: Vec::new(),
        weight_comps: Vec::new(),
        tracker: ctx.tracker(def),
    }))
}

struct WeightedAvgAgg {
    value: BoundVs,
    weight: BoundVs,
    format: DocValueFormat,
    readers: Option<(NumReader, NumReader)>,
    sums: Vec<f64>,
    sum_comps: Vec<f64>,
    weights: Vec<f64>,
    weight_comps: Vec<f64>,
    tracker: MemTracker,
}

impl Aggregator for WeightedAvgAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.readers = Some((self.value.numeric_reader(seg)?, self.weight.numeric_reader(seg)?));
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let Some((vr, wr)) = self.readers.as_mut() else { return Ok(()) };
        for i in 0..b.len() {
            let doc = b.docs[i];
            let vals = vr.doubles(doc, b.score(i))?;
            if vals.is_empty() {
                continue;
            }
            let ws = wr.doubles(doc, b.score(i))?;
            if ws.is_empty() {
                continue;
            }
            if ws.len() > 1 {
                return Err(AggError::illegal_argument(
                    "Encountered more than one weight for a single document. Use a script to combine multiple weights-per-doc into a single value.",
                ));
            }
            let w = ws[0];
            let ord = b.ords[i];
            grow(&mut self.sums, ord, 0.0);
            grow(&mut self.sum_comps, ord, 0.0);
            grow(&mut self.weights, ord, 0.0);
            grow(&mut self.weight_comps, ord, 0.0);
            self.tracker.ensure(self.sums.len() * 32)?;
            let o = ord as usize;
            let mut vs = CompensatedSum::new(self.sums[o], self.sum_comps[o]);
            let mut wsum = CompensatedSum::new(self.weights[o], self.weight_comps[o]);
            for &v in vals {
                vs.add(v * w);
                wsum.add(w);
            }
            self.sums[o] = vs.value;
            self.sum_comps[o] = vs.delta;
            self.weights[o] = wsum.value;
            self.weight_comps[o] = wsum.delta;
        }
        Ok(())
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<crate::internal::AggValue>> {
        Ok(owning
            .iter()
            .map(|&o| {
                crate::internal::AggValue::WeightedAvg(Box::new(WeightedAvg {
                    sum: self.sums.get(o as usize).copied().unwrap_or(0.0),
                    weight: self.weights.get(o as usize).copied().unwrap_or(0.0),
                    format: self.format.clone(),
                }))
            })
            .collect())
    }

    fn build_empty(&self) -> crate::internal::AggValue {
        crate::internal::AggValue::WeightedAvg(Box::new(WeightedAvg { sum: 0.0, weight: 0.0, format: self.format.clone() }))
    }

    fn needs_scores(&self) -> bool {
        self.value.script.as_ref().is_some_and(|s| s.needs_score()) || self.weight.script.as_ref().is_some_and(|s| s.needs_score())
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        single_value_validate_sort(name, key, rest)
    }

    fn sort_value(&self, _key: Option<&str>, _rest: &[PathElement], ord: u32) -> f64 {
        let w = self.weights.get(ord as usize).copied().unwrap_or(0.0);
        if w == 0.0 { f64::NAN } else { self.sums[ord as usize] / w }
    }
}

/// `weighted_avg`.
#[derive(Clone, Debug)]
pub struct WeightedAvg {
    pub sum: f64,
    pub weight: f64,
    pub format: DocValueFormat,
}

impl WeightedAvg {
    pub fn value(&self) -> f64 {
        self.sum / self.weight
    }
}

impl Internal for WeightedAvg {
    fn type_name(&self) -> &'static str {
        "weighted_avg"
    }
    fn class_name(&self) -> &'static str {
        "InternalWeightedAvg"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut s = CompensatedSum::default();
        let mut w = CompensatedSum::default();
        for a in &list {
            s.add(a.sum);
            w.add(a.weight);
        }
        Ok(WeightedAvg { sum: s.value, weight: w.value, format: list[0].format.clone() })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        out.push("value", if self.weight != 0.0 { Json::Float(self.value()) } else { Json::Null });
        if self.weight != 0.0 {
            add_value_as_string(out, "value_as_string", &self.format, self.value());
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.value(), path)
    }
}

/// `rate` (see `metrics::rate` for collection).
#[derive(Clone, Debug)]
pub struct Rate {
    pub sum: f64,
    pub divisor: f64,
    pub format: DocValueFormat,
}

impl Rate {
    pub fn value(&self) -> f64 {
        self.sum / self.divisor
    }
}

impl Internal for Rate {
    fn type_name(&self) -> &'static str {
        "rate"
    }
    fn class_name(&self) -> &'static str {
        "InternalRate"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut s = CompensatedSum::default();
        let mut divisor = None;
        for a in &list {
            s.add(a.sum);
            if divisor.is_none() {
                divisor = Some(a.divisor);
            }
        }
        Ok(Rate { sum: s.value, divisor: divisor.unwrap_or(1.0), format: list[0].format.clone() })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        out.push("value", Json::Float(self.value()));
        add_value_as_string(out, "value_as_string", &self.format, self.value());
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.value(), path)
    }
}

