//! Structural queries: match-all, match-none, constant score, boost, field exists, and the
//! scorers they share (all docs, doc id sets, constant-score wrappers).

use std::any::Any;
use std::sync::Arc;

use search_codec::{NO_MORE_DOCS, PostingsFlags};

use crate::error::Result;
use crate::schema::Schema;
use crate::search::{
    BulkScorer, Explanation, LeafCollector, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, ScoreValue, Scorer,
    ScorerSupplier, Searcher, SegmentContext, Weight,
};
use crate::util::bitset::DocIdSet;
use crate::util::java::float_to_string;

/// `query.toString() + (score == 1 ? "" : "^" + score)` (Lucene `ConstantScoreWeight.explain`).
pub fn const_desc(desc: &str, score: f32) -> String {
    if score == 1.0 { desc.to_string() } else { format!("{desc}^{}", float_to_string(score)) }
}

/// Every doc `0..max_doc` with a constant score.
pub struct AllScorer {
    doc: u32,
    max_doc: u32,
    score: f32,
}

impl AllScorer {
    pub fn new(max_doc: u32, score: f32) -> AllScorer {
        AllScorer { doc: u32::MAX, max_doc, score }
    }
}

impl Scorable for AllScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl Scorer for AllScorer {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc + 1 };
        self.advance(t)
    }
    fn advance(&mut self, target: u32) -> u32 {
        self.doc = if target < self.max_doc { target } else { NO_MORE_DOCS };
        self.doc
    }
    fn cost(&self) -> u64 {
        self.max_doc as u64
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        self.score
    }
}

/// A materialized doc set with a constant score.
pub struct SetScorer {
    set: Arc<DocIdSet>,
    doc: u32,
    idx: usize,
    score: f32,
    cost: u64,
}

impl SetScorer {
    pub fn new(set: Arc<DocIdSet>, score: f32) -> SetScorer {
        let cost = set.cost();
        SetScorer { set, doc: u32::MAX, idx: 0, score, cost }
    }
}

impl Scorable for SetScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl Scorer for SetScorer {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc.saturating_add(1) };
        self.advance(t)
    }
    fn advance(&mut self, target: u32) -> u32 {
        self.doc = match &*self.set {
            DocIdSet::Empty => NO_MORE_DOCS,
            DocIdSet::All(n) => {
                if target < *n {
                    target
                } else {
                    NO_MORE_DOCS
                }
            }
            DocIdSet::Sparse(v) => {
                let rest = &v[self.idx..];
                let k = if rest.len() > 16 && rest[15] < target {
                    rest.partition_point(|&d| d < target)
                } else {
                    rest.iter().position(|&d| d >= target).unwrap_or(rest.len())
                };
                self.idx += k;
                v.get(self.idx).copied().unwrap_or(NO_MORE_DOCS)
            }
            DocIdSet::Dense(b) => b.next_set_bit(target),
        };
        self.doc
    }
    fn cost(&self) -> u64 {
        self.cost
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        self.score
    }
}

/// Wraps a scorer, replacing its score by a constant.
pub struct ConstScorer {
    pub inner: Box<dyn Scorer>,
    pub score: f32,
}

impl Scorable for ConstScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl Scorer for ConstScorer {
    fn doc(&self) -> u32 {
        self.inner.doc()
    }
    fn next(&mut self) -> u32 {
        self.inner.next()
    }
    fn advance(&mut self, t: u32) -> u32 {
        self.inner.advance(t)
    }
    fn cost(&self) -> u64 {
        self.inner.cost()
    }
    fn has_two_phase(&self) -> bool {
        self.inner.has_two_phase()
    }
    fn approx_next(&mut self) -> u32 {
        self.inner.approx_next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        self.inner.approx_advance(t)
    }
    fn matches(&mut self) -> bool {
        self.inner.matches()
    }
    fn match_cost(&self) -> f32 {
        self.inner.match_cost()
    }
    fn advance_shallow(&mut self, _t: u32) -> u32 {
        NO_MORE_DOCS
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        self.score
    }
    fn apply_required(&mut self, docs: &mut [u32], scores: &mut [f64], len: usize) -> usize {
        let k = self.inner.apply_filter(docs, scores, len);
        if self.score != 0.0 {
            for s in &mut scores[..k] {
                *s += self.score as f64;
            }
        }
        k
    }
    fn apply_filter(&mut self, docs: &mut [u32], scores: &mut [f64], len: usize) -> usize {
        self.inner.apply_filter(docs, scores, len)
    }
}

/// Explains a constant-score weight by checking whether `doc` matches.
pub fn explain_const(w: &dyn Weight, seg: &SegmentContext, doc: u32, desc: &str, score: f32) -> Result<Explanation> {
    if matches_doc(w, seg, doc)? {
        Ok(Explanation::leaf(score, const_desc(desc, score)))
    } else {
        Ok(Explanation::no_match(format!("{desc} doesn't match id {doc}"), vec![]))
    }
}

/// Whether the weight's scorer matches `doc`.
pub fn matches_doc(w: &dyn Weight, seg: &SegmentContext, doc: u32) -> Result<bool> {
    let Some(mut s) = w.scorer(seg)? else { return Ok(false) };
    if s.has_two_phase() {
        let d = s.approx_advance(doc);
        Ok(d == doc && s.matches())
    } else {
        Ok(s.advance(doc) == doc)
    }
}

/// `*:*`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MatchAllQuery;

struct MatchAllWeight {
    score: f32,
}

impl Weight for MatchAllWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(Some(Box::new(AllScorer::new(seg.max_doc(), self.score))))
    }
    fn bulk_scorer(&self, seg: &SegmentContext, _mode: ScoreMode) -> Result<Option<Box<dyn BulkScorer>>> {
        Ok(Some(Box::new(AllBulk { max_doc: seg.max_doc(), score: self.score })))
    }
    fn explain(&self, _seg: &SegmentContext, _doc: u32) -> Result<Explanation> {
        Ok(Explanation::leaf(self.score, const_desc("*:*", self.score)))
    }
    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        Ok(Some(seg.num_docs))
    }
}

/// Collects docs `min..max` directly, skipping deleted ones word by word.
struct AllBulk {
    max_doc: u32,
    score: f32,
}

impl BulkScorer for AllBulk {
    fn score(&mut self, c: &mut dyn LeafCollector, live: Option<&search_codec::LiveDocs>, min: u32, max: u32) -> Result<u32> {
        let end = max.min(self.max_doc);
        let mut sv = ScoreValue(self.score);
        let mut d = min;
        let skips = c.skips();
        while d < end {
            if skips {
                d = c.competitive_from(d);
                if d >= end {
                    break;
                }
            }
            if let Some(l) = live {
                d = l.next_live(d);
                if d >= end {
                    break;
                }
            }
            c.collect(d, &mut sv)?;
            d += 1;
        }
        Ok(if end >= self.max_doc { NO_MORE_DOCS } else { end })
    }
    fn cost(&self) -> u64 {
        self.max_doc as u64
    }
}

impl Query for MatchAllQuery {
    fn weight(&self, _s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(MatchAllWeight { score: boost }))
    }
    fn lucene_string(&self, _schema: &Schema) -> String {
        "*:*".into()
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_tag(h, "match_all");
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Matches nothing (`MatchNoDocsQuery`), with the reason Elasticsearch gives.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct MatchNoneQuery {
    pub reason: String,
}

impl MatchNoneQuery {
    pub fn new(reason: impl Into<String>) -> MatchNoneQuery {
        MatchNoneQuery { reason: reason.into() }
    }
}

struct MatchNoneWeight {
    reason: String,
}

impl Weight for MatchNoneWeight {
    fn scorer(&self, _seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(None)
    }
    fn explain(&self, _seg: &SegmentContext, _doc: u32) -> Result<Explanation> {
        Ok(Explanation::no_match(self.reason.clone(), vec![]))
    }
    fn count(&self, _seg: &SegmentContext) -> Result<Option<u32>> {
        Ok(Some(0))
    }
}

impl Query for MatchNoneQuery {
    fn weight(&self, _s: &Searcher, _mode: ScoreMode, _boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(MatchNoneWeight { reason: self.reason.clone() }))
    }
    fn lucene_string(&self, _schema: &Schema) -> String {
        format!("MatchNoDocsQuery(\"{}\")", self.reason)
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_tag(h, "match_none");
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Every match of the inner query scores the boost (`constant_score`, filters).
#[derive(Clone, Debug)]
pub struct ConstantScoreQuery {
    pub query: QueryRef,
}

impl ConstantScoreQuery {
    pub fn new(query: QueryRef) -> ConstantScoreQuery {
        ConstantScoreQuery { query }
    }
}

struct ConstWeight {
    inner: Box<dyn Weight>,
    score: f32,
    desc: String,
}

struct ConstSupplier {
    inner: Box<dyn ScorerSupplier>,
    score: f32,
}

impl ScorerSupplier for ConstSupplier {
    fn cost(&self) -> u64 {
        self.inner.cost()
    }
    fn get(self: Box<Self>, lead: u64) -> Result<Box<dyn Scorer>> {
        Ok(Box::new(ConstScorer { inner: self.inner.get(lead)?, score: self.score }))
    }
}

impl Weight for ConstWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.inner.scorer(seg)?.map(|s| Box::new(ConstScorer { inner: s, score: self.score }) as Box<dyn Scorer>))
    }
    fn scorer_supplier(&self, seg: &SegmentContext) -> Result<Option<Box<dyn ScorerSupplier>>> {
        Ok(self
            .inner
            .scorer_supplier(seg)?
            .map(|s| Box::new(ConstSupplier { inner: s, score: self.score }) as Box<dyn ScorerSupplier>))
    }
    fn bulk_scorer(&self, seg: &SegmentContext, _mode: ScoreMode) -> Result<Option<Box<dyn BulkScorer>>> {
        // The inner bulk scorer (e.g. a disjunction's) runs without scores; ours replaces them.
        let Some(inner) = self.inner.bulk_scorer(seg, ScoreMode::CompleteNoScores)? else { return Ok(None) };
        Ok(Some(Box::new(ConstBulk { inner, score: self.score })))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        self.inner.count(seg)
    }
}

struct ConstBulk {
    inner: Box<dyn BulkScorer>,
    score: f32,
}

struct ConstLeaf<'a> {
    inner: &'a mut dyn LeafCollector,
    score: f32,
}

impl LeafCollector for ConstLeaf<'_> {
    fn collect(&mut self, doc: u32, _s: &mut dyn Scorable) -> Result<()> {
        self.inner.collect(doc, &mut ScoreValue(self.score))
    }
}

impl BulkScorer for ConstBulk {
    fn score(&mut self, c: &mut dyn LeafCollector, live: Option<&search_codec::LiveDocs>, min: u32, max: u32) -> Result<u32> {
        let mut l = ConstLeaf { inner: c, score: self.score };
        self.inner.score(&mut l, live, min, max)
    }
    fn cost(&self) -> u64 {
        self.inner.cost()
    }
}

impl Query for ConstantScoreQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let inner_mode = if mode.is_exhaustive() || !mode.needs_scores() { ScoreMode::CompleteNoScores } else { ScoreMode::TopDocs };
        let inner = s.filter_weight(&s.rewrite(&self.query)?, inner_mode, 1.0)?;
        Ok(Box::new(ConstWeight { inner, score: boost, desc: self.lucene_string(s.schema()) }))
    }
    /// Lucene's: the inner query loses what only matters for scores — a boost, a nested
    /// constant score, the scoring of boolean clauses (`rewriteNoScoring`: musts become
    /// filters, optional shoulds go) — and match-none bubbles up.
    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        let mut r = s.rewrite(&self.query)?;
        if let Some(b) = r.as_any().downcast_ref::<BoostQuery>() {
            r = b.query.clone();
        } else if let Some(c) = r.as_any().downcast_ref::<ConstantScoreQuery>() {
            r = c.query.clone();
        } else if let Some(b) = r.as_any().downcast_ref::<crate::query::boolean::BooleanQuery>()
            && let Some(nb) = b.rewrite_no_scoring()
        {
            r = Arc::new(nb);
        }
        if r.as_any().is::<MatchNoneQuery>() {
            return Ok(Some(r));
        }
        if !Arc::ptr_eq(&r, &self.query) {
            return Ok(Some(Arc::new(ConstantScoreQuery { query: r })));
        }
        Ok(None)
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        format!("ConstantScore({})", self.query.lucene_string(schema))
    }
    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.query.visit(v)
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_tag(h, "constant_score");
        self.query.cache_identity(h)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Multiplies the inner query's scores.
#[derive(Clone, Debug)]
pub struct BoostQuery {
    pub query: QueryRef,
    pub boost: f32,
}

impl BoostQuery {
    pub fn new(query: QueryRef, boost: f32) -> BoostQuery {
        BoostQuery { query, boost }
    }
}

impl Query for BoostQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        s.rewrite(&self.query)?.weight(s, mode, boost * self.boost)
    }
    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        if self.boost == 1.0 {
            return Ok(Some(self.query.clone()));
        }
        if let Some(b) = self.query.as_any().downcast_ref::<BoostQuery>() {
            return Ok(Some(Arc::new(BoostQuery { query: b.query.clone(), boost: b.boost * self.boost })));
        }
        if self.boost == 0.0 && !self.query.as_any().is::<ConstantScoreQuery>() {
            // Lucene: a 0 boost needs no scores from the inner query.
            return Ok(Some(Arc::new(BoostQuery {
                query: Arc::new(ConstantScoreQuery::new(self.query.clone())),
                boost: 0.0,
            })));
        }
        let r = s.rewrite(&self.query)?;
        if !Arc::ptr_eq(&r, &self.query) {
            return Ok(Some(Arc::new(BoostQuery { query: r, boost: self.boost })));
        }
        Ok(None)
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        format!("({})^{}", self.query.lucene_string(schema), float_to_string(self.boost))
    }
    fn visit(&self, v: &mut dyn QueryVisitor) {
        v.begin_boost(self.boost);
        self.query.visit(v);
        v.end_boost();
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        // A boost changes scores, not the doc set.
        self.query.cache_identity(h)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Docs that have a value in a field: norms, doc values, vectors (Lucene's
/// `FieldExistsQuery`), with terms or points as a fallback for fields with none of those.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FieldExistsQuery {
    pub field: u32,
}

struct ExistsWeight {
    field: u32,
    score: f32,
    desc: String,
}

impl ExistsWeight {
    /// Waits for the postings when the doc set comes from them (fields with neither norms,
    /// doc values, vectors nor points) in a segment still downloading.
    fn ensure(&self, seg: &SegmentContext) -> Result<()> {
        let (r, f) = (&seg.reader, self.field);
        if r.norms(f).is_none() && r.doc_values(f).is_none() && r.vectors(f).is_none() && r.points(f).is_none() {
            seg.ensure_postings(f)?;
        }
        Ok(())
    }

    fn docs(&self, seg: &SegmentContext) -> Option<Arc<DocIdSet>> {
        let r = &seg.reader;
        let f = self.field;
        let set = |ds: &search_codec::DocSet| {
            if ds.is_all() {
                return DocIdSet::All(ds.max_doc());
            }
            let mut b = crate::util::bitset::DocIdSetBuilder::new(r.max_doc());
            let mut c = ds.cursor();
            loop {
                let d = c.next();
                if d == NO_MORE_DOCS {
                    break;
                }
                b.add(d);
            }
            b.build()
        };
        let built = if let Some(n) = r.norms(f) {
            Some(set(n.docs()))
        } else if let Some(dv) = r.doc_values(f) {
            Some(set(match dv {
                search_codec::DocValuesReader::Numeric(v) => v.docs(),
                search_codec::DocValuesReader::Binary(v) => v.docs(),
                search_codec::DocValuesReader::Sorted(v) => v.docs(),
                search_codec::DocValuesReader::SortedSet(v) => v.docs(),
                search_codec::DocValuesReader::SortedNumeric(v) => v.docs(),
            }))
        } else if let Some(v) = r.vectors(f) {
            Some(set(v.docs()))
        } else if let Some(p) = r.points(f) {
            let mut b = crate::util::bitset::DocIdSetBuilder::new(r.max_doc());
            p.visit_all_values(&mut |d, _| b.add(d));
            Some(b.build())
        } else if let Some(t) = r.terms(f) {
            let mut b = crate::util::bitset::DocIdSetBuilder::new(r.max_doc());
            let mut it = t.iter();
            let mut pc = search_codec::PostingsCursor::empty();
            while it.next().is_some() {
                it.postings_reset(PostingsFlags::DOCS, &mut pc);
                loop {
                    let d = pc.next();
                    if d == NO_MORE_DOCS {
                        break;
                    }
                    b.add(d);
                }
            }
            Some(b.build())
        } else {
            None
        };
        built.map(Arc::new)
    }
}

impl Weight for ExistsWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let f = self.field;
        self.ensure(seg)?;
        let set = seg.core.cached("exists", f as u64, || self.docs(seg));
        Ok(match &*set {
            Some(s) => Some(Box::new(SetScorer::new(s.clone(), self.score))),
            None => None,
        })
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        if seg.has_deletions() {
            return Ok(None);
        }
        self.ensure(seg)?;
        let set = seg.core.cached("exists", self.field as u64, || self.docs(seg));
        Ok(Some(set.as_ref().as_ref().map_or(0, |s| s.cost() as u32)))
    }
}

impl Query for FieldExistsQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(ExistsWeight { field: self.field, score: boost, desc: self.lucene_string(s.schema()) }))
    }
    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        // Lucene: every doc of every segment has the field -> match all.
        let mut all = true;
        for seg in s.segments() {
            let r = &seg.reader;
            if r.field_info(self.field).is_none() {
                all = false;
                break;
            }
            let full = if r.norms(self.field).is_some() {
                r.terms(self.field).is_some_and(|t| t.stats().doc_count == r.max_doc())
            } else if let Some(v) = r.vectors(self.field) {
                v.len() == r.max_doc()
            } else if let Some(dv) = r.doc_values(self.field) {
                // Deleted docs never reach collectors, so they don't matter (Lucene 10.5).
                let skipper = match dv {
                    search_codec::DocValuesReader::Numeric(v) => v.skipper(),
                    search_codec::DocValuesReader::SortedNumeric(v) => v.skipper(),
                    search_codec::DocValuesReader::Sorted(v) => v.skipper(),
                    search_codec::DocValuesReader::SortedSet(v) => v.skipper(),
                    search_codec::DocValuesReader::Binary(_) => None,
                }
                // The codec keeps skip data for every field; Lucene only has a skipper for
                // fields declared with one.
                .filter(|_| r.field_info(self.field).is_some_and(|f| f.doc_values_skip_index));
                r.points(self.field).is_some_and(|p| p.doc_count() == r.max_doc())
                    || r.terms(self.field).is_some_and(|t| t.stats().doc_count == r.max_doc())
                    || skipper.is_some_and(|k| k.global_doc_count() == r.max_doc())
            } else {
                false
            };
            if !full {
                all = false;
                break;
            }
        }
        if all {
            return Ok(Some(Arc::new(MatchAllQuery)));
        }
        Ok(None)
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        format!("FieldExistsQuery [field={}]", schema.name(self.field))
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_tag(h, "exists");
        h.write_u32(self.field);
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
