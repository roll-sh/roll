//! The `boosting` query (Lucene's `FunctionScoreQuery.boostByQuery`, as Elasticsearch builds
//! it): docs of `positive`, scored by it, with the score multiplied by `negative_boost` where
//! `negative` also matches.
//!
//! The negative side is only probed at the positive side's docs (advance, then its two-phase
//! check once per doc). Scores are computed in double — `score · boost` — and narrowed, as
//! Lucene's double values sources do; explanations follow Lucene's text.

use std::any::Any;
use std::sync::Arc;

use search_codec::NO_MORE_DOCS;

use crate::error::{Error, Result};
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::util::java::float_to_string;

/// `positive`, demoted by `negative_boost` where `negative` matches.
#[derive(Clone, Debug)]
pub struct BoostingQuery {
    pub positive: QueryRef,
    pub negative: QueryRef,
    pub negative_boost: f32,
}

impl BoostingQuery {
    /// Elasticsearch's checks: a non-negative boost.
    pub fn new(positive: QueryRef, negative: QueryRef, negative_boost: f32) -> Result<BoostingQuery> {
        if negative_boost < 0.0 || negative_boost.is_nan() {
            return Err(Error::illegal("[boosting] query requires negative_boost to be set to positive value"));
        }
        Ok(BoostingQuery { positive, negative, negative_boost })
    }

    fn source_string(&self, schema: &Schema) -> String {
        format!("boost(queryboost(score({}))^{})", self.negative.lucene_string(schema), float_to_string(self.negative_boost))
    }
}

struct BoostingWeight {
    positive: Box<dyn Weight>,
    negative: Box<dyn Weight>,
    negative_boost: f32,
    boost: f32,
    desc: String,
    source: String,
}

struct BoostingScorer {
    positive: Box<dyn Scorer>,
    negative: Option<Box<dyn Scorer>>,
    /// Negative match of the doc last probed (Lucene caches `matches()` per doc).
    probed: (u32, bool),
    negative_boost: f32,
    boost: f32,
}

impl BoostingScorer {
    fn negative_matches(&mut self, doc: u32) -> bool {
        if self.probed.0 == doc {
            return self.probed.1;
        }
        let Some(n) = self.negative.as_mut() else { return false };
        let m = if n.has_two_phase() {
            let d = if n.doc() != u32::MAX && n.doc() >= doc { n.doc() } else { n.approx_advance(doc) };
            d == doc && n.matches()
        } else {
            let d = if n.doc() != u32::MAX && n.doc() >= doc { n.doc() } else { n.advance(doc) };
            d == doc
        };
        self.probed = (doc, m);
        m
    }
}

impl Scorable for BoostingScorer {
    fn score(&mut self) -> f32 {
        let doc = self.positive.doc();
        let s = self.positive.score() as f64;
        let factor = if self.negative_matches(doc) { s * self.negative_boost as f64 } else { s };
        if factor >= 0.0 { (factor * self.boost as f64) as f32 } else { 0.0 }
    }
}

impl Scorer for BoostingScorer {
    fn doc(&self) -> u32 {
        self.positive.doc()
    }
    fn next(&mut self) -> u32 {
        self.positive.next()
    }
    fn advance(&mut self, t: u32) -> u32 {
        self.positive.advance(t)
    }
    fn cost(&self) -> u64 {
        self.positive.cost()
    }
    fn has_two_phase(&self) -> bool {
        self.positive.has_two_phase()
    }
    fn approx_next(&mut self) -> u32 {
        self.positive.approx_next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        self.positive.approx_advance(t)
    }
    fn matches(&mut self) -> bool {
        self.positive.matches()
    }
    fn match_cost(&self) -> f32 {
        self.positive.match_cost()
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        f32::INFINITY
    }
}

impl Weight for BoostingWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(positive) = self.positive.scorer(seg)? else { return Ok(None) };
        let negative = self.negative.scorer(seg)?;
        Ok(Some(Box::new(BoostingScorer { positive, negative, probed: (NO_MORE_DOCS, false), negative_boost: self.negative_boost, boost: self.boost })))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let inner = self.positive.explain(seg, doc)?;
        if !inner.is_match() {
            return Ok(inner);
        }
        let neg = self.negative.explain(seg, doc)?;
        let (value, expl) = if neg.is_match() {
            let v = inner.value() as f64 * self.negative_boost as f64;
            let boost_expl = Explanation::leaf(self.negative_boost, format!("Matched boosting query score({})", self.source));
            (v, Explanation::matched(v, "product of:", vec![inner, boost_expl]))
        } else {
            (inner.value() as f64, inner)
        };
        let q = format!("FunctionScoreQuery({}, scored by {})", self.desc, boost_source(&self.source, self.negative_boost));
        Ok(if self.boost != 1.0 {
            Explanation::matched((value * self.boost as f64) as f32, format!("weight({q}), product of:"), vec![Explanation::leaf(self.boost, "boost"), expl])
        } else {
            Explanation::matched(expl.value, format!("weight({q}), result of:"), vec![expl])
        })
    }

    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        self.positive.count(seg)
    }
}

fn boost_source(negative: &str, boost: f32) -> String {
    format!("boost(queryboost(score({negative}))^{})", float_to_string(boost))
}

impl Query for BoostingQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let inner_mode = if mode.needs_scores() { ScoreMode::Complete } else { ScoreMode::CompleteNoScores };
        let positive = s.weight_of(&s.rewrite(&self.positive)?, inner_mode, 1.0)?;
        if !mode.needs_scores() {
            return Ok(positive);
        }
        let negative = s.weight_of(&s.rewrite(&self.negative)?, ScoreMode::Complete, 1.0)?;
        Ok(Box::new(BoostingWeight {
            positive,
            negative,
            negative_boost: self.negative_boost,
            boost,
            desc: self.positive.lucene_string(s.schema()),
            source: self.negative.lucene_string(s.schema()),
        }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        let p = s.rewrite(&self.positive)?;
        if Arc::ptr_eq(&p, &self.positive) {
            return Ok(None);
        }
        Ok(Some(Arc::new(BoostingQuery { positive: p, ..self.clone() })))
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        format!("FunctionScoreQuery({}, scored by {})", self.positive.lucene_string(schema), self.source_string(schema))
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.positive.visit(v);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
