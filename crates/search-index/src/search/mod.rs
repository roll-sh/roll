//! The query execution model: Lucene's Query → Weight → Scorer → Collector pipeline.
//!
//! - A [`Query`] is immutable and shareable. [`Searcher::rewrite`] rewrites it to primitive
//!   queries (multi-term expansion, boolean simplification) until it stops changing.
//! - A [`Weight`] is a query bound to a searcher: statistics are gathered once (term states,
//!   BM25 weights) and it hands out one [`Scorer`] per segment.
//! - A [`Scorer`] iterates matching docs of one segment in increasing order and scores them.
//!   Expensive matches use two-phase iteration: `approx_next`/`approx_advance` move over a cheap
//!   superset and `matches` confirms (phrases, scripts, doc-value filters), so conjunctions
//!   verify only docs every clause approximates. Scorers expose block-max upper bounds
//!   (`advance_shallow` + `max_score`) and accept a minimum competitive score, which lets
//!   top-k disjunctions skip non-competitive blocks (WAND/MaxScore).
//! - A [`BulkScorer`] drives a whole segment (or a window) into a [`LeafCollector`]; boolean
//!   queries provide specialized bulk scorers (MaxScore for top-k disjunctions, block-max
//!   conjunctions, batched conjunctions that visit every match) and everything else falls
//!   back to iterating its scorer. Bulk scorers are bounded: a window never confirms a match
//!   past its end, so windows and partitions don't scan into each other.
//! - A [`Collector`] decides what to keep (top hits, counts, aggregations in `search-aggs`).
//!
//! Doc ids are segment-local in scorers and collectors; [`Searcher`] maps global ids
//! (`base + doc`) for fetch and explain.
//!
//! Concurrent searches ([`Searcher::search_concurrent`]) split the segments into slices of
//! [`Partition`]s — large segments into several doc id ranges, as Lucene 10 does — and run
//! them on [`pool`]'s on-demand workers plus the calling thread; [`Searcher::count`] iterates
//! the segments [`Weight::count`] can't answer the same way.

pub mod cache;
pub mod explain;
pub mod ordinals;
pub mod pool;
pub mod profile;
pub mod scorers;

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use search_codec::{LiveDocs, NO_MORE_DOCS, SegmentReader};

use crate::error::{Error, Result};
use crate::schema::Schema;
use crate::similarity::{CollectionStats, TermStats};
pub use explain::{ExplValue, Explanation};

thread_local! {
    static DEFERRED: std::cell::RefCell<Option<Error>> = const { std::cell::RefCell::new(None) };
}

/// Records an error raised inside a scorer (a script failing, a function producing an
/// invalid score). Scorers can't return errors from `next`/`score`, so they record it, stop
/// matching, and the searcher reports it after the segment (Lucene throws instead).
pub fn defer_error(e: Error) {
    DEFERRED.with(|d| {
        let mut d = d.borrow_mut();
        if d.is_none() {
            *d = Some(e);
        }
    })
}

/// Takes the error recorded by [`defer_error`] on this thread, if any.
pub fn take_deferred_error() -> Option<Error> {
    DEFERRED.with(|d| d.borrow_mut().take())
}

/// What a search needs from scorers (Lucene `ScoreMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScoreMode {
    /// Every match, with scores.
    Complete,
    /// Every match, no scores.
    CompleteNoScores,
    /// Only the best-scoring matches: scorers may skip docs that can't compete.
    TopScores,
    /// Only the best matches by a sort that is not the score; no scores.
    TopDocs,
    /// As `TopDocs`, with scores.
    TopDocsWithScores,
}

impl ScoreMode {
    #[inline]
    pub fn needs_scores(self) -> bool {
        matches!(self, ScoreMode::Complete | ScoreMode::TopScores | ScoreMode::TopDocsWithScores)
    }

    /// Whether every match must be visited.
    #[inline]
    pub fn is_exhaustive(self) -> bool {
        matches!(self, ScoreMode::Complete | ScoreMode::CompleteNoScores)
    }
}

/// Something that can produce the score of the current doc (handed to collectors).
pub trait Scorable {
    fn score(&mut self) -> f32;
    /// Collectors announce the lowest score that can still enter their results.
    fn set_min_competitive_score(&mut self, _min: f32) {}
}

/// A fixed score (bulk scorers that computed it already).
pub struct ScoreValue(pub f32);

impl Scorable for ScoreValue {
    fn score(&mut self) -> f32 {
        self.0
    }
}

/// Iterates and scores the matches of one query in one segment.
///
/// `doc()` is `u32::MAX` before the first move and [`NO_MORE_DOCS`] at the end. For scorers
/// with `has_two_phase()`, `next`/`advance` return only confirmed matches, while
/// `approx_next`/`approx_advance` return candidates that `matches` must confirm.
pub trait Scorer: Scorable + Send {
    fn doc(&self) -> u32;
    fn next(&mut self) -> u32;
    fn advance(&mut self, target: u32) -> u32;
    /// Upper bound of the number of matches (drives conjunction ordering).
    fn cost(&self) -> u64;

    /// Whether this scorer splits iteration into approximation + confirmation.
    fn has_two_phase(&self) -> bool {
        false
    }
    fn approx_next(&mut self) -> u32 {
        self.next()
    }
    fn approx_advance(&mut self, target: u32) -> u32 {
        self.advance(target)
    }
    /// Confirms the approximation's current doc.
    fn matches(&mut self) -> bool {
        true
    }
    /// Expected cost of `matches` per doc (orders confirmations: cheap first).
    fn match_cost(&self) -> f32 {
        0.0
    }

    /// Moves score bounds (not the iterator) to the block containing `target`; returns the last
    /// doc of the region whose bound [`max_score`](Self::max_score) can describe cheaply.
    fn advance_shallow(&mut self, _target: u32) -> u32 {
        NO_MORE_DOCS
    }
    /// Upper bound of scores of docs from the current position up to `upto` (inclusive).
    fn max_score(&mut self, _upto: u32) -> f32 {
        f32::INFINITY
    }
    /// Frequency-like statistic of the current doc, for explanations (term freq, phrase freq).
    fn freq(&mut self) -> f32 {
        1.0
    }

    /// Fills `buf` with the current doc and the following matches below `upto` that are live,
    /// with their scores, and leaves the scorer on the doc after the batch (Lucene's
    /// `nextDocsAndScores`). The scorer must be positioned. An empty batch means no match is
    /// left below `upto`. Term scorers override this to score a decoded postings block at a
    /// time; the default iterates up to 64 docs.
    fn next_docs_and_scores(&mut self, upto: u32, live: Option<&LiveDocs>, buf: &mut DocScoreBuf) {
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

    /// Keeps the docs of the sorted batch `docs[..len]` this scorer matches, in order, adding
    /// its score to theirs in `scores`, and returns how many remain (Lucene's
    /// `ScorerUtil.applyRequiredClause`). Moves the scorer forward only. Term scorers override
    /// it to advance their cursor and score the survivors without a call per doc.
    fn apply_required(&mut self, docs: &mut [u32], scores: &mut [f64], len: usize) -> usize {
        apply_required_per_doc(self, docs, scores, len)
    }

    /// [`apply_required`](Self::apply_required) for a clause that doesn't score (a filter):
    /// keeps the matching docs and their scores, adding nothing.
    fn apply_filter(&mut self, docs: &mut [u32], scores: &mut [f64], len: usize) -> usize {
        apply_filter_per_doc(self, docs, scores, len)
    }
}

/// [`Scorer::apply_filter`] a doc at a time.
pub fn apply_filter_per_doc<S: Scorer + ?Sized>(s: &mut S, docs: &mut [u32], scores: &mut [f64], len: usize) -> usize {
    let mut cur = s.doc();
    let mut k = 0;
    for j in 0..len {
        let t = docs[j];
        if cur == u32::MAX || cur < t {
            cur = s.advance(t);
        }
        if cur == t {
            docs[k] = t;
            scores[k] = scores[j];
            k += 1;
        }
    }
    k
}

/// [`Scorer::apply_required`] a doc at a time.
pub fn apply_required_per_doc<S: Scorer + ?Sized>(s: &mut S, docs: &mut [u32], scores: &mut [f64], len: usize) -> usize {
    let mut cur = s.doc();
    let mut k = 0;
    for j in 0..len {
        let t = docs[j];
        if cur == u32::MAX || cur < t {
            cur = s.advance(t);
        }
        if cur == t {
            docs[k] = t;
            scores[k] = scores[j] + s.score() as f64;
            k += 1;
        }
    }
    k
}

/// Capacity of a [`DocScoreBuf`] (one postings block).
pub const BATCH: usize = 128;

/// A batch of docs and their scores (Lucene's `DocAndFloatFeatureBuffer`).
pub struct DocScoreBuf {
    pub docs: [u32; BATCH],
    pub scores: [f32; BATCH],
    pub len: usize,
}

impl DocScoreBuf {
    pub fn new() -> Box<DocScoreBuf> {
        Box::new(DocScoreBuf { docs: [0; BATCH], scores: [0.0; BATCH], len: 0 })
    }
}

/// Iterates with `next` until `doc() >= target` (for scorers without a better `advance`).
pub fn slow_advance(s: &mut dyn Scorer, target: u32) -> u32 {
    let mut d = s.doc();
    if d != u32::MAX && d >= target {
        return d;
    }
    loop {
        d = s.next();
        if d >= target {
            return d;
        }
    }
}

/// A scorer with known cost that is built only when needed (lets conjunctions pick the
/// cheapest implementation, e.g. points vs doc values for a range).
pub trait ScorerSupplier {
    fn cost(&self) -> u64;
    /// `lead_cost`: the cost of the clause leading the iteration (`u64::MAX` when this
    /// scorer leads).
    fn get(self: Box<Self>, lead_cost: u64) -> Result<Box<dyn Scorer>>;
}

/// A plain scorer as a supplier.
pub struct ReadyScorer(pub Box<dyn Scorer>);

impl ScorerSupplier for ReadyScorer {
    fn cost(&self) -> u64 {
        self.0.cost()
    }
    fn get(self: Box<Self>, _lead_cost: u64) -> Result<Box<dyn Scorer>> {
        Ok(self.0)
    }
}

/// Scores a range of a segment's docs into a collector.
pub trait BulkScorer {
    /// Collects matches in `min..max` that are live; returns the next candidate doc at or
    /// after `max` (or [`NO_MORE_DOCS`]).
    fn score(&mut self, collector: &mut dyn LeafCollector, live: Option<&LiveDocs>, min: u32, max: u32) -> Result<u32>;
    fn cost(&self) -> u64;
}

/// The default bulk scorer: iterates a scorer.
pub struct DefaultBulkScorer {
    pub scorer: Box<dyn Scorer>,
    /// The scorer sits on an approximation doc nobody confirmed yet (a bounded window of a
    /// two-phase scorer ended there).
    pending: bool,
}

impl DefaultBulkScorer {
    pub fn new(scorer: Box<dyn Scorer>) -> DefaultBulkScorer {
        DefaultBulkScorer { scorer, pending: false }
    }
}

impl BulkScorer for DefaultBulkScorer {
    fn score(&mut self, collector: &mut dyn LeafCollector, live: Option<&LiveDocs>, min: u32, max: u32) -> Result<u32> {
        let s = &mut self.scorer;
        if max != NO_MORE_DOCS && s.has_two_phase() {
            // Lucene's `DefaultBulkScorer` over a `TwoPhaseIterator`: the approximation walks
            // the window and only its docs below `max` are confirmed, so a window (or a
            // partition) never confirms candidates past its end. The scorer stays on the
            // unconfirmed doc at or after `max`, which the next window confirms first.
            let mut d = s.doc();
            if d == u32::MAX || d < min {
                d = s.approx_advance(min);
            }
            let skips = collector.skips();
            while d < max {
                if skips {
                    let t = collector.competitive_from(d);
                    if t > d {
                        d = s.approx_advance(t);
                        continue;
                    }
                }
                if live.is_none_or(|l| l.is_live(d)) && s.matches() {
                    collector.collect(d, s.as_mut())?;
                }
                d = s.approx_next();
            }
            self.pending = true;
            return Ok(d);
        }
        // Unbounded: `next()` confirms inside the scorer (one call per match, not two per
        // candidate).
        let mut d = s.doc();
        if d == u32::MAX || d < min {
            d = s.advance(min);
        } else if std::mem::take(&mut self.pending) && d != NO_MORE_DOCS && !s.matches() {
            d = s.next();
        }
        self.pending = false;
        if collector.skips() {
            return score_skipping(s.as_mut(), collector, live, d, max);
        }
        while d < max {
            if live.is_none_or(|l| l.is_live(d)) {
                collector.collect(d, s.as_mut())?;
            }
            d = s.next();
        }
        Ok(d)
    }

    fn cost(&self) -> u64 {
        self.scorer.cost()
    }
}

/// Iterates `s` from `d` (its current doc) to `max`, jumping over the docs the collector says
/// cannot compete.
pub(crate) fn score_skipping(s: &mut dyn Scorer, collector: &mut dyn LeafCollector, live: Option<&LiveDocs>, mut d: u32, max: u32) -> Result<u32> {
    let t = collector.competitive_from(d);
    if t > d {
        d = s.advance(t);
    }
    while d < max {
        if live.is_none_or(|l| l.is_live(d)) {
            collector.collect(d, s)?;
        }
        let t = collector.competitive_from(d + 1);
        d = if t > d + 1 { s.advance(t) } else { s.next() };
    }
    Ok(d)
}

/// A query bound to a searcher.
pub trait Weight: Send + Sync {
    /// The scorer of a segment, `None` when nothing can match there.
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>>;

    /// A deferred scorer with its cost.
    fn scorer_supplier(&self, seg: &SegmentContext) -> Result<Option<Box<dyn ScorerSupplier>>> {
        Ok(self.scorer(seg)?.map(|s| Box::new(ReadyScorer(s)) as Box<dyn ScorerSupplier>))
    }

    /// A bulk scorer of a segment (specialized by boolean queries).
    fn bulk_scorer(&self, seg: &SegmentContext, _mode: ScoreMode) -> Result<Option<Box<dyn BulkScorer>>> {
        Ok(self.scorer(seg)?.map(|scorer| Box::new(DefaultBulkScorer::new(scorer)) as Box<dyn BulkScorer>))
    }

    /// Why `doc` (segment-local) matches or not, with its score.
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation>;

    /// The number of matches in the segment, if it can be known without iterating (live docs
    /// accounted for).
    fn count(&self, _seg: &SegmentContext) -> Result<Option<u32>> {
        Ok(None)
    }
}

/// Lucene's `QueryVisitor`, reduced to what highlighting and more-like-this need.
/// Whether a term matches (multi-term queries hand these to visitors).
pub type TermMatcher = Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

/// One entry of a [`SegmentCore`] cache: kind, key, value, approximate bytes.
type CacheEntry = (&'static str, u64, Arc<dyn Any + Send + Sync>, usize);

pub trait QueryVisitor {
    /// A term the query matches on `field`.
    fn term(&mut self, field: u32, term: &[u8]);
    /// A query on `field` that matches terms by an automaton or other logic (highlighters
    /// re-run it over the doc's terms).
    fn multi_term(&mut self, _field: u32, _matcher: TermMatcher) {}
    /// A phrase: terms per relative position (several terms at one position for
    /// multi-phrases) and its slop. Phrase queries report this instead of their terms.
    fn phrase(&mut self, field: u32, slots: &[(u32, Vec<Vec<u8>>)], _slop: u32) {
        for (_, ts) in slots {
            for t in ts {
                self.term(field, t);
            }
        }
    }
    /// Whether to descend into must-not clauses (default no).
    fn wants_negated(&self) -> bool {
        false
    }
    /// A multi-term query (prefix, wildcard, regexp, fuzzy, range). The default reports its
    /// matcher through [`multi_term`](Self::multi_term); visitors that expand it against the
    /// index (the fast vector highlighter) take the query itself.
    fn multi_term_query(&mut self, q: &crate::query::MultiTermQuery) {
        let m = q.clone();
        self.multi_term(q.field, Arc::new(move |t: &[u8]| m.matches_term(t)));
    }
    /// Entering a boosted sub-query (`BoostQuery`); paired with [`end_boost`](Self::end_boost).
    fn begin_boost(&mut self, _boost: f32) {}
    fn end_boost(&mut self) {}
}

/// A query.
pub trait Query: Send + Sync + std::fmt::Debug + Any {
    /// Creates the weight. `boost` multiplies scores.
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>>;

    /// A simpler equivalent query, or `None` when this one is primitive.
    fn rewrite(&self, _s: &Searcher) -> Result<Option<QueryRef>> {
        Ok(None)
    }

    /// Lucene's `toString("")` (explanation descriptions embed it).
    fn lucene_string(&self, schema: &Schema) -> String;

    /// Reports the terms the query matches.
    fn visit(&self, _v: &mut dyn QueryVisitor) {}

    /// Feeds an exact identity of the docs this query matches (scoring aside) into `h`, for
    /// filter cache keys; false when the query can't be cached (the default: scripts,
    /// automata, anything whose doc set its fields don't pin down).
    fn cache_identity(&self, _h: &mut dyn std::hash::Hasher) -> bool {
        false
    }

    /// The query's class in profiles (Lucene's / Elasticsearch's simple class name).
    fn profile_name(&self) -> String {
        profile::class_name(std::any::type_name_of_val(self))
    }

    fn as_any(&self) -> &dyn Any;
}

/// Hashes a type tag and a value's `Debug` output (exact for plain data) into `h`.
pub(crate) fn hash_debug(h: &mut dyn std::hash::Hasher, tag: &str, v: &dyn std::fmt::Debug) -> bool {
    struct W<'a>(&'a mut dyn std::hash::Hasher);
    impl std::fmt::Write for W<'_> {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            self.0.write(s.as_bytes());
            Ok(())
        }
    }
    hash_tag(h, tag);
    use std::fmt::Write;
    let _ = write!(W(h), "{v:?}");
    true
}

/// Starts a query's identity: its type tag.
pub(crate) fn hash_tag(h: &mut dyn std::hash::Hasher, tag: &str) {
    h.write(tag.as_bytes());
    h.write_u8(0xff);
}

/// Hashes a byte string with its length.
pub(crate) fn hash_bytes(h: &mut dyn std::hash::Hasher, b: &[u8]) {
    h.write_usize(b.len());
    h.write(b);
}

pub type QueryRef = Arc<dyn Query>;

/// Collects the matches of one segment.
pub trait LeafCollector {
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> Result<()>;
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
    /// Whether [`competitive_from`](Self::competitive_from) can skip docs (Lucene's
    /// `competitiveIterator`); bulk scorers ask once per segment and skip only then.
    fn skips(&self) -> bool {
        false
    }
    /// The first doc at or after `doc` that may still be collected usefully
    /// ([`NO_MORE_DOCS`]: none); docs before it would not change the results (sorted top
    /// hits whose queue is full skip docs with non-competitive values).
    fn competitive_from(&mut self, doc: u32) -> u32 {
        doc
    }
}

/// Collects matches across segments.
pub trait Collector {
    fn score_mode(&self) -> ScoreMode;
    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'a>>;
}

/// Per-segment caches shared by every snapshot holding the segment (keyed by the cache user).
pub struct SegmentCore {
    pub uid: u64,
    pub reader: SegmentReader,
    cache: parking_lot::Mutex<Vec<CacheEntry>>,
    /// Set while the segment's file is still downloading (`shard::gate`).
    pub(crate) gate: Option<Arc<crate::shard::gate::Gate>>,
}

impl SegmentCore {
    pub fn new(uid: u64, reader: SegmentReader) -> SegmentCore {
        SegmentCore { uid, reader, cache: parking_lot::Mutex::new(Vec::new()), gate: None }
    }

    pub(crate) fn with_gate(uid: u64, reader: SegmentReader, gate: Option<Arc<crate::shard::gate::Gate>>) -> SegmentCore {
        SegmentCore { gate, ..SegmentCore::new(uid, reader) }
    }

    /// Blocks until the postings of `field` are readable (segments still downloading).
    #[inline]
    pub fn ensure_postings(&self, field: u32) -> Result<()> {
        match &self.gate {
            None => Ok(()),
            Some(g) => g.ensure_postings(field),
        }
    }

    /// Blocks until the positions (and offsets, payloads) of `field` are readable.
    #[inline]
    pub fn ensure_positions(&self, field: u32) -> Result<()> {
        match &self.gate {
            None => Ok(()),
            Some(g) => g.ensure_positions(field),
        }
    }

    /// Blocks until stored documents are readable (segments still downloading).
    #[inline]
    pub fn ensure_stored(&self) -> Result<()> {
        match &self.gate {
            None => Ok(()),
            Some(g) => g.ensure_stored(),
        }
    }

    /// Whether every byte of the segment file is readable.
    pub fn is_complete(&self) -> bool {
        self.gate.as_ref().is_none_or(|g| g.is_complete())
    }

    /// A cached value for `(kind, key)`, built on first use.
    pub fn cached<T: Any + Send + Sync>(&self, kind: &'static str, key: u64, build: impl FnOnce() -> T) -> Arc<T> {
        if let Some(v) = self.cache_get::<T>(kind, key) {
            return v;
        }
        let v = Arc::new(build());
        self.cache_put(kind, key, v.clone(), std::mem::size_of::<T>() + 64, 64);
        v
    }

    /// The cached value for `(kind, key)`, if any.
    pub fn cache_get<T: Any + Send + Sync>(&self, kind: &'static str, key: u64) -> Option<Arc<T>> {
        let c = self.cache.lock();
        for (k, kk, v, _) in c.iter() {
            if *k == kind && *kk == key
                && let Ok(v) = v.clone().downcast::<T>()
            {
                return Some(v);
            }
        }
        None
    }

    /// Stores `(kind, key)`, evicting the oldest entries of `kind` beyond `max_of_kind`.
    pub fn cache_put<T: Any + Send + Sync>(&self, kind: &'static str, key: u64, v: Arc<T>, bytes: usize, max_of_kind: usize) {
        let mut c = self.cache.lock();
        if c.iter().any(|(k, kk, ..)| *k == kind && *kk == key) {
            return;
        }
        let n = c.iter().filter(|(k, ..)| *k == kind).count();
        if n >= max_of_kind
            && let Some(i) = c.iter().position(|(k, ..)| *k == kind)
        {
            c.remove(i);
        }
        c.push((kind, key, v as Arc<dyn Any + Send + Sync>, bytes));
    }

    /// Approximate heap bytes of the cached values.
    pub fn cache_bytes(&self) -> usize {
        self.cache.lock().iter().map(|e| e.3).sum()
    }
}

/// One segment of a point-in-time searcher.
#[derive(Clone)]
pub struct SegmentContext {
    /// Position in the searcher's segment list.
    pub ord: u32,
    /// Global doc id of this segment's doc 0.
    pub base: u32,
    pub reader: SegmentReader,
    /// `None`: no deletions.
    pub live: Option<Arc<LiveDocs>>,
    pub num_docs: u32,
    pub core: Arc<SegmentCore>,
}

impl SegmentContext {
    #[inline]
    pub fn max_doc(&self) -> u32 {
        self.reader.max_doc()
    }

    #[inline]
    pub fn is_live(&self, doc: u32) -> bool {
        self.live.as_ref().is_none_or(|l| l.is_live(doc))
    }

    pub fn has_deletions(&self) -> bool {
        self.live.is_some()
    }

    /// Blocks until the doc streams (docs, freqs, impacts) of `field` are readable. Every
    /// reader of postings calls this (or [`ensure_positions`](Self::ensure_positions)) first:
    /// a segment opened while still downloading
    /// ([`SegmentHandle::fetching`](crate::shard::SegmentHandle::fetching)) has only its term
    /// dictionaries, doc values, points and stored-fields index in at open. Free otherwise.
    #[inline]
    pub fn ensure_postings(&self, field: u32) -> Result<()> {
        self.core.ensure_postings(field)
    }

    /// [`ensure_postings`](Self::ensure_postings) plus positions, offsets and payloads: for
    /// cursors opened with `PostingsFlags::POSITIONS` or more.
    #[inline]
    pub fn ensure_positions(&self, field: u32) -> Result<()> {
        self.core.ensure_positions(field)
    }

    /// Blocks until stored documents are readable; call before `reader.stored()` loads one.
    #[inline]
    pub fn ensure_stored(&self) -> Result<()> {
        self.core.ensure_stored()
    }
}

/// Point value bounds of a field across a searcher's segments ([`Searcher::point_bounds`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointBounds {
    /// Per-dimension minimum, packed (sortable bytes).
    pub min: Vec<u8>,
    /// Per-dimension maximum, packed.
    pub max: Vec<u8>,
    /// Docs with a point (deleted docs included).
    pub doc_count: u64,
}

/// Limits applied while building queries.
#[derive(Clone, Copy, Debug)]
pub struct SearchLimits {
    /// `indices.query.bool.max_clause_count` (Lucene `IndexSearcher.maxClauseCount`).
    pub max_clause_count: usize,
    /// The fewest docs worth a thread: a concurrent search uses up to
    /// [`parallelism`](Self::parallelism) slices of at least this many docs, splitting
    /// segments of two slices' worth or more into doc id ranges.
    pub parallel_min_docs: u32,
    pub parallelism: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        SearchLimits { max_clause_count: 4096, parallel_min_docs: 250_000, parallelism: 1 }
    }
}

/// Deadline and cancellation of one search.
#[derive(Clone, Default)]
pub struct SearchControl {
    pub deadline: Option<Instant>,
    pub cancelled: Option<Arc<AtomicBool>>,
}

impl SearchControl {
    #[inline]
    pub fn check(&self) -> Result<()> {
        if let Some(c) = &self.cancelled
            && c.load(Ordering::Relaxed)
        {
            return Err(Error::Cancelled);
        }
        if let Some(d) = self.deadline
            && Instant::now() >= d
        {
            return Err(Error::TimeExceeded);
        }
        Ok(())
    }

    fn is_noop(&self) -> bool {
        self.deadline.is_none() && self.cancelled.is_none()
    }
}

struct SearcherInner {
    segments: Vec<SegmentContext>,
    schema: Arc<Schema>,
    max_doc: u32,
    num_docs: u32,
    limits: SearchLimits,
    generation: u64,
    query_cache: Option<Arc<cache::QueryCachePolicy>>,
    /// Global ordinals per field, built on first use (shared by every derived view).
    ordinals: Arc<parking_lot::Mutex<HashMap<u32, Option<Arc<ordinals::GlobalOrdinals>>>>>,
    /// Set on profiled views: weights are timed per query.
    profiler: Option<Arc<profile::Profiler>>,
}

/// A point-in-time view of a shard: segments with their live docs, the schema they were
/// written with, and statistics. Cheap to clone; searches never block writers.
#[derive(Clone)]
pub struct Searcher {
    inner: Arc<SearcherInner>,
}

impl std::fmt::Debug for Searcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Searcher")
            .field("segments", &self.inner.segments.len())
            .field("max_doc", &self.inner.max_doc)
            .field("num_docs", &self.inner.num_docs)
            .finish()
    }
}

impl Searcher {
    /// A searcher over `(core, live docs)` pairs, in order.
    pub fn new(segs: Vec<(Arc<SegmentCore>, Option<Arc<LiveDocs>>)>, schema: Arc<Schema>, generation: u64) -> Searcher {
        let mut segments = Vec::with_capacity(segs.len());
        let mut base = 0u32;
        let mut num_docs = 0u32;
        for (i, (core, live)) in segs.into_iter().enumerate() {
            let reader = core.reader.clone();
            let md = reader.max_doc();
            let live = live.filter(|l| l.num_deleted() > 0);
            let nd = live.as_ref().map_or(md, |l| l.num_live());
            segments.push(SegmentContext { ord: i as u32, base, reader, live, num_docs: nd, core });
            // A shard holds at most `shard::MAX_DOCS` docs, so global doc ids fit; wrapping
            // would point hits at other docs, so overflowing is a bug, never silent.
            base = base.checked_add(md).expect("global doc ids exceed u32 (more than shard::MAX_DOCS docs)");
            num_docs += nd;
        }
        Searcher {
            inner: Arc::new(SearcherInner {
                segments,
                schema,
                max_doc: base,
                num_docs,
                limits: SearchLimits::default(),
                generation,
                query_cache: None,
                ordinals: Arc::default(),
                profiler: None,
            }),
        }
    }

    /// The same view with other limits.
    pub fn with_limits(&self, limits: SearchLimits) -> Searcher {
        let i = &self.inner;
        Searcher {
            inner: Arc::new(SearcherInner {
                segments: i.segments.clone(),
                schema: i.schema.clone(),
                max_doc: i.max_doc,
                num_docs: i.num_docs,
                limits,
                generation: i.generation,
                query_cache: i.query_cache.clone(),
                ordinals: i.ordinals.clone(),
                profiler: i.profiler.clone(),
            }),
        }
    }

    /// The same segments read with a newer schema (fields added since the snapshot).
    pub fn with_schema(&self, schema: Arc<Schema>) -> Searcher {
        let i = &self.inner;
        Searcher {
            inner: Arc::new(SearcherInner {
                segments: i.segments.clone(),
                schema,
                max_doc: i.max_doc,
                num_docs: i.num_docs,
                limits: i.limits,
                generation: i.generation,
                query_cache: i.query_cache.clone(),
                ordinals: i.ordinals.clone(),
                profiler: i.profiler.clone(),
            }),
        }
    }

    /// The same view using a filter cache policy (`None`: no caching).
    pub fn with_query_cache(&self, policy: Option<Arc<cache::QueryCachePolicy>>) -> Searcher {
        let i = &self.inner;
        Searcher {
            inner: Arc::new(SearcherInner {
                segments: i.segments.clone(),
                schema: i.schema.clone(),
                max_doc: i.max_doc,
                num_docs: i.num_docs,
                limits: i.limits,
                generation: i.generation,
                query_cache: policy,
                ordinals: i.ordinals.clone(),
                profiler: i.profiler.clone(),
            }),
        }
    }

    /// The filter cache policy, when caching is on.
    pub fn query_cache(&self) -> Option<&Arc<cache::QueryCachePolicy>> {
        self.inner.query_cache.as_ref()
    }

    /// The weight of a non-scoring clause, through the filter cache when one is configured.
    pub fn filter_weight(&self, q: &QueryRef, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let w = self.weight_of(q, mode, boost)?;
        if mode.needs_scores() {
            return Ok(w);
        }
        Ok(cache::maybe_cache(self, q, w))
    }

    pub fn segments(&self) -> &[SegmentContext] {
        &self.inner.segments
    }

    pub fn schema(&self) -> &Schema {
        &self.inner.schema
    }

    pub fn schema_arc(&self) -> &Arc<Schema> {
        &self.inner.schema
    }

    /// The same view, profiling every query it runs into `profiler`
    /// ([`profile`]); `None` stops profiling.
    pub fn with_profiler(&self, profiler: Option<Arc<profile::Profiler>>) -> Searcher {
        let i = &self.inner;
        Searcher {
            inner: Arc::new(SearcherInner {
                segments: i.segments.clone(),
                schema: i.schema.clone(),
                max_doc: i.max_doc,
                num_docs: i.num_docs,
                limits: i.limits,
                generation: i.generation,
                query_cache: i.query_cache.clone(),
                ordinals: i.ordinals.clone(),
                profiler,
            }),
        }
    }

    pub fn profiler(&self) -> Option<&Arc<profile::Profiler>> {
        self.inner.profiler.as_ref()
    }

    /// The weight of a (rewritten) query; composite queries build their sub-queries' weights
    /// through this, so a profiled view times each one as a node of the query tree.
    pub fn weight_of(&self, q: &QueryRef, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let Some(p) = &self.inner.profiler else { return q.weight(self, mode, boost) };
        let (_, timings) = p.push(q.profile_name(), q.lucene_string(self.schema()));
        let start = std::time::Instant::now();
        let w = q.weight(self, mode, boost);
        p.pop();
        let w = w?;
        timings.add(profile::Timing::CreateWeight, start);
        Ok(Box::new(profile::ProfileWeight { inner: w, t: timings }))
    }

    /// Global ordinals of a sorted / sorted-set doc values field over this view's segments,
    /// built on first use and cached with the view; `None` when no segment has the field.
    pub fn global_ordinals(&self, field: u32) -> Option<Arc<ordinals::GlobalOrdinals>> {
        if let Some(g) = self.inner.ordinals.lock().get(&field) {
            return g.clone();
        }
        // Built outside the lock: other fields stay available meanwhile (a racing builder of
        // the same field just loses its copy).
        let g = ordinals::GlobalOrdinals::build(self, field).map(Arc::new);
        self.inner.ordinals.lock().entry(field).or_insert(g).clone()
    }

    pub fn limits(&self) -> &SearchLimits {
        &self.inner.limits
    }

    /// Docs including deleted ones (Lucene `maxDoc`).
    pub fn max_doc(&self) -> u32 {
        self.inner.max_doc
    }

    /// Live docs (Lucene `numDocs`), nested children included.
    pub fn num_docs(&self) -> u32 {
        self.inner.num_docs
    }

    /// Increases with every refresh that produced this view.
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    /// The segment holding global doc `doc` and the local id.
    pub fn segment_of(&self, doc: u32) -> Option<(&SegmentContext, u32)> {
        let segs = &self.inner.segments;
        let i = segs.partition_point(|s| s.base <= doc);
        if i == 0 {
            return None;
        }
        let s = &segs[i - 1];
        let local = doc - s.base;
        (local < s.max_doc()).then_some((s, local))
    }

    /// Statistics of `field` across segments; `None` when no doc has the field
    /// (Lucene returns null then and similarities are not used).
    pub fn collection_stats(&self, field: u32) -> Option<CollectionStats> {
        let mut st = CollectionStats { max_doc: self.inner.max_doc as u64, doc_count: 0, sum_total_term_freq: 0, sum_doc_freq: 0 };
        for s in &self.inner.segments {
            if let Some(t) = s.reader.terms(field) {
                let ts = t.stats();
                st.doc_count += ts.doc_count as u64;
                st.sum_doc_freq += ts.sum_doc_freq;
                st.sum_total_term_freq += ts.sum_total_term_freq;
            }
        }
        (st.doc_count > 0).then_some(st)
    }

    /// Per-dimension minimum and maximum packed point values of `field` across segments and
    /// the number of docs with a point (deleted docs included): Lucene's
    /// `PointValues.getMinPackedValue / getMaxPackedValue / getDocCount(reader, field)`, which
    /// Elasticsearch's `isFieldWithinQuery` uses to rewrite ranges covering every value.
    /// `None` when no segment has points for the field.
    pub fn point_bounds(&self, field: u32) -> Option<PointBounds> {
        let mut out: Option<PointBounds> = None;
        for s in &self.inner.segments {
            let Some(p) = s.reader.points(field) else { continue };
            let bpd = p.spec().bytes_per_dim as usize;
            let (lo, hi) = (p.min_packed_value(), p.max_packed_value());
            match out.as_mut() {
                None => out = Some(PointBounds { min: lo.to_vec(), max: hi.to_vec(), doc_count: p.doc_count() as u64 }),
                Some(b) => {
                    for d in 0..lo.len() / bpd.max(1) {
                        let r = d * bpd..(d + 1) * bpd;
                        if lo[r.clone()] < b.min[r.clone()] {
                            b.min[r.clone()].copy_from_slice(&lo[r.clone()]);
                        }
                        if hi[r.clone()] > b.max[r.clone()] {
                            b.max[r.clone()].copy_from_slice(&hi[r]);
                        }
                    }
                    b.doc_count += p.doc_count() as u64;
                }
            }
        }
        out
    }

    /// Statistics of one term across segments (deleted docs included, as in Lucene).
    pub fn term_stats(&self, field: u32, term: &[u8]) -> TermStats {
        let mut t = TermStats { doc_freq: 0, total_term_freq: 0 };
        let h = search_codec::bloom::hash64(term);
        for s in &self.inner.segments {
            if let Some(tr) = s.reader.terms(field)
                && let Some(st) = tr.seek_exact_hashed(term, h)
            {
                t.doc_freq += st.doc_freq as u64;
                t.total_term_freq += st.total_term_freq;
            }
        }
        t
    }

    /// Rewrites until the query is primitive.
    pub fn rewrite(&self, q: &QueryRef) -> Result<QueryRef> {
        let mut cur = q.clone();
        for _ in 0..64 {
            match cur.rewrite(self)? {
                Some(n) => cur = n,
                None => return Ok(cur),
            }
        }
        Ok(cur)
    }

    /// Rewrites and builds the weight.
    pub fn create_weight(&self, q: &QueryRef, mode: ScoreMode) -> Result<Box<dyn Weight>> {
        let start = std::time::Instant::now();
        let q = self.rewrite(q)?;
        if let Some(p) = &self.inner.profiler {
            p.add_rewrite(start);
        }
        self.weight_of(&q, mode, 1.0)
    }

    /// Runs `q` into `collector` over every segment.
    pub fn search(&self, q: &QueryRef, collector: &mut dyn Collector) -> Result<()> {
        self.search_with(q, collector, &SearchControl::default())
    }

    /// [`search`](Self::search) with a deadline / cancellation. On timeout the collector keeps
    /// what it collected and `Err(TimeExceeded)` is returned.
    pub fn search_with(&self, q: &QueryRef, collector: &mut dyn Collector, ctl: &SearchControl) -> Result<()> {
        let w = self.create_weight(q, collector.score_mode())?;
        self.search_weight(w.as_ref(), collector, ctl)
    }

    /// Runs a weight into a collector.
    pub fn search_weight(&self, w: &dyn Weight, collector: &mut dyn Collector, ctl: &SearchControl) -> Result<()> {
        search_segments(&self.inner.segments, w, collector, ctl)
    }

    /// Runs `q` with one collector per slice of segments, on up to
    /// [`SearchLimits::parallelism`] threads (Lucene's concurrent search with a
    /// `CollectorManager`, intra-segment partitions included: see [`slices`](Self::slices)).
    /// A single slice runs on the calling thread.
    pub fn search_concurrent<M: CollectorManager>(&self, q: &QueryRef, m: &M, ctl: &SearchControl) -> Result<M::Out> {
        let w = self.create_weight(q, m.score_mode())?;
        let slices = self.slices();
        // Workers run nested concurrent searches inline: they never wait on each other.
        if slices.len() <= 1 || pool::in_worker() {
            let mut c = m.new_collector()?;
            search_segments(&self.inner.segments, w.as_ref(), &mut c, ctl)?;
            return m.reduce(vec![c]);
        }
        let collectors = m.new_collectors(slices.len())?;
        let cs = run_slices(Arc::from(w), slices, collectors, ctl)?;
        m.reduce(cs)
    }

    /// How [`search_concurrent`](Self::search_concurrent) splits this searcher: one entry per
    /// slice (thread), each a list of segment partitions in index order.
    pub fn slices(&self) -> Vec<Vec<Partition>> {
        let lim = &self.inner.limits;
        let segs = &self.inner.segments;
        let total: u64 = segs.iter().map(|s| s.reader.max_doc() as u64).sum();
        let per_min = (lim.parallel_min_docs as u64).max(1);
        let k = (total / per_min).min(lim.parallelism as u64) as usize;
        if k <= 1 {
            return vec![segs.iter().map(Partition::whole).collect()];
        }
        // About `total / k` docs per slice. Segments of at least two slices' worth split into
        // equal doc ranges, one slice each; the others go, largest first, to the slice with
        // the fewest docs (a new one while there are fewer than `k`).
        let per = total.div_ceil(k as u64);
        let mut order: Vec<&SegmentContext> = segs.iter().collect();
        order.sort_by_key(|s| (std::cmp::Reverse(s.reader.max_doc()), s.ord));
        let mut slices: Vec<(u64, Vec<Partition>)> = Vec::new();
        let mut rest = Vec::new();
        for s in order {
            let md = s.reader.max_doc() as u64;
            let n = ((md / per) as usize).min(k - slices.len());
            if n < 2 {
                rest.push(s);
                continue;
            }
            for i in 0..n as u64 {
                let lo = (md * i / n as u64) as u32;
                let hi = if i + 1 == n as u64 { NO_MORE_DOCS } else { (md * (i + 1) / n as u64) as u32 };
                slices.push(((hi.min(md as u32) - lo) as u64, vec![Partition { seg: s.clone(), min: lo, max: hi }]));
            }
        }
        for s in rest {
            let md = s.reader.max_doc() as u64;
            if slices.len() < k {
                slices.push((md, vec![Partition::whole(s)]));
            } else {
                let i = (0..slices.len()).min_by_key(|&i| slices[i].0).unwrap();
                slices[i].0 += md;
                slices[i].1.push(Partition::whole(s));
            }
        }
        // Each slice visits its partitions in index order (collectors break ties by doc id).
        let mut out: Vec<Vec<Partition>> = slices.into_iter().map(|(_, p)| p).filter(|p| !p.is_empty()).collect();
        for s in &mut out {
            s.sort_by_key(|p| (p.seg.ord, p.min));
        }
        out
    }
}

/// Runs `slices[i]` into `collectors[i]`: the first on the calling thread, the others on pool
/// workers.
fn run_slices<C: Collector + Send + 'static>(
    w: Arc<dyn Weight>,
    slices: Vec<Vec<Partition>>,
    collectors: Vec<C>,
    ctl: &SearchControl,
) -> Result<Vec<C>> {
    let n = slices.len();
    debug_assert_eq!(n, collectors.len());
    let mut collectors = collectors.into_iter();
    let mut first = collectors.next().expect("one collector per slice");
    let mut slices = slices.into_iter();
    let mine_parts = slices.next().expect("one slice");
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Result<C>)>();
    for (i, (parts, mut c)) in slices.zip(collectors).enumerate() {
        let (w, ctl, tx, hold) = (w.clone(), ctl.clone(), tx.clone(), tx.clone());
        pool::execute_held(
            Box::new(move || {
                let r = search_partitions(&parts, w.as_ref(), &mut c, &ctl).map(|_| c);
                let _ = tx.send((i + 1, r));
            }),
            Box::new(hold),
        );
    }
    drop(tx);
    // The calling thread takes the first slice instead of waiting idle.
    let mine = search_partitions(&mine_parts, w.as_ref(), &mut first, ctl).map(|_| first);
    let mut results: Vec<Option<Result<C>>> = (0..n).map(|_| None).collect();
    results[0] = Some(mine);
    // Until every hold has dropped, not just until every result is in: the workers are then
    // back on the pool's idle stack, and the next search wakes them instead of colder ones or
    // new threads. (Letting this thread also run slices no worker has started yet
    // measured slower: it serializes slices a worker would have taken a moment later.)
    for (i, r) in rx.iter() {
        results[i] = Some(r);
    }
    let mut cs = Vec::with_capacity(n);
    for r in results {
        // A slice whose job panicked never reports.
        cs.push(r.unwrap_or_else(|| Err(Error::illegal_state("search thread panicked")))?);
    }
    Ok(cs)
}

/// Creates the per-slice collectors of a concurrent search and merges their results.
pub trait CollectorManager: Sync {
    /// Slices run on pool workers, so collectors own their state.
    type C: Collector + Send + 'static;
    type Out;
    fn score_mode(&self) -> ScoreMode;
    fn new_collector(&self) -> Result<Self::C>;
    /// The collectors of one search (one per slice); managers whose collectors share state
    /// during a search (hit counts, score bars) create it fresh here.
    fn new_collectors(&self, n: usize) -> Result<Vec<Self::C>> {
        (0..n).map(|_| self.new_collector()).collect()
    }
    fn reduce(&self, collectors: Vec<Self::C>) -> Result<Self::Out>;
}

/// Runs a weight into a collector over `segments`.
pub fn search_segments(segments: &[SegmentContext], w: &dyn Weight, collector: &mut dyn Collector, ctl: &SearchControl) -> Result<()> {
    let parts: Vec<Partition> = segments.iter().map(Partition::whole).collect();
    search_partitions(&parts, w, collector, ctl)
}

/// The docs `min..max` of one segment: the unit of work of a concurrent search (Lucene's
/// `LeafReaderContextPartition`). Large segments split into several, searched by different
/// slices, so a shard merged down to one segment still uses every thread.
#[derive(Clone)]
pub struct Partition {
    pub seg: SegmentContext,
    pub min: u32,
    /// Exclusive; [`NO_MORE_DOCS`] for "to the end".
    pub max: u32,
}

impl Partition {
    pub fn whole(seg: &SegmentContext) -> Partition {
        Partition { seg: seg.clone(), min: 0, max: NO_MORE_DOCS }
    }
}

impl std::fmt::Debug for Partition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "seg {} [{}, {})", self.seg.ord, self.min, self.max)
    }
}

/// Runs a weight into a collector over segment partitions. A collector sees each segment
/// once per call (`leaf` per partition; never two partitions of one segment in a slice).
pub fn search_partitions(parts: &[Partition], w: &dyn Weight, collector: &mut dyn Collector, ctl: &SearchControl) -> Result<()> {
    let mode = collector.score_mode();
    take_deferred_error();
    for part in parts {
        let seg = &part.seg;
        ctl.check()?;
        let mut leaf = match collector.leaf(seg) {
            Ok(l) => l,
            Err(Error::CollectionTerminated) => continue,
            Err(e) => return Err(e),
        };
        let Some(mut bs) = w.bulk_scorer(seg, mode)? else {
            leaf.finish()?;
            continue;
        };
        let live = seg.live.as_deref();
        let r = if ctl.is_noop() {
            bs.score(leaf.as_mut(), live, part.min, part.max).map(|_| ())
        } else {
            score_with_checks(bs.as_mut(), leaf.as_mut(), live, ctl, part.min, part.max)
        };
        match r {
            Ok(()) | Err(Error::CollectionTerminated) => {}
            Err(e) => {
                let _ = leaf.finish();
                return Err(e);
            }
        }
        if let Some(e) = take_deferred_error() {
            return Err(e);
        }
        leaf.finish()?;
    }
    Ok(())
}

impl Searcher {
    /// The number of live matches when every segment can tell it without iterating
    /// (`Weight::count`: term queries without deletions, match-all, exists on indexed fields…),
    /// as Elasticsearch shortcuts total hits.
    pub fn count_if_cheap(&self, q: &QueryRef) -> Result<Option<u64>> {
        let w = self.create_weight(q, ScoreMode::CompleteNoScores)?;
        let mut total = 0u64;
        for seg in &self.inner.segments {
            match w.count(seg)? {
                Some(c) => total += c as u64,
                None => return Ok(None),
            }
        }
        Ok(Some(total))
    }

    /// Number of live matches. Segments that know their count without iterating
    /// ([`Weight::count`]) answer directly; the others are iterated, split into slices run
    /// concurrently as [`search_concurrent`](Self::search_concurrent) does when
    /// [`SearchLimits::parallelism`] allows.
    pub fn count(&self, q: &QueryRef) -> Result<u64> {
        self.count_with(q, &SearchControl::default())
    }

    /// [`count`](Self::count) with a deadline and cancellation.
    pub fn count_with(&self, q: &QueryRef, ctl: &SearchControl) -> Result<u64> {
        let w = self.create_weight(q, ScoreMode::CompleteNoScores)?;
        let mut total = 0u64;
        let mut iterate = vec![false; self.inner.segments.len()];
        for seg in &self.inner.segments {
            match w.count(seg)? {
                Some(c) => total += c as u64,
                None => iterate[seg.ord as usize] = true,
            }
        }
        if !iterate.contains(&true) {
            return Ok(total);
        }
        let slices: Vec<Vec<Partition>> = self
            .slices()
            .into_iter()
            .map(|s| s.into_iter().filter(|p| iterate[p.seg.ord as usize]).collect::<Vec<_>>())
            .filter(|s| !s.is_empty())
            .collect();
        if slices.len() <= 1 || pool::in_worker() {
            let mut c = crate::collector::TotalHitCountCollector::default();
            for s in &slices {
                search_partitions(s, w.as_ref(), &mut c, ctl)?;
            }
            return Ok(total + c.total);
        }
        let collectors = (0..slices.len()).map(|_| crate::collector::TotalHitCountCollector::default()).collect();
        let cs = run_slices(Arc::from(w), slices, collectors, ctl)?;
        Ok(total + cs.iter().map(|c| c.total).sum::<u64>())
    }

    /// Explains global doc `doc`.
    pub fn explain(&self, q: &QueryRef, doc: u32) -> Result<Explanation> {
        let (seg, local) = self
            .segment_of(doc)
            .ok_or_else(|| Error::illegal(format!("doc {doc} out of range")))?;
        let w = self.create_weight(q, ScoreMode::Complete)?;
        take_deferred_error();
        let e = w.explain(seg, local)?;
        if let Some(err) = take_deferred_error() {
            return Err(err);
        }
        Ok(e)
    }

    /// The stored fields of global doc `doc`.
    pub fn stored(&self, doc: u32) -> Result<Vec<(u32, search_codec::stored::OwnedStoredValue)>> {
        let (seg, local) = self
            .segment_of(doc)
            .ok_or_else(|| Error::illegal(format!("doc {doc} out of range")))?;
        seg.ensure_stored()?;
        Ok(seg.reader.stored().document(local)?)
    }
}

/// Scores in growing windows, checking the deadline between windows (Lucene's
/// `TimeLimitingBulkScorer`).
fn score_with_checks(
    bs: &mut dyn BulkScorer,
    leaf: &mut dyn LeafCollector,
    live: Option<&LiveDocs>,
    ctl: &SearchControl,
    mut min: u32,
    end: u32,
) -> Result<()> {
    let mut window = 1u32 << 12;
    while min < end {
        ctl.check()?;
        let max = min.saturating_add(window).min(end);
        min = bs.score(leaf, live, min, max)?;
        window = window.saturating_mul(2).min(1 << 20);
    }
    Ok(())
}

/// Wraps a query for `Arc` construction.
pub fn query<Q: Query>(q: Q) -> QueryRef {
    Arc::new(q)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Approximation: every doc below `n`; confirmed: multiples of 3.
    struct Thirds {
        doc: u32,
        n: u32,
    }

    impl Scorable for Thirds {
        fn score(&mut self) -> f32 {
            1.0
        }
    }

    impl Scorer for Thirds {
        fn doc(&self) -> u32 {
            self.doc
        }
        fn next(&mut self) -> u32 {
            loop {
                let d = self.approx_next();
                if d == NO_MORE_DOCS || self.matches() {
                    return d;
                }
            }
        }
        fn advance(&mut self, t: u32) -> u32 {
            let d = self.approx_advance(t);
            if d == NO_MORE_DOCS || self.matches() { d } else { self.next() }
        }
        fn cost(&self) -> u64 {
            self.n as u64
        }
        fn has_two_phase(&self) -> bool {
            true
        }
        fn approx_next(&mut self) -> u32 {
            let t = if self.doc == u32::MAX { 0 } else { self.doc + 1 };
            self.approx_advance(t)
        }
        fn approx_advance(&mut self, t: u32) -> u32 {
            self.doc = if t >= self.n { NO_MORE_DOCS } else { t.max(if self.doc == u32::MAX { 0 } else { self.doc }) };
            self.doc
        }
        fn matches(&mut self) -> bool {
            self.doc.is_multiple_of(3)
        }
    }

    struct Docs(Vec<u32>);

    impl LeafCollector for Docs {
        fn collect(&mut self, doc: u32, _s: &mut dyn Scorable) -> Result<()> {
            self.0.push(doc);
            Ok(())
        }
    }

    #[test]
    fn bounded_windows_confirm_only_their_candidates() {
        let want: Vec<u32> = (0..50).filter(|d| d % 3 == 0).collect();
        // Bounded windows, then an unbounded rest (ReqExcl once its exclusions run out).
        for windows in [vec![10, 11, 25, NO_MORE_DOCS], vec![1, 2, NO_MORE_DOCS], vec![NO_MORE_DOCS], vec![7, 49, 50, NO_MORE_DOCS]] {
            let mut bs = DefaultBulkScorer::new(Box::new(Thirds { doc: u32::MAX, n: 50 }));
            let mut c = Docs(Vec::new());
            let mut min = 0;
            for &max in &windows {
                let next = bs.score(&mut c, None, min, max).unwrap();
                assert!(next >= max.min(50) || next == NO_MORE_DOCS, "{windows:?}: {next} < {max}");
                min = max;
            }
            assert_eq!(c.0, want, "{windows:?}");
        }
        // A partition confirms its candidates only: 20..30 and nothing past 30.
        let mut bs = DefaultBulkScorer::new(Box::new(Thirds { doc: u32::MAX, n: 50 }));
        let mut c = Docs(Vec::new());
        assert_eq!(bs.score(&mut c, None, 20, 31).unwrap(), 31);
        assert_eq!(c.0, vec![21, 24, 27, 30]);
        assert_eq!(bs.scorer.doc(), 31, "left unconfirmed on the partition end");
    }
}
