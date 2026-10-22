//! Query profiling (Elasticsearch's `profile: true`, Lucene's `QueryProfiler`).
//!
//! A searcher built with [`Searcher::with_profiler`](crate::search::Searcher::with_profiler) routes every weight it creates — the top
//! query and every sub-query composite queries build through [`Searcher::weight_of`](crate::search::Searcher::weight_of) — through
//! a [`Profiler`]: weight creation nests profile nodes (the query tree after rewriting), and
//! each node's weight and scorers are wrapped to time `create_weight`, `build_scorer`,
//! `next_doc`, `advance`, `match`, `score`, `shallow_advance`, `compute_max_score`,
//! `set_min_competitive_score` and `count_weight`, with call counts. As in Lucene, profiled
//! weights use plain bulk scoring (no specialized bulk scorers), so the profile describes the
//! profiled execution. Rewrite time is the total spent rewriting. Without a profiler none of
//! this exists: composites call straight into the query.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use search_codec::LiveDocs;

use crate::error::Result;
use crate::search::{
    BulkScorer, Collector, DefaultBulkScorer, DocScoreBuf, Explanation, LeafCollector, Scorable, ScoreMode, Scorer, SegmentContext, Weight,
};

/// Elasticsearch's `QueryTimingType`, in its breakdown order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timing {
    CreateWeight,
    BuildScorer,
    NextDoc,
    Advance,
    Match,
    Score,
    ShallowAdvance,
    ComputeMaxScore,
    SetMinCompetitiveScore,
    CountWeight,
}

impl Timing {
    pub const ALL: [Timing; 10] = [
        Timing::CreateWeight,
        Timing::BuildScorer,
        Timing::NextDoc,
        Timing::Advance,
        Timing::Match,
        Timing::Score,
        Timing::ShallowAdvance,
        Timing::ComputeMaxScore,
        Timing::SetMinCompetitiveScore,
        Timing::CountWeight,
    ];

    /// The breakdown key (`<key>` and `<key>_count`).
    pub fn key(self) -> &'static str {
        match self {
            Timing::CreateWeight => "create_weight",
            Timing::BuildScorer => "build_scorer",
            Timing::NextDoc => "next_doc",
            Timing::Advance => "advance",
            Timing::Match => "match",
            Timing::Score => "score",
            Timing::ShallowAdvance => "shallow_advance",
            Timing::ComputeMaxScore => "compute_max_score",
            Timing::SetMinCompetitiveScore => "set_min_competitive_score",
            Timing::CountWeight => "count_weight",
        }
    }
}

/// The profile class of a query type: Lucene's name where ours differs.
pub(crate) fn class_name(rust_type: &str) -> String {
    let base = rust_type.rsplit("::").next().unwrap_or(rust_type);
    let base = base.split('<').next().unwrap_or(base);
    match base {
        "MatchAllQuery" => "MatchAllDocsQuery",
        "MatchNoneQuery" => "MatchNoDocsQuery",
        "FieldExistsQuery" => "FieldExistsQuery",
        "ToParentQuery" => "ESToParentBlockJoinQuery",
        "ToChildQuery" => "ToChildBlockJoinQuery",
        "ScoreDocQuery" => "KnnScoreDocQuery",
        "GeoDistanceQuery" => "LatLonPointDistanceQuery",
        "GeoShapeQuery" => "LatLonShapeQuery",
        "JoinQuery" => "LateParsingQuery",
        "ResolvedJoin" => "GlobalOrdinalsQuery",
        "BoostingQuery" => "FunctionScoreQuery",
        "SynonymQuery" => "SynonymQuery",
        "DisMaxQuery" => "DisjunctionMaxQuery",
        "CappedScoreQuery" => "CappedScoreQuery",
        "BinaryDvConfirmedQuery" => "BinaryDvConfirmedAutomatonQuery",
        other => other,
    }
    .to_string()
}

/// Timings of one profiled query.
#[derive(Debug, Default)]
pub struct NodeTimings {
    nanos: [AtomicU64; 10],
    counts: [AtomicU64; 10],
}

impl NodeTimings {
    #[inline]
    pub(crate) fn add(&self, t: Timing, start: Instant) {
        let i = t as usize;
        self.nanos[i].fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        self.counts[i].fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct Node {
    kind: String,
    description: String,
    timings: Arc<NodeTimings>,
    children: Vec<usize>,
}

/// A profiled query node (Elasticsearch's `ProfileResult`).
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileNode {
    /// The query's class (`TermQuery`, `BooleanQuery`, …).
    pub kind: String,
    /// The query's string.
    pub description: String,
    /// `(key, nanos, count)` in Elasticsearch's breakdown order.
    pub breakdown: Vec<(&'static str, u64, u64)>,
    /// Sum of the breakdown's times.
    pub time_in_nanos: u64,
    pub children: Vec<ProfileNode>,
}

/// Collects the query profile of the searches run through a profiled searcher.
#[derive(Debug, Default)]
pub struct Profiler {
    nodes: Mutex<Vec<Node>>,
    roots: Mutex<Vec<usize>>,
    /// Nodes whose weights are being created (the parent of the next node).
    stack: Mutex<Vec<usize>>,
    rewrite_nanos: AtomicU64,
}

impl Profiler {
    pub fn new() -> Arc<Profiler> {
        Arc::new(Profiler::default())
    }

    pub(crate) fn add_rewrite(&self, start: Instant) {
        self.rewrite_nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// Opens a node under the weight being created (or as a root).
    pub(crate) fn push(&self, kind: String, description: String) -> (usize, Arc<NodeTimings>) {
        let timings = Arc::new(NodeTimings::default());
        let mut nodes = self.nodes.lock();
        let id = nodes.len();
        nodes.push(Node { kind, description, timings: timings.clone(), children: Vec::new() });
        let mut stack = self.stack.lock();
        match stack.last() {
            Some(&p) => nodes[p].children.push(id),
            None => self.roots.lock().push(id),
        }
        stack.push(id);
        (id, timings)
    }

    pub(crate) fn pop(&self) {
        self.stack.lock().pop();
    }

    /// Total rewrite time.
    pub fn rewrite_time(&self) -> u64 {
        self.rewrite_nanos.load(Ordering::Relaxed)
    }

    /// The profiled query trees, one per top-level weight created.
    pub fn tree(&self) -> Vec<ProfileNode> {
        let nodes = self.nodes.lock();
        let build = |id: usize| -> ProfileNode {
            fn rec(nodes: &[Node], id: usize) -> ProfileNode {
                let n = &nodes[id];
                let breakdown: Vec<(&'static str, u64, u64)> = Timing::ALL
                    .iter()
                    .map(|&t| (t.key(), n.timings.nanos[t as usize].load(Ordering::Relaxed), n.timings.counts[t as usize].load(Ordering::Relaxed)))
                    .collect();
                ProfileNode {
                    kind: n.kind.clone(),
                    description: n.description.clone(),
                    time_in_nanos: breakdown.iter().map(|b| b.1).sum(),
                    breakdown,
                    children: n.children.iter().map(|&c| rec(nodes, c)).collect(),
                }
            }
            rec(&nodes, id)
        };
        self.roots.lock().iter().map(|&r| build(r)).collect()
    }
}

/// A weight timing its node.
pub(crate) struct ProfileWeight {
    pub(crate) inner: Box<dyn Weight>,
    pub(crate) t: Arc<NodeTimings>,
}

impl Weight for ProfileWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let start = Instant::now();
        let s = self.inner.scorer(seg);
        self.t.add(Timing::BuildScorer, start);
        Ok(s?.map(|inner| Box::new(ProfileScorer { inner, t: self.t.clone() }) as Box<dyn Scorer>))
    }

    // Profiled searches score doc at a time, through the timed scorer.
    fn bulk_scorer(&self, seg: &SegmentContext, _mode: ScoreMode) -> Result<Option<Box<dyn BulkScorer>>> {
        Ok(self.scorer(seg)?.map(|scorer| Box::new(DefaultBulkScorer::new(scorer)) as Box<dyn BulkScorer>))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        self.inner.explain(seg, doc)
    }

    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        let start = Instant::now();
        let c = self.inner.count(seg);
        self.t.add(Timing::CountWeight, start);
        c
    }
}

struct ProfileScorer {
    inner: Box<dyn Scorer>,
    t: Arc<NodeTimings>,
}

impl Scorable for ProfileScorer {
    fn score(&mut self) -> f32 {
        let start = Instant::now();
        let s = self.inner.score();
        self.t.add(Timing::Score, start);
        s
    }
    fn set_min_competitive_score(&mut self, m: f32) {
        let start = Instant::now();
        self.inner.set_min_competitive_score(m);
        self.t.add(Timing::SetMinCompetitiveScore, start);
    }
}

impl Scorer for ProfileScorer {
    fn doc(&self) -> u32 {
        self.inner.doc()
    }
    fn next(&mut self) -> u32 {
        let start = Instant::now();
        let d = self.inner.next();
        self.t.add(Timing::NextDoc, start);
        d
    }
    fn advance(&mut self, target: u32) -> u32 {
        let start = Instant::now();
        let d = self.inner.advance(target);
        self.t.add(Timing::Advance, start);
        d
    }
    fn cost(&self) -> u64 {
        self.inner.cost()
    }
    fn has_two_phase(&self) -> bool {
        self.inner.has_two_phase()
    }
    fn approx_next(&mut self) -> u32 {
        let start = Instant::now();
        let d = self.inner.approx_next();
        self.t.add(Timing::NextDoc, start);
        d
    }
    fn approx_advance(&mut self, target: u32) -> u32 {
        let start = Instant::now();
        let d = self.inner.approx_advance(target);
        self.t.add(Timing::Advance, start);
        d
    }
    fn matches(&mut self) -> bool {
        let start = Instant::now();
        let m = self.inner.matches();
        self.t.add(Timing::Match, start);
        m
    }
    fn match_cost(&self) -> f32 {
        self.inner.match_cost()
    }
    fn advance_shallow(&mut self, target: u32) -> u32 {
        let start = Instant::now();
        let d = self.inner.advance_shallow(target);
        self.t.add(Timing::ShallowAdvance, start);
        d
    }
    fn max_score(&mut self, upto: u32) -> f32 {
        let start = Instant::now();
        let s = self.inner.max_score(upto);
        self.t.add(Timing::ComputeMaxScore, start);
        s
    }
    fn freq(&mut self) -> f32 {
        self.inner.freq()
    }
    fn next_docs_and_scores(&mut self, upto: u32, live: Option<&LiveDocs>, buf: &mut DocScoreBuf) {
        // Doc at a time, so every step is timed.
        buf.len = 0;
        let mut d = self.doc();
        while d < upto && buf.len < 64 {
            if live.is_none_or(|l| l.is_live(d)) {
                buf.docs[buf.len] = d;
                buf.scores[buf.len] = self.score();
                buf.len += 1;
            }
            d = self.next();
        }
    }
}

/// Elasticsearch's collector profile (`CollectorResult`): a collector's name, the reason it
/// runs, and the time spent in it (leaf creation and collection, children included).
pub struct ProfileCollector<C> {
    pub inner: C,
    nanos: AtomicU64,
}

impl<C: Collector> ProfileCollector<C> {
    pub fn new(inner: C) -> ProfileCollector<C> {
        ProfileCollector { inner, nanos: AtomicU64::new(0) }
    }

    pub fn time_in_nanos(&self) -> u64 {
        self.nanos.load(Ordering::Relaxed)
    }
}

struct ProfileLeaf<'a> {
    inner: Box<dyn LeafCollector + 'a>,
    nanos: &'a AtomicU64,
}

impl LeafCollector for ProfileLeaf<'_> {
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> Result<()> {
        let start = Instant::now();
        let r = self.inner.collect(doc, scorer);
        self.nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        r
    }
    fn finish(&mut self) -> Result<()> {
        self.inner.finish()
    }
    fn skips(&self) -> bool {
        self.inner.skips()
    }
    fn competitive_from(&mut self, doc: u32) -> u32 {
        self.inner.competitive_from(doc)
    }
}

impl<C: Collector> Collector for ProfileCollector<C> {
    fn score_mode(&self) -> ScoreMode {
        self.inner.score_mode()
    }
    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'a>> {
        let start = Instant::now();
        let inner = self.inner.leaf(seg);
        self.nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Ok(Box::new(ProfileLeaf { inner: inner?, nanos: &self.nanos }))
    }
}
