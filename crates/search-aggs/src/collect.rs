//! Per-shard collection: the [`Aggregator`] model and the [`AggCollector`] that plugs it into
//! search-index's [`Collector`].
//!
//! Aggregators follow Elasticsearch's bucket-ordinal model: every aggregator collects docs into
//! "owning bucket ordinals" (0 at the top level; a bucket aggregation's own bucket ordinals for
//! its sub-aggregations) and keeps per-ordinal state in dense arrays. Unlike Elasticsearch,
//! collection is batched: the top-level collector buffers matching docs (and scores, only when
//! some aggregation reads them) and hands each aggregator a [`Batch`] of `(doc, ordinal)`
//! pairs — one virtual call per batch per aggregator instead of one per doc, and tight loops
//! over doc values inside. Bucket aggregations forward to their sub-aggregations through a
//! [`Subs`] buffer the same way. Docs within a segment arrive in non-decreasing order.
//!
//! Under a query matching every doc ([`ShardOptions::matches_all`]), top-level aggregations may
//! take a segment without deletions from its doc-values statistics (`min`, `max`,
//! `value_count` from the skip index, as Elasticsearch reads points for `min`/`max`); when all
//! of them do, the collector tells the search to skip the segment's docs.
//!
//! After collection, [`Aggregator::build`] turns ordinals into results: bucket aggregations
//! select their buckets per owning ordinal, then build all selected sub-bucket ordinals of a
//! sub-aggregation in one call (`buildSubAggsForAllBuckets`).

use std::sync::Arc;

use search_index::{Collector, LeafCollector, Scorable, ScoreMode, Searcher, SegmentContext};

use crate::error::{AggError, Result};
use crate::field::FieldLookup;
use crate::internal::{Agg, AggValue, Aggs};
use crate::memory::MemoryBudget;
use crate::request::{AggDef, AggRequest, PathElement};
use crate::values::DocCounter;

/// Docs per batch handed to aggregators.
pub const BATCH: usize = 256;

/// `(doc, owning ordinal)` pairs of one segment, docs non-decreasing; `scores` is empty when no
/// aggregation in the tree reads scores.
#[derive(Clone, Copy)]
pub struct Batch<'a> {
    pub docs: &'a [u32],
    pub ords: &'a [u32],
    pub scores: &'a [f32],
}

impl Batch<'_> {
    #[inline]
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    #[inline]
    pub fn score(&self, i: usize) -> f32 {
        if self.scores.is_empty() { f32::NAN } else { self.scores[i] }
    }
}

/// One aggregation's shard-side state.
pub trait Aggregator: Send {
    /// Prepares for a segment (or a doc-id range of one).
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()>;
    fn collect(&mut self, b: Batch<'_>) -> Result<()>;
    /// Called after the last batch of a segment.
    fn finish_segment(&mut self) -> Result<()> {
        Ok(())
    }
    /// Called once after every segment was collected (deferred work).
    fn post_collection(&mut self) -> Result<()> {
        Ok(())
    }
    /// Whether the aggregator took the current segment from its statistics (after
    /// [`set_segment`](Self::set_segment)) and needs none of its docs: when every top-level
    /// aggregator says so, the search skips the segment.
    fn segment_done(&self) -> bool {
        false
    }
    /// Results for each owning ordinal, in order.
    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>>;
    /// The result of a bucket nothing was collected into.
    fn build_empty(&self) -> AggValue;
    fn needs_scores(&self) -> bool {
        false
    }
    /// Checks that `key` (the metric name or `[key]` of the last path element) and the rest of
    /// a sort path can order parent buckets (`resolveSortPath` + `bucketComparator`).
    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        let _ = (key, rest);
        Err(AggError::illegal_argument(format!(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end. [{name}] is not single-bucket."
        )))
    }
    /// The sort value of owning ordinal `ord` for a validated path. Single-bucket aggregations
    /// return doc counts, metrics their value (NaN for none).
    fn sort_value(&self, key: Option<&str>, rest: &[PathElement], ord: u32) -> f64 {
        let _ = (key, rest, ord);
        f64::NAN
    }
    /// Whether [`sort_value`](Self::sort_value) is a doc count (no NaN handling).
    fn sort_is_count(&self, key: Option<&str>, rest: &[PathElement]) -> bool {
        let _ = (key, rest);
        false
    }
}

/// What binding an aggregation to a shard needs.
pub struct BindCtx<'a> {
    pub lookup: &'a dyn FieldLookup,
    pub searcher: &'a Searcher,
    pub budget: Option<Arc<MemoryBudget>>,
    /// Types of the enclosing aggregations, outermost first.
    pub parents: Vec<&'static str>,
    /// The nested path the docs being collected belong to (`nested` / `reverse_nested`).
    pub nested_path: Option<String>,
    pub doc_count_field: Option<u32>,
    /// The closest enclosing `SizedBucketAggregator` (for `rate`).
    pub sized: Option<SizedAncestor>,
    /// The enclosing `random_sampler`'s sampling (`SamplingContext`), when it samples.
    pub sampling: Option<crate::bucket::random_sampler::Sampling>,
    /// How many owning buckets the aggregation being bound may collect into.
    pub cardinality: BucketCardinality,
    /// The name of the closest enclosing `nested` aggregation over a mapped nested path.
    pub nested_agg: Option<String>,
    /// The request has a `time_series` aggregation (Elasticsearch then searches in `_tsid`
    /// order, which counter `rate`s need).
    pub time_series_request: bool,
    /// The query matches every live doc ([`ShardOptions::matches_all`]).
    pub matches_all: bool,
}

/// Elasticsearch's `CardinalityUpperBound`: an upper bound on the owning bucket ordinals an
/// aggregator collects into. Some aggregators behave differently when it is exactly one
/// (`auto_date_histogram`'s single-owner mode, `variable_width_histogram`'s nesting check), so
/// this follows Elasticsearch's rules exactly: range-like aggregations multiply it by their
/// range count, `filters` by its filter count (plus the other bucket), single-bucket
/// aggregations pass it through, `global` resets it to one, and everything else (including
/// `sampler`, `diversified_sampler`, `children` and `parent`, which are deferring or join
/// aggregators in Elasticsearch) makes it many.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketCardinality {
    /// Collects nothing (`CardinalityUpperBound.NONE`).
    None,
    /// At most this many owning buckets.
    Known(u32),
    Many,
}

impl BucketCardinality {
    pub const ONE: BucketCardinality = BucketCardinality::Known(1);

    /// `CardinalityUpperBound.multiply`.
    pub fn multiply(self, buckets: usize) -> BucketCardinality {
        match self {
            BucketCardinality::None => BucketCardinality::None,
            _ if buckets == 0 => BucketCardinality::None,
            BucketCardinality::Many => BucketCardinality::Many,
            BucketCardinality::Known(n) => {
                let m = n as u64 * buckets as u64;
                if m >= i32::MAX as u64 { BucketCardinality::Many } else { BucketCardinality::Known(m as u32) }
            }
        }
    }

    /// Whether at most one owning bucket (ordinal 0) is collected (`== CardinalityUpperBound.ONE`).
    pub fn is_one(self) -> bool {
        self == BucketCardinality::ONE
    }

    /// The cardinality `def` gives its sub-aggregations when it collects with `self`.
    pub fn for_subs_of(self, def: &AggDef) -> BucketCardinality {
        use crate::bucket::single::SingleSpec;
        use crate::parse::AggKind;
        match &def.kind {
            AggKind::Range(s) => self.multiply(s.ranges.len()),
            AggKind::IpRange(s) => self.multiply(s.ranges.len()),
            AggKind::Filters(s) => self.multiply(s.filters.len() + usize::from(s.other_bucket)),
            AggKind::Single(SingleSpec::Global) => BucketCardinality::ONE,
            AggKind::Single(SingleSpec::Sampler { .. } | SingleSpec::Diversified { .. }) => BucketCardinality::Many,
            AggKind::Single(_) | AggKind::RandomSampler(_) => self,
            _ => BucketCardinality::Many,
        }
    }
}

/// Bucket starts per bucket ordinal, published by a [`SizedAncestor`] at build time.
pub type BucketStarts = Arc<std::sync::Mutex<Vec<Option<i64>>>>;

/// The closest enclosing `SizedBucketAggregator` — a `date_histogram`, or a `composite` — that a
/// `rate` divides its sums by.
#[derive(Clone)]
pub struct SizedAncestor {
    /// `parents.len()` when the ancestor was bound: its direct children see one more parent.
    pub depth: usize,
    pub name: String,
    /// The date rounding; `None` for a composite without exactly one `date_histogram` source.
    pub rounding: Option<crate::rounding::Rounding>,
    /// The rounded start of each of the ancestor's bucket ordinals, filled by the ancestor just
    /// before it builds its sub-aggregations (`bucketSize(bucket, unit)`).
    pub starts: BucketStarts,
}

impl SizedAncestor {
    pub fn new(depth: usize, name: &str, rounding: Option<crate::rounding::Rounding>) -> SizedAncestor {
        SizedAncestor { depth, name: name.to_string(), rounding, starts: Arc::new(std::sync::Mutex::new(Vec::new())) }
    }

    /// Records bucket starts (ordinal, start) before the sub-aggregations are built.
    pub fn publish(starts: &std::sync::Mutex<Vec<Option<i64>>>, entries: impl Iterator<Item = (u32, Option<i64>)>) {
        let mut v = starts.lock().unwrap_or_else(|e| e.into_inner());
        v.clear();
        for (ord, start) in entries {
            let i = ord as usize;
            if v.len() <= i {
                v.resize(i + 1, None);
            }
            v[i] = start;
        }
    }
}

impl<'a> BindCtx<'a> {
    /// A top-level aggregation of a query matching every live doc: per-segment statistics
    /// (doc-values skip indexes) stand for the segment's docs when it has no deletions
    /// (Elasticsearch's points shortcut of top-level `min` / `max` under `match_all`).
    pub fn whole_segments(&self) -> bool {
        self.matches_all && self.parents.is_empty()
    }

    pub fn tracker(&self, def: &AggDef) -> crate::memory::MemTracker {
        crate::memory::MemTracker::new(self.budget.clone(), &def.name)
    }

    pub fn doc_counter(&self, seg: &SegmentContext) -> DocCounter {
        DocCounter::open(seg, self.doc_count_field)
    }

    pub fn is_top_level(&self) -> bool {
        self.parents.is_empty()
    }
}

/// An aggregator with its definition.
pub struct Node {
    pub def: Arc<AggDef>,
    pub agg: Box<dyn Aggregator>,
}

impl Node {
    fn wrap(&self, value: AggValue) -> Agg {
        Agg { name: self.def.name.clone(), meta: self.def.meta.clone(), value }
    }
}

/// The sub-aggregations of a bucket aggregation, with the batch buffer feeding them.
pub struct Subs {
    pub nodes: Vec<Node>,
    docs: Vec<u32>,
    ords: Vec<u32>,
    scores: Vec<f32>,
    needs_scores: bool,
}

impl Subs {
    /// Binds the sub-aggregations of `def` (the caller pushed its own type on `ctx.parents`).
    pub fn bind(def: &AggDef, ctx: &mut BindCtx<'_>) -> Result<Subs> {
        let mut nodes = Vec::with_capacity(def.subs.len());
        ctx.parents.push(def.type_name);
        let outer = ctx.cardinality;
        ctx.cardinality = outer.for_subs_of(def);
        let mut out = Ok(());
        for d in &def.subs {
            match crate::parse::create(d, ctx) {
                Ok(a) => nodes.push(Node { def: d.clone(), agg: a }),
                Err(e) => {
                    out = Err(e);
                    break;
                }
            }
        }
        ctx.cardinality = outer;
        ctx.parents.pop();
        out.map(|()| Subs::from_nodes(nodes))
    }

    pub fn from_nodes(nodes: Vec<Node>) -> Subs {
        let needs_scores = nodes.iter().any(|n| n.agg.needs_scores());
        Subs { nodes, docs: Vec::with_capacity(BATCH), ords: Vec::with_capacity(BATCH), scores: Vec::new(), needs_scores }
    }

    pub fn empty() -> Subs {
        Subs::from_nodes(Vec::new())
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn needs_scores(&self) -> bool {
        self.needs_scores
    }

    pub fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        for n in &mut self.nodes {
            n.agg.set_segment(seg)?;
        }
        Ok(())
    }

    /// Queues `(doc, ord)` for the sub-aggregations.
    #[inline]
    pub fn push(&mut self, doc: u32, ord: u32, score: f32) -> Result<()> {
        self.docs.push(doc);
        self.ords.push(ord);
        if self.needs_scores {
            self.scores.push(score);
        }
        if self.docs.len() >= BATCH {
            self.flush()?;
        }
        Ok(())
    }

    /// Hands queued pairs to the sub-aggregations.
    pub fn flush(&mut self) -> Result<()> {
        if self.docs.is_empty() {
            return Ok(());
        }
        let b = Batch { docs: &self.docs, ords: &self.ords, scores: &self.scores };
        for n in &mut self.nodes {
            n.agg.collect(b)?;
        }
        self.docs.clear();
        self.ords.clear();
        self.scores.clear();
        Ok(())
    }

    /// Passes a whole batch through with the same ordinals (single-bucket aggregations whose
    /// bucket ordinal is the owning ordinal).
    pub fn collect_direct(&mut self, b: Batch<'_>) -> Result<()> {
        self.flush()?;
        for n in &mut self.nodes {
            n.agg.collect(b)?;
        }
        Ok(())
    }

    pub fn finish_segment(&mut self) -> Result<()> {
        self.flush()?;
        for n in &mut self.nodes {
            n.agg.finish_segment()?;
        }
        Ok(())
    }

    pub fn post_collection(&mut self) -> Result<()> {
        for n in &mut self.nodes {
            n.agg.post_collection()?;
        }
        Ok(())
    }

    /// The sub-aggregations of each bucket ordinal (`buildSubAggsForAllBuckets`).
    pub fn build(&mut self, ords: &[u32]) -> Result<Vec<Aggs>> {
        let mut out: Vec<Aggs> = (0..ords.len()).map(|_| Aggs { list: Vec::with_capacity(self.nodes.len()) }).collect();
        if ords.is_empty() {
            return Ok(out);
        }
        for n in &mut self.nodes {
            let values = n.agg.build(ords)?;
            debug_assert_eq!(values.len(), ords.len());
            for (i, v) in values.into_iter().enumerate() {
                let a = Agg { name: n.def.name.clone(), meta: n.def.meta.clone(), value: v };
                out[i].list.push(a);
            }
        }
        Ok(out)
    }

    /// Sub-aggregations of a bucket that collected nothing (`buildEmptySubAggregations`).
    pub fn build_empty(&self) -> Aggs {
        Aggs { list: self.nodes.iter().map(|n| n.wrap(n.agg.build_empty())).collect() }
    }

    /// `resolveSortPathOnValidAgg`: validates a sort path starting at one of these.
    pub fn validate_sort(&self, path: &[PathElement]) -> Result<()> {
        let first = &path[0];
        let Some(n) = self.nodes.iter().find(|n| n.def.name == first.name) else {
            return Err(AggError::illegal_argument(format!(
                "The provided aggregation [{}] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets.",
                first.full
            )));
        };
        let rest = &path[1..];
        if !rest.is_empty() && first.key.is_some() {
            return Err(AggError::illegal_argument(format!("Key only allowed on last aggregation path element but got [{}]", first.full)));
        }
        let key = first.key.as_deref().or(first.metric.as_deref());
        n.agg.validate_sort(&n.def.name, key, rest)
    }

    /// The sort value of a validated path for bucket ordinal `ord`.
    pub fn sort_value(&self, path: &[PathElement], ord: u32) -> f64 {
        let first = &path[0];
        match self.nodes.iter().find(|n| n.def.name == first.name) {
            Some(n) => n.agg.sort_value(first.key.as_deref().or(first.metric.as_deref()), &path[1..], ord),
            None => f64::NAN,
        }
    }

    pub fn sort_is_count(&self, path: &[PathElement]) -> bool {
        let first = &path[0];
        match self.nodes.iter().find(|n| n.def.name == first.name) {
            Some(n) => n.agg.sort_is_count(first.key.as_deref().or(first.metric.as_deref()), &path[1..]),
            None => false,
        }
    }
}

/// `SingleBucketAggregator` sort semantics: ordering by a single-bucket aggregation orders by
/// its doc count, or by a path continuing into its sub-aggregations.
pub fn single_bucket_validate_sort(name: &str, subs: &Subs, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
    if !rest.is_empty() {
        return subs.validate_sort(rest);
    }
    match key {
        None | Some("doc_count") => Ok(()),
        Some(_) => Err(AggError::illegal_argument(format!(
            "Ordering on a single-bucket aggregation can only be done on its doc_count. Either drop the key (a la \"{name}\") or change it to \"doc_count\" (a la \"{name}.doc_count\") or \"key\"."
        ))),
    }
}

/// `NumericMetricsAggregator.SingleValue.bucketComparator` validation.
pub fn single_value_validate_sort(name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
    if !rest.is_empty() {
        return Err(not_single_bucket(name));
    }
    if let Some(k) = key
        && k != "value"
    {
        return Err(AggError::illegal_argument(format!(
            "Ordering on a single-value metrics aggregation can only be done on its value. Either drop the key (a la \"{name}\") or change it to \"value\" (a la \"{name}.value\")"
        )));
    }
    Ok(())
}

/// `NumericMetricsAggregator.MultiValue.bucketComparator` validation.
pub fn multi_value_validate_sort(name: &str, key: Option<&str>, rest: &[PathElement], has_metric: impl Fn(&str) -> bool) -> Result<()> {
    if !rest.is_empty() {
        return Err(not_single_bucket(name));
    }
    let Some(k) = key else {
        return Err(AggError::illegal_argument("When ordering on a multi-value metrics aggregation a metric name must be specified."));
    };
    if !has_metric(k) {
        return Err(AggError::illegal_argument(format!("Unknown metric name [{k}] on multi-value metrics aggregation [{name}]")));
    }
    Ok(())
}

fn not_single_bucket(name: &str) -> AggError {
    AggError::illegal_argument(format!(
        "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end. [{name}] is not single-bucket."
    ))
}

/// Options of one shard's aggregation run.
#[derive(Clone, Default)]
pub struct ShardOptions {
    /// The memory budget aggregators reserve from.
    pub budget: Option<Arc<MemoryBudget>>,
    /// The query matches every live doc (`match_all`, no `post_filter`): top-level aggregations
    /// may then read segment statistics instead of docs. [`AggRequest::run`](crate::AggRequest::run)
    /// sets it for a `MatchAllQuery`.
    pub matches_all: bool,
}

/// Collects one shard's aggregations; plug it into a search (alone or in a
/// `MultiCollector`), then call [`finish`](AggCollector::finish).
pub struct AggCollector {
    searcher: Searcher,
    /// Top-level aggregators except `global` ones.
    nodes: Vec<Node>,
    /// `global` aggregators, collected over every doc at finish.
    globals: Vec<Node>,
    /// Request order: (is_global, index).
    order: Vec<(bool, usize)>,
    /// What `global` aggregations collect.
    global_query: Option<search_index::QueryRef>,
    needs_scores: bool,
    docs: Vec<u32>,
    scores: Vec<f32>,
    zeros: Vec<u32>,
}

/// Whether any aggregation of the tree has type `ty`.
fn has_type(defs: &[Arc<AggDef>], ty: &str) -> bool {
    defs.iter().any(|d| d.type_name == ty || has_type(&d.subs, ty))
}

impl AggCollector {
    pub(crate) fn new(req: &AggRequest, lookup: &dyn FieldLookup, searcher: &Searcher, opts: &ShardOptions) -> Result<AggCollector> {
        let mut ctx = BindCtx {
            lookup,
            searcher,
            budget: opts.budget.clone(),
            parents: Vec::new(),
            nested_path: None,
            doc_count_field: lookup.doc_count_field(),
            sized: None,
            sampling: None,
            cardinality: BucketCardinality::ONE,
            nested_agg: None,
            time_series_request: has_type(&req.aggs, "time_series"),
            matches_all: opts.matches_all,
        };
        let mut nodes = Vec::new();
        let mut globals = Vec::new();
        let mut order = Vec::new();
        for d in &req.aggs {
            let agg = crate::parse::create(d, &mut ctx)?;
            if d.type_name == "global" {
                order.push((true, globals.len()));
                globals.push(Node { def: d.clone(), agg });
            } else {
                order.push((false, nodes.len()));
                nodes.push(Node { def: d.clone(), agg });
            }
        }
        let needs_scores = nodes.iter().any(|n| n.agg.needs_scores());
        Ok(AggCollector {
            searcher: searcher.clone(),
            global_query: (!globals.is_empty()).then(|| lookup.global_query()),
            nodes,
            globals,
            order,
            needs_scores,
            docs: Vec::with_capacity(BATCH),
            scores: Vec::with_capacity(BATCH),
            zeros: vec![0; BATCH],
        })
    }

    /// Whether any aggregation reads scores.
    pub fn needs_scores(&self) -> bool {
        self.needs_scores
    }

    fn flush(&mut self) -> Result<()> {
        if self.docs.is_empty() {
            return Ok(());
        }
        let n = self.docs.len();
        let b = Batch { docs: &self.docs, ords: &self.zeros[..n], scores: &self.scores };
        for node in &mut self.nodes {
            node.agg.collect(b)?;
        }
        self.docs.clear();
        self.scores.clear();
        Ok(())
    }

    /// Ends collection: runs `global` aggregations over every doc, then builds the shard's
    /// results in request order.
    pub fn finish(mut self) -> Result<Aggs> {
        for n in &mut self.nodes {
            n.agg.post_collection()?;
        }
        if let Some(q) = self.global_query.take() {
            let mut g = GlobalCollector { nodes: std::mem::take(&mut self.globals), docs: Vec::with_capacity(BATCH), scores: Vec::new(), zeros: vec![0; BATCH] };
            self.searcher.search(&q, &mut g)?;
            for n in &mut g.nodes {
                n.agg.post_collection()?;
            }
            self.globals = g.nodes;
        }
        let mut out = Vec::with_capacity(self.order.len());
        for (is_global, i) in self.order.clone() {
            let node = if is_global { &mut self.globals[i] } else { &mut self.nodes[i] };
            let v = node.agg.build(&[0])?.pop().ok_or_else(|| AggError::illegal_state("aggregator built no result"))?;
            out.push(node.wrap(v));
        }
        Ok(Aggs { list: out })
    }
}

impl Collector for AggCollector {
    fn score_mode(&self) -> ScoreMode {
        if self.needs_scores { ScoreMode::Complete } else { ScoreMode::CompleteNoScores }
    }

    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafCollector + 'a>> {
        for n in &mut self.nodes {
            n.agg.set_segment(seg)?;
        }
        let done = !self.nodes.is_empty() && self.nodes.iter().all(|n| n.agg.segment_done());
        Ok(Box::new(TopLeaf { c: self, done }))
    }
}

struct TopLeaf<'a> {
    c: &'a mut AggCollector,
    /// Every aggregator took the segment from its statistics.
    done: bool,
}

impl LeafCollector for TopLeaf<'_> {
    fn skips(&self) -> bool {
        self.done
    }

    fn competitive_from(&mut self, doc: u32) -> u32 {
        if self.done { search_codec::NO_MORE_DOCS } else { doc }
    }

    #[inline]
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> search_index::Result<()> {
        self.c.docs.push(doc);
        if self.c.needs_scores {
            self.c.scores.push(scorer.score());
        }
        if self.c.docs.len() >= BATCH {
            self.c.flush()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> search_index::Result<()> {
        self.c.flush()?;
        for n in &mut self.c.nodes {
            n.agg.finish_segment()?;
        }
        Ok(())
    }
}

struct GlobalCollector {
    nodes: Vec<Node>,
    docs: Vec<u32>,
    scores: Vec<f32>,
    zeros: Vec<u32>,
}

impl GlobalCollector {
    fn flush(&mut self) -> Result<()> {
        if self.docs.is_empty() {
            return Ok(());
        }
        let n = self.docs.len();
        let b = Batch { docs: &self.docs, ords: &self.zeros[..n], scores: &self.scores };
        for node in &mut self.nodes {
            node.agg.collect(b)?;
        }
        self.docs.clear();
        self.scores.clear();
        Ok(())
    }
}

impl Collector for GlobalCollector {
    fn score_mode(&self) -> ScoreMode {
        if self.nodes.iter().any(|n| n.agg.needs_scores()) { ScoreMode::Complete } else { ScoreMode::CompleteNoScores }
    }

    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafCollector + 'a>> {
        for n in &mut self.nodes {
            n.agg.set_segment(seg)?;
        }
        let scores = self.nodes.iter().any(|n| n.agg.needs_scores());
        Ok(Box::new(GlobalLeaf { c: self, scores }))
    }
}

struct GlobalLeaf<'a> {
    c: &'a mut GlobalCollector,
    scores: bool,
}

impl LeafCollector for GlobalLeaf<'_> {
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> search_index::Result<()> {
        self.c.docs.push(doc);
        if self.scores {
            self.c.scores.push(scorer.score());
        }
        if self.c.docs.len() >= BATCH {
            self.c.flush()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> search_index::Result<()> {
        self.c.flush()?;
        for n in &mut self.c.nodes {
            n.agg.finish_segment()?;
        }
        Ok(())
    }
}

/// Grows a per-ordinal array to hold `ord`, filling with `fill`.
#[inline]
pub fn grow<T: Clone>(v: &mut Vec<T>, ord: u32, fill: T) {
    let need = ord as usize + 1;
    if v.len() < need {
        let new_len = need.max(v.len() + v.len() / 2).max(16);
        v.resize(new_len, fill);
    }
}
