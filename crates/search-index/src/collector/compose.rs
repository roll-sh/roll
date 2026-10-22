//! Collector composition: several collectors over one pass ([`MultiCollector`]),
//! `terminate_after` ([`TerminateAfter`]), `min_score` for any collector ([`MinScore`]), and
//! [`CollectorManager`]s for concurrent segment search with
//! their merges ([`TopScoreManager`], [`TopFieldManager`], [`CountManager`]).

use std::cmp::Ordering;

use crate::collector::top_docs::{ScoreDoc, TopDocs, TopScoreCollector, TotalHitCountCollector, TotalRelation};
use crate::collector::top_field::{FieldDoc, SortKey, SortValue, TopFieldCollector, TopFieldDocs, compare_field_docs};
use crate::error::{Error, Result};
use crate::search::{Collector, CollectorManager, LeafCollector, Scorable, ScoreMode, Searcher, SegmentContext};

/// The score mode serving several collectors at once.
pub fn combine_modes(modes: impl IntoIterator<Item = ScoreMode>) -> ScoreMode {
    let mut scores = false;
    let mut exhaustive = false;
    let mut all_top_scores = true;
    let mut any = false;
    for m in modes {
        any = true;
        scores |= m.needs_scores();
        exhaustive |= m.is_exhaustive();
        all_top_scores &= m == ScoreMode::TopScores;
    }
    match (any, scores, exhaustive) {
        (false, ..) => ScoreMode::CompleteNoScores,
        (_, true, _) if all_top_scores => ScoreMode::TopScores,
        (_, true, true) => ScoreMode::Complete,
        (_, true, false) => ScoreMode::TopDocsWithScores,
        (_, false, true) => ScoreMode::CompleteNoScores,
        (_, false, false) => ScoreMode::TopDocs,
    }
}

/// Runs several collectors in one pass (Lucene's `MultiCollector`). A collector that
/// terminates (on a segment or for good) stops receiving docs while the others go on; scores
/// are computed once per doc; the minimum competitive score passed to the scorer is the
/// lowest any scoring collector announced (none until all of them did).
pub struct MultiCollector<'c> {
    collectors: Vec<&'c mut dyn Collector>,
}

impl<'c> MultiCollector<'c> {
    pub fn new(collectors: Vec<&'c mut dyn Collector>) -> MultiCollector<'c> {
        MultiCollector { collectors }
    }
}

struct MultiLeaf<'a> {
    leaves: Vec<Option<Box<dyn LeafCollector + 'a>>>,
    /// Per leaf: the minimum competitive score it announced (`None`: none yet / no scores).
    mins: Vec<Option<f32>>,
    scoring: Vec<bool>,
    applied: f32,
    /// Minimums only reach the scorer when every collector allows skipping.
    forward: bool,
}

/// Caches the score of the current doc and merges the collectors' minimums.
struct SharedScorable<'s> {
    inner: &'s mut dyn Scorable,
    score: Option<f32>,
    announced: Option<f32>,
}

impl Scorable for SharedScorable<'_> {
    fn score(&mut self) -> f32 {
        match self.score {
            Some(s) => s,
            None => {
                let s = self.inner.score();
                self.score = Some(s);
                s
            }
        }
    }
    fn set_min_competitive_score(&mut self, min: f32) {
        self.announced = Some(min);
    }
}

impl LeafCollector for MultiLeaf<'_> {
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> Result<()> {
        let mut live = 0;
        let mut cached: Option<f32> = None;
        let mut changed = false;
        for i in 0..self.leaves.len() {
            let Some(l) = self.leaves[i].as_mut() else { continue };
            let mut s = SharedScorable { inner: &mut *scorer, score: cached, announced: None };
            let r = l.collect(doc, &mut s);
            cached = s.score;
            if let Some(m) = s.announced {
                self.mins[i] = Some(m);
                changed = true;
            }
            match r {
                Ok(()) => live += 1,
                Err(Error::CollectionTerminated) => {
                    if let Some(mut l) = self.leaves[i].take() {
                        l.finish()?;
                    }
                    // A finished collector no longer limits the others.
                    self.scoring[i] = false;
                    changed = true;
                }
                Err(e) => return Err(e),
            }
        }
        if live == 0 {
            return Err(Error::CollectionTerminated);
        }
        if changed && self.forward {
            let mut min = f32::INFINITY;
            let mut all = true;
            for (i, sc) in self.scoring.iter().enumerate() {
                if !*sc {
                    continue;
                }
                match self.mins[i] {
                    Some(m) => min = min.min(m),
                    None => all = false,
                }
            }
            if all && min.is_finite() && min > self.applied {
                self.applied = min;
                scorer.set_min_competitive_score(min);
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        for l in self.leaves.iter_mut().flatten() {
            l.finish()?;
        }
        Ok(())
    }

    // A doc can be skipped only when every collector still running would skip it.
    fn skips(&self) -> bool {
        self.leaves.iter().flatten().all(|l| l.skips())
    }

    fn competitive_from(&mut self, doc: u32) -> u32 {
        self.leaves.iter_mut().flatten().map(|l| l.competitive_from(doc)).min().unwrap_or(doc)
    }
}

impl Collector for MultiCollector<'_> {
    fn score_mode(&self) -> ScoreMode {
        combine_modes(self.collectors.iter().map(|c| c.score_mode()))
    }

    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'a>> {
        let forward = self.score_mode() == ScoreMode::TopScores;
        let mut leaves = Vec::with_capacity(self.collectors.len());
        let mut scoring = Vec::with_capacity(self.collectors.len());
        let mut any = false;
        for c in self.collectors.iter_mut() {
            let mode = c.score_mode();
            match c.leaf(seg) {
                Ok(l) => {
                    any = true;
                    leaves.push(Some(l));
                    scoring.push(mode.needs_scores());
                }
                Err(Error::CollectionTerminated) => {
                    leaves.push(None);
                    scoring.push(false);
                }
                Err(e) => return Err(e),
            }
        }
        if !any {
            return Err(Error::CollectionTerminated);
        }
        let n = leaves.len();
        Ok(Box::new(MultiLeaf { leaves, mins: vec![None; n], scoring, applied: 0.0, forward }))
    }
}

/// Elasticsearch's `terminate_after`: stops after `max` collected docs (per shard).
pub struct TerminateAfter<C> {
    pub inner: C,
    max: u64,
    count: u64,
    /// Whether collection stopped while docs may remain (`terminated_early`).
    pub terminated_early: bool,
}

impl<C: Collector> TerminateAfter<C> {
    pub fn new(inner: C, max: u64) -> TerminateAfter<C> {
        TerminateAfter { inner, max, count: 0, terminated_early: false }
    }

    pub fn count(&self) -> u64 {
        self.count
    }
}

struct TerminateLeaf<'a> {
    inner: Box<dyn LeafCollector + 'a>,
    max: u64,
    count: &'a mut u64,
    terminated: &'a mut bool,
}

impl LeafCollector for TerminateLeaf<'_> {
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> Result<()> {
        if *self.count >= self.max {
            *self.terminated = true;
            return Err(Error::CollectionTerminated);
        }
        *self.count += 1;
        self.inner.collect(doc, scorer)
    }
    fn finish(&mut self) -> Result<()> {
        self.inner.finish()
    }
    // No skipping: `terminate_after` counts every match in order.
}

impl<C: Collector> Collector for TerminateAfter<C> {
    fn score_mode(&self) -> ScoreMode {
        // Every doc up to `max` must be seen: no skipping by score.
        match self.inner.score_mode() {
            ScoreMode::TopScores => ScoreMode::Complete,
            ScoreMode::TopDocs => ScoreMode::CompleteNoScores,
            ScoreMode::TopDocsWithScores => ScoreMode::Complete,
            m => m,
        }
    }

    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'a>> {
        if self.count >= self.max {
            self.terminated_early = true;
            return Err(Error::CollectionTerminated);
        }
        let inner = self.inner.leaf(seg)?;
        Ok(Box::new(TerminateLeaf { inner, max: self.max, count: &mut self.count, terminated: &mut self.terminated_early }))
    }
}

/// `min_score` for any collector: docs scoring below `min` are not collected.
pub struct MinScore<C> {
    pub inner: C,
    min: f32,
}

impl<C: Collector> MinScore<C> {
    pub fn new(inner: C, min: f32) -> MinScore<C> {
        MinScore { inner, min }
    }
}

struct MinScoreLeaf<'a> {
    inner: Box<dyn LeafCollector + 'a>,
    min: f32,
}

struct Fixed<'s> {
    score: f32,
    inner: &'s mut dyn Scorable,
}

impl Scorable for Fixed<'_> {
    fn score(&mut self) -> f32 {
        self.score
    }
    fn set_min_competitive_score(&mut self, m: f32) {
        self.inner.set_min_competitive_score(m);
    }
}

impl LeafCollector for MinScoreLeaf<'_> {
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> Result<()> {
        let score = scorer.score();
        if score < self.min {
            return Ok(());
        }
        self.inner.collect(doc, &mut Fixed { score, inner: scorer })
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

impl<C: Collector> Collector for MinScore<C> {
    fn score_mode(&self) -> ScoreMode {
        match self.inner.score_mode() {
            ScoreMode::CompleteNoScores => ScoreMode::Complete,
            ScoreMode::TopDocs => ScoreMode::TopDocsWithScores,
            m => m,
        }
    }
    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'a>> {
        Ok(Box::new(MinScoreLeaf { inner: self.inner.leaf(seg)?, min: self.min }))
    }
}

/// `post_filter`: the inner collector sees only the docs matching `filter` (collectors run
/// beside it, such as aggregations, still see every match). Scores and minimum competitive
/// scores pass through.
pub struct PostFilter<C> {
    pub inner: C,
    filter: Box<dyn crate::search::Weight>,
}

impl<C: Collector> PostFilter<C> {
    pub fn new(searcher: &crate::search::Searcher, filter: &crate::search::QueryRef, inner: C) -> Result<PostFilter<C>> {
        let filter = searcher.create_weight(filter, ScoreMode::CompleteNoScores)?;
        Ok(PostFilter { inner, filter })
    }
}

struct PostFilterLeaf<'a> {
    inner: Box<dyn LeafCollector + 'a>,
    filter: Option<Box<dyn crate::search::Scorer>>,
}

impl PostFilterLeaf<'_> {
    /// Whether the filter matches `doc` (docs arrive in increasing order).
    fn accepts(&mut self, doc: u32) -> bool {
        let Some(f) = self.filter.as_mut() else { return false };
        let cur = f.doc();
        if cur != u32::MAX && cur > doc {
            return false;
        }
        if f.has_two_phase() {
            if cur == doc {
                // Positioned here by an earlier advance that overshot: not confirmed yet.
                return f.matches();
            }
            f.approx_advance(doc) == doc && f.matches()
        } else {
            (if cur == doc { cur } else { f.advance(doc) }) == doc
        }
    }
}

impl LeafCollector for PostFilterLeaf<'_> {
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> Result<()> {
        if self.accepts(doc) { self.inner.collect(doc, scorer) } else { Ok(()) }
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

impl<C: Collector> Collector for PostFilter<C> {
    fn score_mode(&self) -> ScoreMode {
        self.inner.score_mode()
    }
    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'a>> {
        let filter = self.filter.scorer(seg)?;
        Ok(Box::new(PostFilterLeaf { inner: self.inner.leaf(seg)?, filter }))
    }
}

/// Merges per-slice top hits by score (score desc, doc asc).
pub fn merge_top_docs(parts: Vec<TopDocs>, n: usize) -> TopDocs {
    let mut total = 0u64;
    let mut relation = TotalRelation::EqualTo;
    let mut hits: Vec<ScoreDoc> = Vec::new();
    for p in parts {
        total = total.saturating_add(p.total_hits);
        if p.relation == TotalRelation::GreaterThanOrEqualTo {
            relation = TotalRelation::GreaterThanOrEqualTo;
        }
        hits.extend(p.hits);
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
    hits.truncate(n);
    TopDocs { total_hits: total, relation, hits }
}

/// Merges per-slice sorted hits.
pub fn merge_top_field_docs(keys: &[SortKey], parts: Vec<TopFieldDocs>, n: usize) -> TopFieldDocs {
    let mut total = 0u64;
    let mut relation = TotalRelation::EqualTo;
    let mut max_score: Option<f32> = None;
    let mut hits: Vec<FieldDoc> = Vec::new();
    for p in parts {
        total = total.saturating_add(p.total_hits);
        if p.relation == TotalRelation::GreaterThanOrEqualTo {
            relation = TotalRelation::GreaterThanOrEqualTo;
        }
        max_score = match (max_score, p.max_score) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        hits.extend(p.hits);
    }
    hits.sort_by(|a, b| compare_field_docs(keys, a, b));
    hits.truncate(n);
    TopFieldDocs { total_hits: total, relation, hits, max_score }
}

/// Concurrent top hits by score.
pub struct TopScoreManager {
    pub n: usize,
    pub threshold: u64,
    pub after: Option<ScoreDoc>,
    pub min_score: Option<f32>,
}

impl CollectorManager for TopScoreManager {
    type C = TopScoreCollector;
    type Out = TopDocs;
    fn score_mode(&self) -> ScoreMode {
        if self.threshold == u64::MAX || self.min_score.is_some() { ScoreMode::Complete } else { ScoreMode::TopScores }
    }
    fn new_collector(&self) -> Result<TopScoreCollector> {
        let mut c = TopScoreCollector::new(self.n, self.threshold);
        if let Some(a) = self.after {
            c = c.with_after(a);
        }
        if let Some(m) = self.min_score {
            c = c.with_min_score(m);
        }
        Ok(c)
    }
    /// The slices share the hit count against the threshold and the best score bar
    /// (Lucene's `HitsThresholdChecker` and `MaxScoreAccumulator`).
    fn new_collectors(&self, k: usize) -> Result<Vec<TopScoreCollector>> {
        let shared = crate::collector::top_docs::SharedTopScore::new();
        (0..k).map(|_| Ok(self.new_collector()?.with_shared(shared.clone()))).collect()
    }
    fn reduce(&self, cs: Vec<TopScoreCollector>) -> Result<TopDocs> {
        Ok(merge_top_docs(cs.into_iter().map(|c| c.top_docs()).collect(), self.n))
    }
}

/// Concurrent sorted top hits.
pub struct TopFieldManager {
    pub keys: Vec<SortKey>,
    pub n: usize,
    pub threshold: u64,
    pub track_scores: bool,
    pub after: Option<(Vec<SortValue>, Option<u32>)>,
    pub searcher: Searcher,
}

impl CollectorManager for TopFieldManager {
    type C = TopFieldCollector;
    type Out = TopFieldDocs;
    fn score_mode(&self) -> ScoreMode {
        TopFieldCollector::new(self.keys.clone(), self.n, self.threshold, self.track_scores).score_mode()
    }
    fn new_collector(&self) -> Result<TopFieldCollector> {
        let mut c = TopFieldCollector::new(self.keys.clone(), self.n, self.threshold, self.track_scores).with_searcher(&self.searcher);
        if let Some((v, d)) = &self.after {
            c = c.with_after(v.clone(), *d);
        }
        Ok(c)
    }
    /// The slices count hits against the threshold together.
    fn new_collectors(&self, k: usize) -> Result<Vec<TopFieldCollector>> {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        (0..k).map(|_| Ok(self.new_collector()?.with_shared_hits(hits.clone()))).collect()
    }
    fn reduce(&self, cs: Vec<TopFieldCollector>) -> Result<TopFieldDocs> {
        Ok(merge_top_field_docs(&self.keys, cs.into_iter().map(|c| c.top_docs()).collect(), self.n))
    }
}

/// Concurrent hit counting.
pub struct CountManager;

impl CollectorManager for CountManager {
    type C = TotalHitCountCollector;
    type Out = u64;
    fn score_mode(&self) -> ScoreMode {
        ScoreMode::CompleteNoScores
    }
    fn new_collector(&self) -> Result<TotalHitCountCollector> {
        Ok(TotalHitCountCollector::default())
    }
    fn reduce(&self, cs: Vec<TotalHitCountCollector>) -> Result<u64> {
        Ok(cs.iter().map(|c| c.total).sum())
    }
}

/// Orders two hits by score desc, doc asc (helper for callers merging shards).
pub fn cmp_score_docs(a: &ScoreDoc, b: &ScoreDoc) -> Ordering {
    b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc))
}
