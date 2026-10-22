//! Aggregation results: shard partials, (partially) reduced results and their rendering —
//! Elasticsearch's `InternalAggregation` family.
//!
//! A shard builds an [`Aggs`] tree after collection; the coordinating side merges any number
//! of them with [`reduce`](crate::reduce). Partial reduces (`final_reduce: false`, Elasticsearch's
//! batched reduce) keep every bucket and the full mergeable state (HyperLogLog registers,
//! t-digests, compensated sums) so they can be merged again; the final reduce applies sizes,
//! `min_doc_count`, empty buckets, the `search.max_buckets` limit and then the pipeline
//! aggregations, top-down, as `InternalAggregations.topLevelReduce` does.
//!
//! Every result type lives in its aggregation's module and implements [`Internal`];
//! [`AggValue`] is the closed set of them. Multi-bucket results also implement
//! [`MultiBuckets`], which pipelines use to read and rewrite buckets.

use std::sync::Arc;

use crate::error::{AggError, Result};
use crate::json::Json;
use crate::script::ScriptHost;

pub use crate::bucket::composite::Composite;
pub use crate::bucket::filters::{AdjacencyMatrix, Filters};
pub use crate::bucket::geogrid::GeoGrid;
pub use crate::bucket::rare_terms::RareTerms;
pub use crate::bucket::significant::SignificantTerms;
pub use crate::bucket::histogram::{DateHistogram, Histogram};
pub use crate::bucket::range::{IpPrefix, Range};
pub use crate::bucket::single::SingleBucket;
pub use crate::bucket::terms::Terms;
pub use crate::metrics::cardinality::Cardinality;
pub use crate::metrics::geo::{GeoBounds, GeoCentroid};
pub use crate::metrics::percentiles::Percentiles;
pub use crate::metrics::sketch::{Boxplot, MedianAbsoluteDeviation};
pub use crate::metrics::string_stats::StringStats;
pub use crate::metrics::top_hits::TopHits;
pub use crate::metrics::top_metrics::TopMetrics;
pub use crate::metrics::simple::{Avg, ExtendedStats, Max, Min, Rate, Stats, Sum, ValueCount, WeightedAvg};
pub use crate::pipeline::values::{BucketMetricValue, Derivative, ExtendedStatsBucket, PercentilesBucket, SimpleLongValue, SimpleValue, StatsBucket};

/// One named aggregation result.
#[derive(Clone, Debug)]
pub struct Agg {
    pub name: String,
    pub meta: Option<Arc<Json>>,
    pub value: AggValue,
}

/// The aggregations of one level (the top level or a bucket), in request order.
#[derive(Clone, Debug, Default)]
pub struct Aggs {
    pub list: Vec<Agg>,
}

impl Aggs {
    pub fn new(list: Vec<Agg>) -> Aggs {
        Aggs { list }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Agg> {
        self.list.iter().find(|a| a.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Agg> {
        self.list.iter_mut().find(|a| a.name == name)
    }

    /// Approximate heap bytes (memory accounting of reduce buffers).
    pub fn ram_bytes(&self) -> usize {
        self.list.iter().map(|a| 64 + a.name.len() + a.value.ram_bytes()).sum()
    }
}

/// Options of a reduce.
#[derive(Clone)]
pub struct ReduceOptions {
    /// The last reduce (apply sizes, `min_doc_count`, empty buckets, pipelines).
    pub final_reduce: bool,
    /// `search.max_buckets` (65536).
    pub max_buckets: usize,
    /// Whether earlier partial reduces (batched shard results) went into this reduce.
    pub has_batched_result: bool,
    /// Reduce-side scripts (`bucket_script`, `bucket_selector`, `moving_fn`, `scripted_metric`).
    pub scripts: Option<Arc<dyn ScriptHost>>,
    /// Memory budget for the reduce buffers.
    pub budget: Option<Arc<crate::memory::MemoryBudget>>,
}

impl Default for ReduceOptions {
    fn default() -> Self {
        ReduceOptions { final_reduce: true, max_buckets: 65536, has_batched_result: false, scripts: None, budget: None }
    }
}

impl std::fmt::Debug for ReduceOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReduceOptions").field("final_reduce", &self.final_reduce).field("max_buckets", &self.max_buckets).finish()
    }
}

/// State of one reduce (`AggregationReduceContext`).
pub struct ReduceCtx<'a> {
    pub opts: &'a ReduceOptions,
    buckets: usize,
}

impl<'a> ReduceCtx<'a> {
    pub fn new(opts: &'a ReduceOptions) -> ReduceCtx<'a> {
        ReduceCtx { opts, buckets: 0 }
    }

    #[inline]
    pub fn is_final(&self) -> bool {
        self.opts.final_reduce
    }

    /// `consumeBucketsAndMaybeBreak(n)`: counts buckets of the final reduce against
    /// `search.max_buckets`.
    pub fn consume_buckets(&mut self, n: usize) -> Result<()> {
        if !self.opts.final_reduce || n == 0 {
            return Ok(());
        }
        self.buckets += n;
        if self.buckets > self.opts.max_buckets {
            return Err(AggError::too_many_buckets(self.opts.max_buckets));
        }
        Ok(())
    }

    /// `consumeBucketsAndMaybeBreak(-n)`: buckets merged away.
    pub fn release_buckets(&mut self, n: usize) {
        if self.opts.final_reduce {
            self.buckets = self.buckets.saturating_sub(n);
        }
    }
}

/// What a property path resolves to (`getProperty`).
#[derive(Debug)]
pub enum Prop<'a> {
    Num(f64),
    /// A bucket key (or other non-numeric value).
    Value(Json),
    Agg(&'a Agg),
    Array(Vec<Prop<'a>>),
}

/// Behaviour shared by every result type.
pub trait Internal: Clone + std::fmt::Debug + Send + Sync + 'static {
    /// The `typed_keys` prefix (`sterms`, `avg`, `date_histogram`, …).
    fn type_name(&self) -> &'static str;
    /// The Java class name (for messages that print `getClass().getSimpleName()`).
    fn class_name(&self) -> &'static str;
    /// Merges results of the same aggregation from several shards (or partial reduces).
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self>;
    /// Multi-bucket results reduce even when alone (`mustReduceOnSingleInternalAgg`).
    fn must_reduce_single(&self) -> bool {
        false
    }
    /// Unmapped results don't lead a reduce (`canLeadReduction`).
    fn can_lead(&self) -> bool {
        true
    }
    /// Fields of the rendered aggregation object (after `meta`).
    fn render(&self, out: &mut Json, rc: &RenderCtx);
    /// `getProperty(path)` (path non-empty).
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>>;
    /// Approximate heap bytes.
    fn ram_bytes(&self) -> usize {
        64
    }
}

/// A multi-bucket result, as pipelines see it.
pub trait MultiBuckets {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn doc_count(&self, i: usize) -> u64;
    /// `getKey()`.
    fn key(&self, i: usize) -> Json;
    /// `getKeyAsString()`.
    fn key_string(&self, i: usize) -> String;
    fn aggs(&self, i: usize) -> &Aggs;
    fn aggs_mut(&mut self, i: usize) -> &mut Aggs;
    /// Keeps the buckets whose flag is set (`bucket_selector`, `bucket_sort` truncation).
    fn retain_buckets(&mut self, keep: &[bool]);
    /// Reorders buckets: new bucket `j` is old bucket `order[j]` (`bucket_sort`).
    fn reorder(&mut self, order: &[usize]);
    /// The numeric x-axis key of histogram-like buckets (derivative units).
    fn numeric_key(&self, i: usize) -> Option<f64> {
        let _ = i;
        None
    }
}

/// Rendering options.
#[derive(Clone, Copy, Debug, Default)]
pub struct RenderCtx {
    pub typed_keys: bool,
}

macro_rules! agg_values {
    ($($v:ident($t:ty)),* $(,)?) => {
        /// Every result type.
        #[derive(Clone, Debug)]
        pub enum AggValue { $($v(Box<$t>)),* }

        impl AggValue {
            pub fn type_name(&self) -> &'static str { match self { $(AggValue::$v(x) => x.type_name()),* } }
            pub fn class_name(&self) -> &'static str { match self { $(AggValue::$v(x) => x.class_name()),* } }
            fn must_reduce_single(&self) -> bool { match self { $(AggValue::$v(x) => x.must_reduce_single()),* } }
            fn can_lead(&self) -> bool { match self { $(AggValue::$v(x) => x.can_lead()),* } }
            pub fn render_body(&self, out: &mut Json, rc: &RenderCtx) { match self { $(AggValue::$v(x) => x.render(out, rc)),* } }
            pub fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> { match self { $(AggValue::$v(x) => x.property(name, path)),* } }
            pub fn ram_bytes(&self) -> usize { match self { $(AggValue::$v(x) => x.ram_bytes()),* } }
            fn same_kind(&self, o: &AggValue) -> bool { std::mem::discriminant(self) == std::mem::discriminant(o) }

            fn reduce_list(name: &str, list: Vec<AggValue>, ctx: &mut ReduceCtx<'_>) -> Result<AggValue> {
                // The first result that can lead decides the type; unmapped results that can't
                // lead are dropped unless every result is one of them.
                let lead = list.iter().position(|v| v.can_lead());
                let Some(lead) = lead else {
                    let first = list.into_iter().next().expect("non-empty");
                    return Ok(first);
                };
                let mut items = Vec::with_capacity(list.len());
                let lead_kind = list[lead].clone_kind_probe();
                for v in list {
                    if !v.can_lead() && !v.same_kind(&lead_kind) {
                        continue;
                    }
                    if !v.same_kind(&lead_kind) {
                        return Err(reduce_type_mismatch(name));
                    }
                    items.push(v);
                }
                match lead_kind {
                    $(AggValue::$v(_) => {
                        let typed: Vec<$t> = items.into_iter().map(|v| match v { AggValue::$v(x) => *x, _ => unreachable!() }).collect();
                        Ok(AggValue::$v(Box::new(<$t as Internal>::reduce(typed, ctx)?)))
                    })*
                }
            }

            /// The multi-bucket view, if this is a multi-bucket result.
            pub fn multi(&self) -> Option<&dyn MultiBuckets> {
                match self {
                    AggValue::Terms(x) => Some(x.as_ref()),
                    AggValue::Histogram(x) => Some(x.as_ref()),
                    AggValue::DateHistogram(x) => Some(x.as_ref()),
                    AggValue::AutoDateHistogram(x) => Some(x.as_ref()),
                    AggValue::VariableWidthHistogram(x) => Some(x.as_ref()),
                    AggValue::Range(x) => Some(x.as_ref()),
                    AggValue::IpPrefix(x) => Some(x.as_ref()),
                    AggValue::Filters(x) => Some(x.as_ref()),
                    AggValue::AdjacencyMatrix(x) => Some(x.as_ref()),
                    AggValue::Composite(x) => Some(x.as_ref()),
                    AggValue::GeoGrid(x) => Some(x.as_ref()),
                    AggValue::RareTerms(x) => Some(x.as_ref()),
                    AggValue::SignificantTerms(x) => Some(x.as_ref()),
                    AggValue::TimeSeries(x) => Some(x.as_ref()),
                    _ => None,
                }
            }

            pub fn multi_mut(&mut self) -> Option<&mut dyn MultiBuckets> {
                match self {
                    AggValue::Terms(x) => Some(x.as_mut()),
                    AggValue::Histogram(x) => Some(x.as_mut()),
                    AggValue::DateHistogram(x) => Some(x.as_mut()),
                    AggValue::AutoDateHistogram(x) => Some(x.as_mut()),
                    AggValue::VariableWidthHistogram(x) => Some(x.as_mut()),
                    AggValue::Range(x) => Some(x.as_mut()),
                    AggValue::IpPrefix(x) => Some(x.as_mut()),
                    AggValue::Filters(x) => Some(x.as_mut()),
                    AggValue::AdjacencyMatrix(x) => Some(x.as_mut()),
                    AggValue::Composite(x) => Some(x.as_mut()),
                    AggValue::GeoGrid(x) => Some(x.as_mut()),
                    AggValue::RareTerms(x) => Some(x.as_mut()),
                    AggValue::SignificantTerms(x) => Some(x.as_mut()),
                    AggValue::TimeSeries(x) => Some(x.as_mut()),
                    _ => None,
                }
            }
        }
    };
}

agg_values! {
    Avg(Avg),
    Sum(Sum),
    Min(Min),
    Max(Max),
    ValueCount(ValueCount),
    Stats(Stats),
    ExtendedStats(ExtendedStats),
    WeightedAvg(WeightedAvg),
    Rate(Rate),
    Cardinality(Cardinality),
    Percentiles(Percentiles),
    MedianAbsoluteDeviation(MedianAbsoluteDeviation),
    Boxplot(Boxplot),
    TopHits(TopHits),
    StringStats(StringStats),
    TopMetrics(TopMetrics),
    MatrixStats(crate::metrics::matrix_stats::MatrixStats),
    ScriptedMetric(crate::metrics::scripted_metric::ScriptedMetric),
    TTest(crate::metrics::t_test::TTest),
    CartesianBounds(crate::metrics::cartesian::CartesianBounds),
    CartesianCentroid(crate::metrics::cartesian::CartesianCentroid),
    GeoLine(crate::metrics::geo_line::GeoLine),
    TimeSeries(crate::bucket::time_series::TimeSeries),
    ResetTrackingRate(crate::metrics::ts_rate::ResetTrackingRate),
    Composite(Composite),
    GeoGrid(GeoGrid),
    RareTerms(RareTerms),
    SignificantTerms(SignificantTerms),
    GeoBounds(GeoBounds),
    GeoCentroid(GeoCentroid),
    Terms(Terms),
    Histogram(Histogram),
    DateHistogram(DateHistogram),
    AutoDateHistogram(crate::bucket::auto_histogram::AutoDateHistogram),
    VariableWidthHistogram(crate::bucket::variable_width::VariableWidthHistogram),
    Range(Range),
    IpPrefix(IpPrefix),
    Single(SingleBucket),
    Filters(Filters),
    AdjacencyMatrix(AdjacencyMatrix),
    SimpleValue(SimpleValue),
    SimpleLongValue(SimpleLongValue),
    Derivative(Derivative),
    BucketMetricValue(BucketMetricValue),
    StatsBucket(StatsBucket),
    ExtendedStatsBucket(ExtendedStatsBucket),
    PercentilesBucket(PercentilesBucket),
}

impl AggValue {
    /// A cheap value of the same variant used only to compare kinds.
    fn clone_kind_probe(&self) -> AggValue {
        self.clone()
    }

    /// The aggregations of a single-bucket result.
    pub fn single_aggs(&self) -> Option<&Aggs> {
        match self {
            AggValue::Single(s) => Some(&s.aggs),
            _ => None,
        }
    }

    pub fn single_aggs_mut(&mut self) -> Option<&mut Aggs> {
        match self {
            AggValue::Single(s) => Some(&mut s.aggs),
            _ => None,
        }
    }

    /// The value of a single-value numeric metric (`InternalNumericMetricsAggregation.SingleValue`).
    pub fn single_value(&self) -> Option<f64> {
        Some(match self {
            AggValue::Avg(a) => a.value(),
            AggValue::Sum(a) => a.value(),
            AggValue::Min(a) => a.value,
            AggValue::Max(a) => a.value,
            AggValue::ValueCount(a) => a.count as f64,
            AggValue::WeightedAvg(a) => a.value(),
            AggValue::TTest(a) => a.value(),
            AggValue::ResetTrackingRate(a) => a.value(),
            AggValue::Rate(a) => a.value(),
            AggValue::Cardinality(a) => a.value() as f64,
            AggValue::MedianAbsoluteDeviation(a) => a.value,
            AggValue::SimpleValue(a) => a.value,
            AggValue::SimpleLongValue(a) => a.value as f64,
            AggValue::Derivative(a) => a.value,
            AggValue::BucketMetricValue(a) => a.value,
            _ => return None,
        })
    }

    /// Whether this is a multi-value numeric metric (`NumericMetricsAggregation.MultiValue`).
    pub fn is_multi_value_metric(&self) -> bool {
        matches!(
            self,
            AggValue::Stats(_)
                | AggValue::ExtendedStats(_)
                | AggValue::StatsBucket(_)
                | AggValue::ExtendedStatsBucket(_)
                | AggValue::PercentilesBucket(_)
                | AggValue::Percentiles(_)
                | AggValue::Boxplot(_)
                | AggValue::TopMetrics(_)
        )
    }

    /// `MultiValue.value(name)` for multi-value metrics.
    pub fn multi_value(&self, name: &str) -> Result<f64> {
        match self.property("", &[name.to_string()])? {
            Prop::Num(v) => Ok(v),
            _ => Err(AggError::illegal_argument(format!("path not supported for [{name}]"))),
        }
    }
}

/// `String.hashCode` (over UTF-16 code units) spread like `HashMap.hash`.
fn java_string_hash(s: &str) -> u32 {
    let h = s.encode_utf16().fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(u as i32)) as u32;
    h ^ (h >> 16)
}

/// The iteration order of a `java.util.HashMap<String, _>` created with `new HashMap<>(initial)`
/// and fed `computeIfAbsent(keys[k])` for each `k` in `calls` (repeats allowed).
pub fn java_hash_map_order(keys: &[&str], calls: &[usize], initial: usize) -> Vec<usize> {
    let mut m = crate::util::JavaHashMapModel::new(Some(initial));
    let mut present = vec![false; keys.len()];
    for &k in calls {
        if present[k] {
            m.compute_if_absent_existing();
        } else {
            present[k] = true;
            m.compute_if_absent_new(k, java_string_hash(keys[k]));
        }
    }
    m.order()
}

/// `AggregationErrors.reduceTypeMismatch`.
pub fn reduce_type_mismatch(name: &str) -> AggError {
    AggError::illegal_argument(format!(
        "Merging/Reducing the aggregations failed when computing the aggregation [{name}] because the field you gave in the aggregation query existed as two different types in two different indices"
    ))
}

/// The results of one aggregation name across several lists (name, meta, values).
type Group = (String, Option<Arc<Json>>, Vec<AggValue>);

impl Aggs {
    /// `InternalAggregations.reduce(List)`: a single result keeps its order (only aggregations
    /// that must reduce alone are reduced); several results merge by name like
    /// [`Aggs::reduce_hashed`].
    pub fn reduce_lists(lists: Vec<Aggs>, ctx: &mut ReduceCtx<'_>) -> Result<Aggs> {
        if lists.len() != 1 {
            return Self::reduce_hashed(lists, ctx);
        }
        let aggs = lists.into_iter().next().expect("one");
        let mut out = Vec::with_capacity(aggs.list.len());
        for a in aggs.list {
            let value = if a.value.must_reduce_single() { AggValue::reduce_list(&a.name, vec![a.value], ctx)? } else { a.value };
            out.push(Agg { name: a.name, meta: a.meta, value });
        }
        Ok(Aggs { list: out })
    }

    /// `AggregatorsReducer` (bucket and single-bucket sub-aggregations, multi-result top level):
    /// merges by name and reduces every aggregation; the output follows the iteration order of
    /// Elasticsearch's `HashMap<String, _>` keyed by name, which is what its responses show.
    pub fn reduce_hashed(lists: Vec<Aggs>, ctx: &mut ReduceCtx<'_>) -> Result<Aggs> {
        let Some(first) = lists.first() else { return Ok(Aggs::default()) };
        let initial = first.list.len();
        let mut groups: Vec<Group> = Vec::with_capacity(initial);
        let mut index: hashbrown::HashMap<String, usize> = hashbrown::HashMap::with_capacity(initial);
        let mut calls = Vec::with_capacity(initial * lists.len());
        for aggs in lists {
            for a in aggs.list {
                match index.get(&a.name) {
                    Some(&g) => {
                        calls.push(g);
                        groups[g].2.push(a.value);
                    }
                    None => {
                        calls.push(groups.len());
                        index.insert(a.name.clone(), groups.len());
                        groups.push((a.name, a.meta, vec![a.value]));
                    }
                }
            }
        }
        let names: Vec<&str> = groups.iter().map(|g| g.0.as_str()).collect();
        let order = java_hash_map_order(&names, &calls, initial);
        let mut slots: Vec<Option<Group>> = groups.into_iter().map(Some).collect();
        let mut out = Vec::with_capacity(slots.len());
        for i in order {
            let (name, meta, values) = slots[i].take().expect("each group once");
            let value = AggValue::reduce_list(&name, values, ctx)?;
            out.push(Agg { name, meta, value });
        }
        Ok(Aggs { list: out })
    }

    /// Renders `{name: {...}, ...}` into `out` (an object).
    pub fn render_into(&self, out: &mut Json, rc: &RenderCtx) {
        for a in &self.list {
            let key = if rc.typed_keys { format!("{}#{}", a.value.type_name(), a.name) } else { a.name.clone() };
            out.push(key, a.render(rc));
        }
    }

    /// The `aggregations` object.
    pub fn to_json(&self, typed_keys: bool) -> Json {
        let mut o = Json::obj();
        self.render_into(&mut o, &RenderCtx { typed_keys });
        o
    }

    /// `InternalAggregations.getProperty(path)`.
    pub fn property(&self, path: &[String]) -> Result<Prop<'_>> {
        let Some(first) = path.first() else { return Err(AggError::illegal_argument("empty path")) };
        let agg = self.get(first).ok_or_else(|| AggError::invalid_agg_path(format!("Cannot find an aggregation named [{first}]")))?;
        if path.len() == 1 { Ok(Prop::Agg(agg)) } else { agg.value.property(&agg.name, &path[1..]) }
    }
}

impl Agg {
    /// The aggregation object (`meta` first, then the type's fields).
    pub fn render(&self, rc: &RenderCtx) -> Json {
        let mut o = Json::obj();
        if let Some(m) = &self.meta {
            o.push("meta", (**m).clone());
        }
        self.value.render_body(&mut o, rc);
        o
    }
}

/// `InternalBucket.getProperty(containingAggName, path)` for a bucket with `doc_count`, a key
/// and sub-aggregations.
pub fn bucket_property<'a>(containing: &str, doc_count: u64, key: impl FnOnce() -> Json, aggs: &'a Aggs, path: &[String]) -> Result<Prop<'a>> {
    let Some(first) = path.first() else { return Err(AggError::illegal_argument("empty path")) };
    match first.as_str() {
        "_count" => {
            if path.len() > 1 {
                return Err(AggError::invalid_agg_path("_count must be the last element in the path"));
            }
            Ok(Prop::Num(doc_count as f64))
        }
        "_key" => {
            if path.len() > 1 {
                return Err(AggError::invalid_agg_path("_key must be the last element in the path"));
            }
            Ok(Prop::Value(key()))
        }
        name => {
            let agg = aggs
                .get(name)
                .ok_or_else(|| AggError::invalid_agg_path(format!("Cannot find an aggregation named [{name}] in [{containing}]")))?;
            if path.len() == 1 { Ok(Prop::Agg(agg)) } else { agg.value.property(&agg.name, &path[1..]) }
        }
    }
}

/// `InternalMultiBucketAggregation.resolvePropertyFromPath`.
pub fn multi_bucket_property<'a, M: MultiBuckets + ?Sized>(m: &'a M, name: &str, path: &[String]) -> Result<Prop<'a>> {
    let first = &path[0];
    if first == "_bucket_count" {
        return Ok(Prop::Num(m.len() as f64));
    }
    if first.len() >= 2 && first.starts_with('\'') && first.ends_with('\'') {
        let want = &first[1..first.len() - 1];
        for i in 0..m.len() {
            if m.key_string(i) == want {
                return bucket_property(name, m.doc_count(i), || m.key(i), m.aggs(i), &path[1..]);
            }
        }
        return Err(AggError::invalid_agg_path(format!("Cannot find an key [{first}] in [{name}]")));
    }
    let mut out = Vec::with_capacity(m.len());
    for i in 0..m.len() {
        out.push(bucket_property(name, m.doc_count(i), || m.key(i), m.aggs(i), path)?);
    }
    Ok(Prop::Array(out))
}

/// `InternalNumericMetricsAggregation.SingleValue.getProperty`.
pub fn single_value_property(name: &str, value: f64, path: &[String]) -> Result<Prop<'static>> {
    if path.len() == 1 && path[0] == "value" {
        return Ok(Prop::Num(value));
    }
    Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path))))
}

/// A Java `List.toString()` of strings.
pub fn java_list(path: &[String]) -> String {
    format!("[{}]", path.join(", "))
}

/// Renders a double the way metric aggregations print `value`: `null` unless finite.
pub fn value_or_null(v: f64) -> Json {
    if v.is_finite() { Json::Float(v) } else { Json::Null }
}

/// Elasticsearch's `CompensatedSum` (Kahan summation).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CompensatedSum {
    pub value: f64,
    pub delta: f64,
}

impl CompensatedSum {
    pub fn new(value: f64, delta: f64) -> CompensatedSum {
        CompensatedSum { value, delta }
    }

    /// `add(value)`.
    #[inline]
    pub fn add(&mut self, v: f64) {
        self.add_with_delta(v, 0.0)
    }

    /// `add(value, delta)`: Kahan summation; infinities and NaN take over the sum.
    #[inline]
    pub fn add_with_delta(&mut self, v: f64, delta: f64) {
        if !v.is_finite() {
            self.value += v;
        }
        if self.value.is_finite() {
            let corrected = v + (self.delta + delta);
            let updated = self.value + corrected;
            self.delta = corrected - (updated - self.value);
            self.value = updated;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::java_hash_map_order;

    fn order(names: &[&str], lists: usize) -> Vec<String> {
        let calls: Vec<usize> = (0..lists).flat_map(|_| 0..names.len()).collect();
        java_hash_map_order(names, &calls, names.len()).into_iter().map(|i| names[i].to_string()).collect()
    }

    /// Orders observed on Elasticsearch 9.5.5 (one shard = one list, three shards = three).
    #[test]
    fn hash_map_order_matches_elasticsearch() {
        assert_eq!(order(&["s", "k"], 1), ["k", "s"]);
        assert_eq!(order(&["hh", "k", "s", "c"], 1), ["hh", "c", "s", "k"]);
        assert_eq!(order(&["hh", "zz", "a", "m"], 1), ["zz", "hh", "m", "a"]);
        assert_eq!(order(&["hh", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9"], 1), ["x8", "hh", "x9", "x1", "x2", "x3", "x4", "x5", "x6", "x7"]);
        assert_eq!(order(&["f", "fs", "fa", "fk"], 3), ["fa", "fk", "fs", "f"]);
        assert_eq!(order(&["r", "rk"], 3), ["rk", "r"]);
        assert_eq!(order(&["h", "h2"], 3), ["h", "h2"]);
    }
}
