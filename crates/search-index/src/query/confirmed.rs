//! `BinaryDvConfirmedQuery` (Elasticsearch's `wildcard` field type): an approximation query —
//! typically n-gram terms of the pattern — whose candidates are confirmed against the doc's
//! binary doc values, which hold the field's original values.
//!
//! The caller owns the value encoding and the test (an automaton for wildcard / regexp /
//! fuzzy / range, or a term set); the engine supplies the doc's raw binary value. Matches
//! score a constant (the boost); the check runs in the two-phase confirmation step with
//! Elasticsearch's match cost, after cheaper conjunction clauses.

use std::any::Any;
use std::sync::Arc;

use search_codec::{BinaryDocValues, DocValuesReader, NO_MORE_DOCS};

use crate::error::Result;
use crate::query::basic::{MatchNoneQuery, explain_const};
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};

/// The confirmation of one doc from its raw binary doc value.
pub type BinaryMatcher = dyn Fn(&[u8]) -> bool + Send + Sync;

/// Docs of `approx` whose binary doc value in `field` passes `matcher`.
#[derive(Clone)]
pub struct BinaryDvConfirmedQuery {
    pub approx: QueryRef,
    pub field: u32,
    pub matcher: Arc<BinaryMatcher>,
    /// The query's string (Elasticsearch's `toString`, e.g. `:*foo*` for a wildcard).
    pub description: String,
}

impl std::fmt::Debug for BinaryDvConfirmedQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BinaryDvConfirmedQuery({:?}, {}, {})", self.approx, self.field, self.description)
    }
}

struct ConfirmedWeight {
    approx: Box<dyn Weight>,
    field: u32,
    matcher: Arc<BinaryMatcher>,
    score: f32,
    desc: String,
}

struct ConfirmedScorer {
    approx: Box<dyn Scorer>,
    values: BinaryDocValues,
    matcher: Arc<BinaryMatcher>,
    score: f32,
}

impl Scorable for ConfirmedScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl Scorer for ConfirmedScorer {
    fn doc(&self) -> u32 {
        self.approx.doc()
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
        if d == NO_MORE_DOCS || self.matches() {
            return d;
        }
        self.next()
    }
    fn cost(&self) -> u64 {
        self.approx.cost()
    }
    fn has_two_phase(&self) -> bool {
        true
    }
    // The approximation's own matches (its two-phase check, if any, is cheaper than ours).
    fn approx_next(&mut self) -> u32 {
        self.approx.next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        self.approx.advance(t)
    }
    fn matches(&mut self) -> bool {
        // Docs without a value can come from match-all approximations.
        self.values.get(self.approx.doc()).is_some_and(|v| (self.matcher)(v))
    }
    fn match_cost(&self) -> f32 {
        1000.0
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        self.score
    }
}

impl Weight for ConfirmedWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(DocValuesReader::Binary(values)) = seg.reader.doc_values(self.field) else { return Ok(None) };
        let Some(approx) = self.approx.scorer(seg)? else { return Ok(None) };
        Ok(Some(Box::new(ConfirmedScorer { approx, values: values.clone(), matcher: self.matcher.clone(), score: self.score })))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for BinaryDvConfirmedQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let approx = s.weight_of(&s.rewrite(&self.approx)?, mode, boost)?;
        let score = if mode.needs_scores() { boost } else { 0.0 };
        Ok(Box::new(ConfirmedWeight { approx, field: self.field, matcher: self.matcher.clone(), score, desc: self.description.clone() }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        let r = s.rewrite(&self.approx)?;
        if r.as_any().is::<MatchNoneQuery>() {
            return Ok(Some(Arc::new(MatchNoneQuery::new(""))));
        }
        if Arc::ptr_eq(&r, &self.approx) {
            return Ok(None);
        }
        Ok(Some(Arc::new(BinaryDvConfirmedQuery { approx: r, ..self.clone() })))
    }

    fn lucene_string(&self, _schema: &Schema) -> String {
        self.description.clone()
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.approx.visit(v);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
