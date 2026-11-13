//! `CappedScoreQuery` (Elasticsearch's search-business-rules module): the wrapped query's
//! docs with their scores capped at a maximum. The `pinned` query caps organic results below
//! the pinned docs' scores with it.
//!
//! Capping keeps top-k pruning valid — a capped score reaches a minimum competitive score `m`
//! exactly when the raw score does (for `m` up to the cap; above it nothing competes) — so the
//! minimum passes straight through. When the wrapped scorer's maximum is already within the
//! cap, its scorer is used as is. Bulk scoring wraps the collector instead of the scorer, so
//! the wrapped query keeps its own bulk scorer.

use std::any::Any;
use std::sync::Arc;

use search_codec::{LiveDocs, NO_MORE_DOCS};

use crate::error::{Error, Result};
use crate::query::basic::BoostQuery;
use crate::schema::Schema;
use crate::search::{
    BulkScorer, Explanation, LeafCollector, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight,
};
use crate::util::java::float_to_string;

/// Caps the scores of `query` at `max_score`.
#[derive(Clone, Debug)]
pub struct CappedScoreQuery {
    pub query: QueryRef,
    pub max_score: f32,
}

impl CappedScoreQuery {
    pub fn new(query: QueryRef, max_score: f32) -> Result<CappedScoreQuery> {
        if max_score.is_nan() || max_score <= 0.0 {
            return Err(Error::illegal(format!(
                "org.elasticsearch.xpack.searchbusinessrules.CappedScoreQuery maxScore must be >0, {} supplied.",
                float_to_string(max_score)
            )));
        }
        Ok(CappedScoreQuery { query, max_score })
    }
}

struct CappedWeight {
    inner: Box<dyn Weight>,
    max: f32,
    top_scores: bool,
    inner_desc: String,
}

struct CappedScorer {
    inner: Box<dyn Scorer>,
    max: f32,
}

impl Scorable for CappedScorer {
    fn score(&mut self) -> f32 {
        self.inner.score().min(self.max)
    }
    fn set_min_competitive_score(&mut self, m: f32) {
        self.inner.set_min_competitive_score(m);
    }
}

impl Scorer for CappedScorer {
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
    fn advance_shallow(&mut self, t: u32) -> u32 {
        self.inner.advance_shallow(t)
    }
    fn max_score(&mut self, upto: u32) -> f32 {
        self.inner.max_score(upto).min(self.max)
    }
    fn freq(&mut self) -> f32 {
        self.inner.freq()
    }
}

/// Hands collectors capped scores.
struct CapScorable<'a> {
    inner: &'a mut dyn Scorable,
    max: f32,
}

impl Scorable for CapScorable<'_> {
    fn score(&mut self) -> f32 {
        self.inner.score().min(self.max)
    }
    fn set_min_competitive_score(&mut self, m: f32) {
        self.inner.set_min_competitive_score(m);
    }
}

struct CapCollector<'a> {
    inner: &'a mut dyn LeafCollector,
    max: f32,
}

impl LeafCollector for CapCollector<'_> {
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> Result<()> {
        self.inner.collect(doc, &mut CapScorable { inner: scorer, max: self.max })
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

struct CappedBulk {
    inner: Box<dyn BulkScorer>,
    max: f32,
}

impl BulkScorer for CappedBulk {
    fn score(&mut self, collector: &mut dyn LeafCollector, live: Option<&LiveDocs>, min: u32, max: u32) -> Result<u32> {
        self.inner.score(&mut CapCollector { inner: collector, max: self.max }, live, min, max)
    }
    fn cost(&self) -> u64 {
        self.inner.cost()
    }
}

impl Weight for CappedWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(mut inner) = self.inner.scorer(seg)? else { return Ok(None) };
        if self.top_scores {
            // Already within the cap: nothing to cap.
            inner.advance_shallow(0);
            if inner.max_score(NO_MORE_DOCS) <= self.max {
                return Ok(Some(inner));
            }
        }
        Ok(Some(Box::new(CappedScorer { inner, max: self.max })))
    }

    fn bulk_scorer(&self, seg: &SegmentContext, mode: ScoreMode) -> Result<Option<Box<dyn BulkScorer>>> {
        Ok(self.inner.bulk_scorer(seg, mode)?.map(|inner| Box::new(CappedBulk { inner, max: self.max }) as Box<dyn BulkScorer>))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let sub = self.inner.explain(seg, doc)?;
        if sub.is_match() && sub.value() > self.max {
            return Ok(Explanation::matched(
                self.max,
                format!("Capped score of {}, max of", self.inner_desc),
                vec![sub, Explanation::leaf(self.max, "maximum score")],
            ));
        }
        Ok(sub)
    }

    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        self.inner.count(seg)
    }
}

impl Query for CappedScoreQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let inner = s.weight_of(&s.rewrite(&self.query)?, mode, boost)?;
        if !mode.needs_scores() {
            return Ok(inner);
        }
        Ok(Box::new(CappedWeight {
            inner,
            max: self.max_score,
            top_scores: mode == ScoreMode::TopScores,
            inner_desc: self.query.lucene_string(s.schema()),
        }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        if let Some(r) = self.query.rewrite(s)? {
            return Ok(Some(Arc::new(CappedScoreQuery { query: r, max_score: self.max_score })));
        }
        if self.query.as_any().is::<CappedScoreQuery>() {
            return Ok(Some(self.query.clone()));
        }
        if let Some(b) = self.query.as_any().downcast_ref::<BoostQuery>() {
            // The cap replaces the boost (Elasticsearch drops it).
            return Ok(Some(Arc::new(CappedScoreQuery { query: b.query.clone(), max_score: self.max_score })));
        }
        Ok(None)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        format!("CappedScore({})", self.query.lucene_string(schema))
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.query.visit(v);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
