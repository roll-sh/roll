//! `TermQuery`: the docs containing one term, scored by the field's similarity.
//!
//! The weight looks the term up once per segment ([`TermStates`]) and sums statistics across
//! segments (deleted docs included, as Lucene). The scorer walks the codec's postings cursor:
//! frequencies decode lazily, norms come from a dense byte array when every doc has one, and
//! block-max impacts give score upper bounds. Under `TopScores` the scorer itself skips blocks
//! whose best possible score is below the collector's minimum (Lucene's `ImpactsDISI`).

use std::any::Any;
use std::sync::Arc;

use search_codec::{Impact, LiveDocs, NO_MORE_DOCS, NormsReader, PostingsCursor, PostingsFlags, SegmentReader, TermState};

use crate::error::Result;
use crate::schema::Schema;
use crate::search::{
    BATCH, DocScoreBuf, Explanation, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight,
};
use crate::similarity::{CollectionStats, SimScorer, TermStats};

/// The similarity name Elasticsearch puts in `weight(…)` explanations.
pub const SIMILARITY_NAME: &str = "PerFieldSimilarity";

/// Lucene's `Term.text()` / `BytesRef.toString()`: UTF-8 when valid, else `[61 62]` hex.
pub fn term_text(t: &[u8]) -> String {
    match std::str::from_utf8(t) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let mut s = String::from("[");
            for (i, b) in t.iter().enumerate() {
                if i > 0 {
                    s.push(' ');
                }
                s.push_str(&format!("{b:x}"));
            }
            s.push(']');
            s
        }
    }
}

/// Per-segment term lookups, done once per weight (Lucene `TermStates`).
#[derive(Clone, Debug)]
pub struct TermStates {
    pub states: Vec<Option<TermState>>,
    pub doc_freq: u64,
    pub total_term_freq: u64,
}

impl TermStates {
    pub fn build(s: &Searcher, field: u32, term: &[u8]) -> TermStates {
        let h = search_codec::bloom::hash64(term);
        let mut ts = TermStates { states: Vec::with_capacity(s.segments().len()), doc_freq: 0, total_term_freq: 0 };
        for seg in s.segments() {
            let st = seg.reader.terms(field).and_then(|t| t.seek_exact_hashed(term, h));
            if let Some(st) = &st {
                ts.doc_freq += st.doc_freq as u64;
                ts.total_term_freq += st.total_term_freq;
            }
            ts.states.push(st);
        }
        ts
    }

    pub fn get(&self, seg: &SegmentContext) -> Option<&TermState> {
        self.states.get(seg.ord as usize).and_then(|s| s.as_ref())
    }

    pub fn stats(&self) -> TermStats {
        TermStats { doc_freq: self.doc_freq, total_term_freq: self.total_term_freq }
    }
}

/// Collection statistics a weight scores with: real ones when scores are needed, Lucene's
/// fake `(1, 1, 1, 1)` otherwise.
pub fn scoring_stats(s: &Searcher, field: u32, mode: ScoreMode) -> Option<CollectionStats> {
    if mode.needs_scores() {
        s.collection_stats(field)
    } else {
        Some(CollectionStats { max_doc: 1, doc_count: 1, sum_total_term_freq: 1, sum_doc_freq: 1 })
    }
}

/// Norm bytes of a field in one segment.
#[derive(Clone)]
pub enum Norms {
    None,
    /// `bytes[doc]` for every doc; the reader keeps the bytes alive.
    Dense(*const u8, usize, #[allow(dead_code)] SegmentReader),
    Sparse(NormsReader),
}

// SAFETY: the pointer refers into the segment's bytes, which the cloned reader keeps alive and
// which are never mutated.
unsafe impl Send for Norms {}
unsafe impl Sync for Norms {}

impl Norms {
    pub fn of(seg: &SegmentContext, field: u32) -> Norms {
        match seg.reader.norms(field) {
            None => Norms::None,
            Some(n) => match n.dense_bytes() {
                Some(b) => Norms::Dense(b.as_ptr(), b.len(), seg.reader.clone()),
                None => Norms::Sparse(n.clone()),
            },
        }
    }

    /// The norm of `doc` (1 when the field has none, as Lucene).
    #[inline(always)]
    pub fn get(&self, doc: u32) -> u8 {
        match self {
            Norms::None => 1,
            Norms::Dense(p, len, _) => {
                let d = doc as usize;
                if d < *len {
                    // SAFETY: bounds checked; see the Send impl.
                    unsafe { *p.add(d) }
                } else {
                    1
                }
            }
            Norms::Sparse(n) => n.get_or(doc, 1),
        }
    }

    pub fn has(&self) -> bool {
        !matches!(self, Norms::None)
    }

    /// `out[i] = get(docs[i])`.
    #[inline]
    pub fn fill(&self, docs: &[u32], out: &mut [u8]) {
        let out = &mut out[..docs.len()];
        match self {
            Norms::None => out.fill(1),
            Norms::Dense(p, len, _) => {
                for (o, &d) in out.iter_mut().zip(docs) {
                    let d = d as usize;
                    // SAFETY: bounds checked; see the Send impl.
                    *o = if d < *len { unsafe { *p.add(d) } } else { 1 };
                }
            }
            Norms::Sparse(n) => {
                for (o, &d) in out.iter_mut().zip(docs) {
                    *o = n.get_or(d, 1);
                }
            }
        }
    }
}

/// Score upper bounds by doc range: a term's postings, or several merged (exact phrases).
/// Levels nest: level `l + 1` covers at least the docs of level `l`.
pub(crate) trait ImpactSource {
    fn impact_levels(&mut self) -> usize;
    /// Last doc covered by `level` (inclusive).
    fn impact_doc_upto(&mut self, level: usize) -> u32;
    /// Competitive `(freq, norm)` pairs of `level`; empty when nothing is left to match.
    fn impacts(&mut self, level: usize) -> &[Impact];
}

impl ImpactSource for PostingsCursor {
    #[inline]
    fn impact_levels(&mut self) -> usize {
        PostingsCursor::impact_levels(self)
    }
    #[inline]
    fn impact_doc_upto(&mut self, level: usize) -> u32 {
        PostingsCursor::impact_doc_upto(self, level)
    }
    #[inline]
    fn impacts(&mut self, level: usize) -> &[Impact] {
        PostingsCursor::impacts(self, level)
    }
}

/// Caches the max score of the current impact levels (Lucene's `MaxScoreCache`).
pub(crate) struct MaxScoreCache {
    upto: [u32; 2],
    score: [f32; 2],
    global: f32,
}

impl MaxScoreCache {
    pub(crate) fn new(sim: &SimScorer) -> MaxScoreCache {
        let global = if sim.is_monotonic() { sim.score(f32::MAX, 1) } else { f32::INFINITY };
        MaxScoreCache { upto: [u32::MAX; 2], score: [0.0; 2], global }
    }

    fn level_score<I: ImpactSource + ?Sized>(&mut self, c: &mut I, sim: &SimScorer, level: usize) -> f32 {
        let up = c.impact_doc_upto(level);
        if level >= self.upto.len() {
            return Self::score_of(c.impacts(level), sim);
        }
        if self.upto[level] != up {
            self.upto[level] = up;
            self.score[level] = Self::score_of(c.impacts(level), sim);
        }
        self.score[level]
    }

    fn score_of(impacts: &[Impact], sim: &SimScorer) -> f32 {
        let mut m = 0f32;
        for imp in impacts {
            let s = sim.score(imp.freq as f32, imp.norm);
            if s > m {
                m = s;
            }
        }
        m
    }

    pub(crate) fn max_score<I: ImpactSource + ?Sized>(&mut self, c: &mut I, sim: &SimScorer, upto: u32) -> f32 {
        if !sim.is_monotonic() {
            return f32::INFINITY;
        }
        let levels = c.impact_levels();
        for level in 0..levels {
            if c.impact_doc_upto(level) >= upto {
                return self.level_score(c, sim, level);
            }
        }
        self.global
    }

    /// Lucene's `MaxScoreCache.getSkipUpTo`: the last doc of the widest level (from 0 up)
    /// whose best score is below `min`, so whole groups of blocks are skipped at once; `None`
    /// when level 0 can compete.
    pub(crate) fn skip_upto<I: ImpactSource + ?Sized>(&mut self, c: &mut I, sim: &SimScorer, min: f32) -> Option<u32> {
        if !sim.is_monotonic() {
            return None;
        }
        let levels = c.impact_levels();
        let mut skip = None;
        for level in 0..levels {
            if self.level_score(c, sim, level) >= min {
                break;
            }
            skip = Some(c.impact_doc_upto(level));
        }
        skip
    }
}

/// Scores the docs of one term.
pub struct TermScorer {
    pub(crate) cursor: PostingsCursor,
    norms: Norms,
    sim: Arc<SimScorer>,
    cache: MaxScoreCache,
    scores: bool,
    /// Impacts-based skipping (TopScores): minimum competitive score and the last doc of the
    /// block already checked against it.
    min_competitive: f32,
    skip_upto: u32,
    skipping: bool,
    /// Batch scratch (frequencies and norms of a postings block).
    batch: Option<Box<TermBatch>>,
}

struct TermBatch {
    freqs: [u32; BATCH],
    norms: [u8; BATCH],
    scores: [f32; BATCH],
}

impl TermBatch {
    fn new() -> Box<TermBatch> {
        Box::new(TermBatch { freqs: [0; BATCH], norms: [0; BATCH], scores: [0.0; BATCH] })
    }
}

impl TermScorer {
    pub fn new(cursor: PostingsCursor, norms: Norms, sim: Arc<SimScorer>, mode: ScoreMode) -> TermScorer {
        let cache = MaxScoreCache::new(&sim);
        TermScorer {
            cursor,
            norms,
            sim,
            cache,
            scores: mode.needs_scores(),
            min_competitive: 0.0,
            skip_upto: u32::MAX,
            skipping: false,
            batch: None,
        }
    }

    #[inline]
    pub fn freq_u32(&mut self) -> u32 {
        self.cursor.freq()
    }

    #[inline]
    pub fn norm(&self) -> u8 {
        self.norms.get(self.cursor.doc())
    }

    pub fn sim(&self) -> &Arc<SimScorer> {
        &self.sim
    }

    /// Lucene `ImpactsDISI.advanceTarget`: the first target in a block that may compete.
    fn competitive_target(&mut self, target: u32) -> u32 {
        if !self.skipping || (self.skip_upto != u32::MAX && target <= self.skip_upto) {
            return target;
        }
        match competitive_target(&mut self.cursor, &mut self.cache, &self.sim, self.min_competitive, target) {
            Some((t, upto)) => {
                self.skip_upto = upto;
                t
            }
            None => NO_MORE_DOCS,
        }
    }
}

/// Lucene `ImpactsDISI.advanceTarget` over any impact source: the first doc `>= target` in a
/// block whose best score reaches `min`, with that block's last doc; `None` when no block
/// left can compete. Skips by the widest non-competitive level at a time.
pub(crate) fn competitive_target<I: ImpactSource + ?Sized + AdvanceShallow>(
    c: &mut I,
    cache: &mut MaxScoreCache,
    sim: &SimScorer,
    min: f32,
    mut target: u32,
) -> Option<(u32, u32)> {
    loop {
        c.shallow(target);
        let upto = c.impact_doc_upto(0);
        match cache.skip_upto(c, sim, min) {
            None => return Some((target, upto)),
            Some(skip) => {
                if skip >= NO_MORE_DOCS {
                    return None;
                }
                target = skip + 1;
            }
        }
    }
}

/// Moves an impact source's view to the block holding a doc.
pub(crate) trait AdvanceShallow {
    fn shallow(&mut self, target: u32);
}

impl AdvanceShallow for PostingsCursor {
    #[inline]
    fn shallow(&mut self, target: u32) {
        self.advance_shallow(target);
    }
}

impl Scorable for TermScorer {
    #[inline]
    fn score(&mut self) -> f32 {
        if !self.scores {
            return 0.0;
        }
        let f = self.cursor.freq() as f32;
        let n = self.norms.get(self.cursor.doc());
        self.sim.score(f, n)
    }

    fn set_min_competitive_score(&mut self, min: f32) {
        if min > self.min_competitive {
            self.min_competitive = min;
            if self.sim.is_monotonic() {
                self.skipping = true;
                self.skip_upto = u32::MAX;
            }
        }
    }
}

impl Scorer for TermScorer {
    #[inline]
    fn doc(&self) -> u32 {
        self.cursor.doc()
    }

    #[inline]
    fn next(&mut self) -> u32 {
        if self.skipping {
            let d = self.cursor.doc();
            let t = if d == u32::MAX { 0 } else { d + 1 };
            return self.advance(t);
        }
        self.cursor.next()
    }

    #[inline]
    fn advance(&mut self, target: u32) -> u32 {
        if self.skipping {
            let t = self.competitive_target(target);
            if t == NO_MORE_DOCS {
                return self.cursor.advance(NO_MORE_DOCS);
            }
            let d = self.cursor.advance(t);
            if d != NO_MORE_DOCS && d > self.skip_upto {
                return self.advance(d);
            }
            return d;
        }
        self.cursor.advance(target)
    }

    fn cost(&self) -> u64 {
        self.cursor.cost()
    }

    fn advance_shallow(&mut self, target: u32) -> u32 {
        self.cursor.advance_shallow(target);
        self.cursor.impact_doc_upto(0)
    }

    fn max_score(&mut self, upto: u32) -> f32 {
        self.cache.max_score(&mut self.cursor, &self.sim, upto)
    }

    fn freq(&mut self) -> f32 {
        self.cursor.freq() as f32
    }

    fn apply_required(&mut self, docs: &mut [u32], scores: &mut [f64], len: usize) -> usize {
        if len > BATCH {
            return crate::search::apply_required_per_doc(self, docs, scores, len);
        }
        let mut tb = self.batch.take().unwrap_or_else(TermBatch::new);
        let mut cur = self.cursor.doc();
        let mut k = 0;
        for j in 0..len {
            let t = docs[j];
            if cur == u32::MAX || cur < t {
                cur = if self.skipping { Scorer::advance(self, t) } else { self.cursor.advance(t) };
            }
            if cur == t {
                docs[k] = t;
                scores[k] = scores[j];
                tb.freqs[k] = self.cursor.freq();
                k += 1;
            }
        }
        if self.scores && k > 0 {
            self.norms.fill(&docs[..k], &mut tb.norms);
            self.sim.score_batch(&tb.freqs, &tb.norms, &mut tb.scores[..k]);
            for (acc, &s) in scores[..k].iter_mut().zip(&tb.scores[..k]) {
                *acc += s as f64;
            }
        }
        self.batch = Some(tb);
        k
    }

    fn apply_filter(&mut self, docs: &mut [u32], scores: &mut [f64], len: usize) -> usize {
        // The cursor directly: no dyn call per doc, nothing scored.
        let mut cur = self.cursor.doc();
        let mut k = 0;
        for j in 0..len {
            let t = docs[j];
            if cur == u32::MAX || cur < t {
                cur = if self.skipping { Scorer::advance(self, t) } else { self.cursor.advance(t) };
            }
            if cur == t {
                docs[k] = t;
                scores[k] = scores[j];
                k += 1;
            }
        }
        k
    }

    fn next_docs_and_scores(&mut self, upto: u32, live: Option<&LiveDocs>, buf: &mut DocScoreBuf) {
        let mut tb = self.batch.take().unwrap_or_else(TermBatch::new);
        loop {
            // Lucene's `ImpactsDISI.ensureCompetitive`: start the batch in a competitive block.
            while self.skipping {
                let d = self.cursor.doc();
                if d >= upto || d == NO_MORE_DOCS {
                    break;
                }
                let t = self.competitive_target(d);
                if t == d {
                    break;
                }
                self.cursor.advance(t);
            }
            let mut n = self.cursor.next_postings(upto, &mut buf.docs, &mut tb.freqs);
            if n > 0
                && let Some(l) = live
            {
                let mut k = 0;
                for i in 0..n {
                    let d = buf.docs[i];
                    if l.is_live(d) {
                        buf.docs[k] = d;
                        tb.freqs[k] = tb.freqs[i];
                        k += 1;
                    }
                }
                if k == 0 {
                    continue;
                }
                n = k;
            }
            buf.len = n;
            if self.scores {
                self.norms.fill(&buf.docs[..n], &mut tb.norms);
                self.sim.score_batch(&tb.freqs, &tb.norms, &mut buf.scores[..n]);
            } else {
                buf.scores[..n].fill(0.0);
            }
            break;
        }
        self.batch = Some(tb);
    }
}

/// Collects a term's docs without dynamic dispatch on the scorer (the most common query).
struct TermBulkScorer {
    s: TermScorer,
    buf: Option<Box<DocScoreBuf>>,
}

/// The score handed to collectors; records the minimum competitive score they announce.
struct ScoreBox {
    score: f32,
    min: f32,
}

impl Scorable for ScoreBox {
    #[inline]
    fn score(&mut self) -> f32 {
        self.score
    }
    fn set_min_competitive_score(&mut self, m: f32) {
        if m > self.min {
            self.min = m;
        }
    }
}

impl crate::search::BulkScorer for TermBulkScorer {
    fn score(
        &mut self,
        collector: &mut dyn crate::search::LeafCollector,
        live: Option<&search_codec::LiveDocs>,
        min: u32,
        max: u32,
    ) -> Result<u32> {
        let s = &mut self.s;
        let mut d = s.doc();
        if d == u32::MAX || d < min {
            d = s.advance(min);
        }
        let mut b = ScoreBox { score: 0.0, min: s.min_competitive };
        if s.scores {
            // Lucene's `BatchScoreBulkScorer`: score a block at a time and hand collectors
            // only the docs that can still compete.
            let buf = self.buf.get_or_insert_with(DocScoreBuf::new);
            loop {
                s.next_docs_and_scores(max, live, buf);
                if buf.len == 0 {
                    break;
                }
                for i in 0..buf.len {
                    let score = buf.scores[i];
                    if b.min <= 0.0 || score >= b.min {
                        b.score = score;
                        collector.collect(buf.docs[i], &mut b)?;
                    }
                }
                if b.min > s.min_competitive {
                    s.set_min_competitive_score(b.min);
                }
            }
            return Ok(s.doc());
        }
        if !s.scores && collector.skips() {
            return crate::search::score_skipping(s, collector, live, d, max);
        }
        while d < max {
            if live.is_none_or(|l| l.is_live(d)) {
                b.score = s.score();
                collector.collect(d, &mut b)?;
                if b.min > s.min_competitive {
                    s.set_min_competitive_score(b.min);
                }
            }
            d = s.next();
        }
        Ok(d)
    }

    fn cost(&self) -> u64 {
        self.s.cost()
    }
}

/// A single-term query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermQuery {
    pub field: u32,
    pub term: Vec<u8>,
    /// Statistics to score with instead of the index's (Lucene's `TermQuery(term, states)`:
    /// blended multi-term rewrites give every term the same document frequency).
    pub stats: Option<TermStats>,
}

impl TermQuery {
    pub fn new(field: u32, term: impl Into<Vec<u8>>) -> TermQuery {
        TermQuery { field, term: term.into(), stats: None }
    }

    pub fn with_stats(field: u32, term: impl Into<Vec<u8>>, stats: TermStats) -> TermQuery {
        TermQuery { field, term: term.into(), stats: Some(stats) }
    }
}

pub(crate) struct TermWeight {
    pub(crate) field: u32,
    pub(crate) states: TermStates,
    pub(crate) sim: Option<Arc<SimScorer>>,
    pub(crate) mode: ScoreMode,
    desc: String,
}

impl TermWeight {
    pub(crate) fn new(
        s: &Searcher,
        field: u32,
        term: &[u8],
        mode: ScoreMode,
        boost: f32,
        desc: String,
        stats: Option<TermStats>,
    ) -> TermWeight {
        let states = TermStates::build(s, field, term);
        let sim = if states.doc_freq == 0 {
            None
        } else {
            let coll = scoring_stats(s, field, mode);
            let ts = if !mode.needs_scores() {
                TermStats { doc_freq: 1, total_term_freq: 1 }
            } else {
                stats.unwrap_or(states.stats())
            };
            coll.map(|c| Arc::new(s.schema().similarity(field).scorer(boost, &c, &[ts])))
        };
        TermWeight { field, states, sim, mode, desc }
    }

    /// Waits for the field's postings when the term is in a segment still downloading.
    fn ensure(&self, seg: &SegmentContext) -> Result<()> {
        if self.states.get(seg).is_some() {
            seg.ensure_postings(self.field)?;
        }
        Ok(())
    }

    /// The scorer of the segment; callers first [`ensure`](Self::ensure) its postings.
    pub(crate) fn term_scorer(&self, seg: &SegmentContext) -> Option<TermScorer> {
        let st = self.states.get(seg)?;
        let sim = self.sim.as_ref()?;
        let terms = seg.reader.terms(self.field)?;
        let flags = if self.mode.needs_scores() { PostingsFlags::FREQS } else { PostingsFlags::DOCS };
        let cursor = terms.postings(st, flags);
        let norms = if self.mode.needs_scores() { Norms::of(seg, self.field) } else { Norms::None };
        Some(TermScorer::new(cursor, norms, sim.clone(), self.mode))
    }
}

impl Weight for TermWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        self.ensure(seg)?;
        Ok(self.term_scorer(seg).map(|s| Box::new(s) as Box<dyn Scorer>))
    }

    fn bulk_scorer(&self, seg: &SegmentContext, _mode: ScoreMode) -> Result<Option<Box<dyn crate::search::BulkScorer>>> {
        self.ensure(seg)?;
        Ok(self.term_scorer(seg).map(|s| Box::new(TermBulkScorer { s, buf: None }) as Box<dyn crate::search::BulkScorer>))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        self.ensure(seg)?;
        if let Some(mut s) = self.term_scorer(seg)
            && s.advance(doc) == doc
        {
            let freq = s.cursor.freq() as f32;
            let norm = Norms::of(seg, self.field).get(doc);
            let fe = Explanation::leaf(freq, "freq, occurrences of term within document");
            let se = self.sim.as_ref().unwrap().explain(fe, norm);
            return Ok(Explanation::matched(
                se.value(),
                format!("weight({} in {doc}) [{SIMILARITY_NAME}], result of:", self.desc),
                vec![se],
            ));
        }
        Ok(Explanation::no_match("no matching term", vec![]))
    }

    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        if seg.has_deletions() {
            return Ok(None);
        }
        Ok(Some(self.states.get(seg).map_or(0, |s| s.doc_freq)))
    }
}

impl Query for TermQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let desc = self.lucene_string(s.schema());
        Ok(Box::new(TermWeight::new(s, self.field, &self.term, mode, boost, desc, self.stats)))
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        format!("{}:{}", schema.name(self.field), term_text(&self.term))
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        v.term(self.field, &self.term);
    }

    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_tag(h, "term");
        h.write_u32(self.field);
        crate::search::hash_bytes(h, &self.term);
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `Arc` helper.
pub fn term(field: u32, t: impl Into<Vec<u8>>) -> QueryRef {
    Arc::new(TermQuery::new(field, t))
}
