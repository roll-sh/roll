//! Top hits by score and hit counting (Lucene's `TopScoreDocCollector` and
//! `TotalHitCountCollector`).
//!
//! The queue holds the best `n` (score desc, doc asc). Ties never replace a queued hit: docs
//! arrive in increasing global order, so an equal score can't win. Once more than
//! `total_hits_threshold` hits were counted and the queue is full, the collector announces
//! `nextUp(worst queued score)` as the minimum competitive score, letting term, WAND and
//! MaxScore scorers skip whole blocks; the total then becomes a lower bound (`gte`), which is
//! Elasticsearch's `track_total_hits` (default 10 000).

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::error::Result;
use crate::search::{Collector, LeafCollector, Scorable, ScoreMode, SegmentContext};

/// Whether a total is exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TotalRelation {
    /// `eq`
    EqualTo,
    /// `gte`
    GreaterThanOrEqualTo,
}

/// A hit: global doc id and score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoreDoc {
    pub doc: u32,
    pub score: f32,
}

/// Top hits plus the total.
#[derive(Clone, Debug, PartialEq)]
pub struct TopDocs {
    pub total_hits: u64,
    pub relation: TotalRelation,
    pub hits: Vec<ScoreDoc>,
}

impl TopDocs {
    pub fn max_score(&self) -> Option<f32> {
        self.hits.iter().map(|h| h.score).reduce(f32::max)
    }
}

/// A queued hit ordered so the heap's top is the worst hit.
#[derive(Clone, Copy, Debug)]
struct Entry {
    score: f32,
    doc: u32,
}

impl PartialEq for Entry {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Entry {
    // "Greater" = worse: lower score, then higher doc.
    fn cmp(&self, o: &Self) -> Ordering {
        o.score.total_cmp(&self.score).then(self.doc.cmp(&o.doc))
    }
}

/// `Math.nextUp` for floats.
pub fn next_up(v: f32) -> f32 {
    if v.is_nan() || v == f32::INFINITY {
        return v;
    }
    if v == 0.0 {
        return f32::from_bits(1);
    }
    let b = v.to_bits();
    f32::from_bits(if v > 0.0 { b + 1 } else { b - 1 })
}

/// State shared by the collectors of one concurrent search: the hit count measured against
/// the threshold (Lucene's `HitsThresholdChecker`) and the best queue bar any collector
/// reached (`MaxScoreAccumulator`), so every slice prunes as soon as the whole search may.
#[derive(Debug, Default)]
pub struct SharedTopScore {
    hits: AtomicU64,
    /// `score bits << 32 | global doc` of the best bar (0: none). Scores are non-negative
    /// under top-scores, so the bit pattern orders like the score.
    bar: AtomicU64,
}

impl SharedTopScore {
    pub fn new() -> Arc<SharedTopScore> {
        Arc::new(SharedTopScore::default())
    }
}

/// Local hits added to the shared count at once (keeps the counter off the hot path).
const SHARED_FLUSH: u64 = 64;
/// Hits between reads of the shared bar (Lucene's `modInterval`).
const SHARED_READ_INTERVAL: u64 = 1024;

/// Collects the top `n` hits by score.
pub struct TopScoreCollector {
    n: usize,
    heap: BinaryHeap<Entry>,
    total: u64,
    threshold: u64,
    relation: TotalRelation,
    /// `search_after`: only hits strictly after `(score, doc)` in (score desc, doc asc).
    after: Option<ScoreDoc>,
    min_score: Option<f32>,
    shared: Option<Arc<SharedTopScore>>,
    /// Part of `total` already added to the shared count.
    flushed: u64,
}

impl TopScoreCollector {
    /// `threshold`: hits counted exactly (`u64::MAX`: all).
    pub fn new(n: usize, threshold: u64) -> TopScoreCollector {
        TopScoreCollector {
            n,
            heap: BinaryHeap::with_capacity(n.min(1 << 16) + 1),
            total: 0,
            threshold,
            relation: TotalRelation::EqualTo,
            after: None,
            min_score: None,
            shared: None,
            flushed: 0,
        }
    }

    /// Shares the hit count and score bar with the other collectors of a concurrent search.
    pub fn with_shared(mut self, shared: Arc<SharedTopScore>) -> Self {
        self.shared = Some(shared);
        self
    }

    /// Hits counted by the whole search (this collector's exactly, the others' as flushed).
    fn global_total(&self) -> u64 {
        match &self.shared {
            Some(sh) => sh.hits.load(AtomicOrdering::Relaxed).saturating_add(self.total - self.flushed),
            None => self.total,
        }
    }

    fn flush_hits(&mut self) {
        if let Some(sh) = &self.shared {
            sh.hits.fetch_add(self.total - self.flushed, AtomicOrdering::Relaxed);
            self.flushed = self.total;
        }
    }

    /// Only hits after this one (`search_after` on score).
    pub fn with_after(mut self, after: ScoreDoc) -> Self {
        self.after = Some(after);
        self
    }

    /// Drops hits scoring below `min` (`min_score`); they don't count either.
    pub fn with_min_score(mut self, min: f32) -> Self {
        self.min_score = Some(min);
        self
    }

    pub fn total_hits(&self) -> u64 {
        self.total
    }

    pub fn top_docs(mut self) -> TopDocs {
        self.flush_hits();
        let mut v: Vec<Entry> = self.heap.into_vec();
        v.sort();
        TopDocs {
            total_hits: self.total,
            relation: self.relation,
            hits: v.into_iter().map(|e| ScoreDoc { doc: e.doc, score: e.score }).collect(),
        }
    }
}

struct TopScoreLeaf<'a> {
    c: &'a mut TopScoreCollector,
    base: u32,
    min_announced: f32,
}

impl TopScoreLeaf<'_> {
    #[inline]
    fn update_min(&mut self, scorer: &mut dyn Scorable) {
        if self.c.heap.len() >= self.c.n && self.c.n > 0 && self.c.global_total() > self.c.threshold {
            let worst = *self.c.heap.peek().unwrap();
            let m = next_up(worst.score);
            if m > self.min_announced {
                self.min_announced = m;
                scorer.set_min_competitive_score(m);
                self.c.relation = TotalRelation::GreaterThanOrEqualTo;
            }
            if let Some(sh) = &self.c.shared
                && worst.score >= 0.0
            {
                sh.bar.fetch_max(((worst.score.to_bits() as u64) << 32) | worst.doc as u64, AtomicOrdering::Relaxed);
            }
        }
    }

    /// Adopts the best bar another slice reached: docs scoring below it can't make the
    /// merged top `n`, nor can ties after its doc (docs arrive in increasing order).
    fn read_shared(&mut self, scorer: &mut dyn Scorable) {
        let Some(sh) = &self.c.shared else { return };
        let raw = sh.bar.load(AtomicOrdering::Relaxed);
        if raw == 0 {
            return;
        }
        let (score, doc) = (f32::from_bits((raw >> 32) as u32), raw as u32);
        let m = if self.base >= doc { next_up(score) } else { score };
        if m > self.min_announced {
            self.min_announced = m;
            scorer.set_min_competitive_score(m);
            self.c.relation = TotalRelation::GreaterThanOrEqualTo;
        }
    }
}

impl LeafCollector for TopScoreLeaf<'_> {
    #[inline]
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> Result<()> {
        if self.min_announced < 0.0 {
            self.min_announced = 0.0;
            self.update_min(scorer);
            self.read_shared(scorer);
        }
        let score = scorer.score();
        if let Some(m) = self.c.min_score
            && score < m
        {
            return Ok(());
        }
        self.c.total += 1;
        if self.c.shared.is_some() {
            if self.c.total - self.c.flushed >= SHARED_FLUSH {
                self.c.flush_hits();
            }
            if self.c.total.is_multiple_of(SHARED_READ_INTERVAL) {
                self.read_shared(scorer);
            }
        }
        if self.c.n == 0 {
            if self.c.global_total() > self.c.threshold {
                self.c.relation = TotalRelation::GreaterThanOrEqualTo;
                return Err(crate::error::Error::CollectionTerminated);
            }
            return Ok(());
        }
        let gdoc = self.base + doc;
        if let Some(a) = self.c.after
            && (score > a.score || (score == a.score && gdoc <= a.doc))
        {
            return Ok(());
        }
        if self.c.heap.len() < self.c.n {
            self.c.heap.push(Entry { score, doc: gdoc });
            self.update_min(scorer);
            return Ok(());
        }
        let worst = self.c.heap.peek().unwrap();
        if score > worst.score {
            *self.c.heap.peek_mut().unwrap() = Entry { score, doc: gdoc };
            self.update_min(scorer);
        } else if self.c.total == self.c.threshold.saturating_add(1)
            || (self.c.shared.is_some() && self.min_announced == 0.0 && self.c.total.is_multiple_of(SHARED_FLUSH))
        {
            // Just past the threshold (or, shared, possibly past it globally).
            self.update_min(scorer);
        }
        Ok(())
    }
}

impl Collector for TopScoreCollector {
    fn score_mode(&self) -> ScoreMode {
        if self.threshold == u64::MAX || self.min_score.is_some() { ScoreMode::Complete } else { ScoreMode::TopScores }
    }

    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'a>> {
        let base = seg.base;
        let mut min_announced = 0.0;
        if (self.heap.len() >= self.n && self.n > 0 && self.global_total() > self.threshold)
            || self.shared.as_ref().is_some_and(|sh| sh.bar.load(AtomicOrdering::Relaxed) != 0)
        {
            // The leaf's scorer learns the bar on its first collected doc.
            min_announced = -1.0;
        }
        Ok(Box::new(TopScoreLeaf { c: self, base, min_announced }))
    }
}

/// Counts hits (using `Weight::count` shortcuts through [`crate::search::Searcher::count`]
/// is cheaper when only the count is needed).
#[derive(Default)]
pub struct TotalHitCountCollector {
    pub total: u64,
}

struct CountLeaf<'a> {
    total: &'a mut u64,
}

impl LeafCollector for CountLeaf<'_> {
    #[inline]
    fn collect(&mut self, _doc: u32, _s: &mut dyn Scorable) -> Result<()> {
        *self.total += 1;
        Ok(())
    }
}

impl Collector for TotalHitCountCollector {
    fn score_mode(&self) -> ScoreMode {
        ScoreMode::CompleteNoScores
    }
    fn leaf<'a>(&'a mut self, _seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'a>> {
        Ok(Box::new(CountLeaf { total: &mut self.total }))
    }
}
