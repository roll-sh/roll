//! `profile: true` for aggregations: the per-shard `profile.shards[].aggregations` tree.
//!
//! When [`ShardOptions::profile`](crate::ShardOptions::profile) is set, every aggregator is
//! wrapped in a [`Profiled`] shim as it is created (`parse::create`), registered in a shared
//! [`Profiler`] under its enclosing aggregation. The shim times the phases Elasticsearch's
//! `AggregationProfileBreakdown` reports — `initialize` (creation), `build_leaf_collector`
//! (per segment), `collect` (per batch, counting docs), `post_collection`,
//! `build_aggregation` — and asks the aggregator for its Elasticsearch class name and debug
//! map ([`Aggregator::profile_type`], [`Aggregator::profile_debug`]). Timing costs two clock
//! reads per batch, and nothing at all when profiling is off. Deferred (breadth-first)
//! sub-aggregations record their replayed collection under their own node, as Elasticsearch
//! does.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch};
use crate::error::Result;
use crate::internal::AggValue;
use crate::json::Json;
use crate::request::PathElement;

/// Timers and counts of one aggregator (`AggregationTimingType`).
#[derive(Clone, Debug, Default)]
struct Node {
    type_name: String,
    description: String,
    nanos: [u64; 6],
    counts: [u64; 6],
    debug: Option<Json>,
    /// Owning buckets built (`built_buckets`, which every aggregator's debug map carries).
    built: u64,
    children: Vec<usize>,
}

/// The phases, in the order Elasticsearch prints them.
const PHASES: [&str; 6] = ["initialize", "build_leaf_collector", "collect", "post_collection", "build_aggregation", "reduce"];
const INITIALIZE: usize = 0;
const LEAF: usize = 1;
const COLLECT: usize = 2;
const POST: usize = 3;
const BUILD: usize = 4;

/// The profile tree of one shard's aggregations.
#[derive(Debug, Default)]
pub struct Profiler {
    nodes: Mutex<Vec<Node>>,
    roots: Mutex<Vec<usize>>,
}

impl Profiler {
    pub fn new() -> Arc<Profiler> {
        Arc::new(Profiler::default())
    }

    /// Registers an aggregation (under `parent`, or at the top level) and returns its id.
    pub(crate) fn register(&self, description: &str, parent: Option<usize>) -> usize {
        let mut nodes = self.nodes.lock().expect("profiler");
        let id = nodes.len();
        nodes.push(Node { description: description.to_string(), ..Default::default() });
        match parent {
            Some(p) => nodes[p].children.push(id),
            None => self.roots.lock().expect("profiler").push(id),
        }
        id
    }

    fn add(&self, id: usize, phase: usize, started: Instant, count: u64) {
        let nanos = started.elapsed().as_nanos() as u64;
        let mut nodes = self.nodes.lock().expect("profiler");
        let n = &mut nodes[id];
        // Elasticsearch reports at least one nanosecond for a phase that ran.
        n.nanos[phase] += nanos.max(1);
        n.counts[phase] += count;
    }

    pub(crate) fn record_initialize(&self, id: usize, started: Instant, agg: &dyn Aggregator) {
        self.add(id, INITIALIZE, started, 1);
        let mut nodes = self.nodes.lock().expect("profiler");
        nodes[id].type_name = agg.profile_type().to_string();
    }

    fn set_debug(&self, id: usize, debug: Json, built: usize) {
        let mut nodes = self.nodes.lock().expect("profiler");
        let n = &mut nodes[id];
        n.built += built as u64;
        let mut debug = if debug.as_object().is_some() { debug } else { Json::obj() };
        // OpenSearch (forked before `built_buckets`) prints only the aggregator's own debug
        // entries, and no `debug` at all when there are none.
        if crate::dialect::opensearch() {
            n.debug = debug.as_object().is_some_and(|o| !o.is_empty()).then_some(debug);
            return;
        }
        debug.push("built_buckets", Json::from(n.built));
        n.debug = Some(debug);
    }

    /// The `aggregations` array of a shard's profile (`AggregationProfileShardResult`).
    pub fn to_json(&self) -> Json {
        let nodes = self.nodes.lock().expect("profiler");
        let roots = self.roots.lock().expect("profiler");
        Json::Arr(roots.iter().map(|&r| node_json(&nodes, r)).collect())
    }
}

fn node_json(nodes: &[Node], id: usize) -> Json {
    let n = &nodes[id];
    let mut o = Json::obj();
    o.push("type", Json::Str(n.type_name.clone()));
    o.push("description", Json::Str(n.description.clone()));
    o.push("time_in_nanos", Json::from(n.nanos.iter().sum::<u64>()));
    let mut b = Json::obj();
    for (i, p) in PHASES.iter().enumerate() {
        b.push(*p, Json::from(n.nanos[i]));
        b.push(format!("{p}_count"), Json::from(n.counts[i]));
    }
    o.push("breakdown", b);
    if let Some(d) = &n.debug {
        o.push("debug", d.clone());
    }
    if !n.children.is_empty() {
        o.push("children", Json::Arr(n.children.iter().map(|&c| node_json(nodes, c)).collect()));
    }
    o
}

/// An aggregator with its phases timed into a [`Profiler`].
pub(crate) struct Profiled {
    pub inner: Box<dyn Aggregator>,
    pub id: usize,
    pub profiler: Arc<Profiler>,
}

impl Aggregator for Profiled {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        let t = Instant::now();
        let r = self.inner.set_segment(seg);
        self.profiler.add(self.id, LEAF, t, 1);
        r
    }
    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let t = Instant::now();
        let r = self.inner.collect(b);
        self.profiler.add(self.id, COLLECT, t, b.len() as u64);
        r
    }
    fn finish_segment(&mut self) -> Result<()> {
        self.inner.finish_segment()
    }
    fn post_collection(&mut self) -> Result<()> {
        let t = Instant::now();
        let r = self.inner.post_collection();
        self.profiler.add(self.id, POST, t, 1);
        r
    }
    fn segment_done(&self) -> bool {
        self.inner.segment_done()
    }
    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let t = Instant::now();
        let r = self.inner.build(owning);
        self.profiler.add(self.id, BUILD, t, 1);
        self.profiler.set_debug(self.id, self.inner.profile_debug(), owning.len());
        r
    }
    fn build_empty(&self) -> AggValue {
        self.inner.build_empty()
    }
    fn needs_scores(&self) -> bool {
        self.inner.needs_scores()
    }
    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        self.inner.validate_sort(name, key, rest)
    }
    fn sort_value(&self, key: Option<&str>, rest: &[PathElement], ord: u32) -> f64 {
        self.inner.sort_value(key, rest, ord)
    }
    fn sort_is_count(&self, key: Option<&str>, rest: &[PathElement]) -> bool {
        self.inner.sort_is_count(key, rest)
    }
    fn profile_type(&self) -> &'static str {
        self.inner.profile_type()
    }
    fn profile_debug(&self) -> Json {
        self.inner.profile_debug()
    }
}
