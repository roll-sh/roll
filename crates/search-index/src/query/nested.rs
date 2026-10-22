//! Block joins over nested documents (Lucene's `ToParentBlockJoinQuery` /
//! `ToChildBlockJoinQuery`, Elasticsearch's `nested` query).
//!
//! Nested children are indexed right before their parent, so a parent's children are the docs
//! between the previous parent and itself. Parents are identified by a filter (a bit set per
//! segment, built once and cached on the segment like Lucene's `BitSetProducer`): for
//! top-level nested fields, the root docs (`_primary_term` exists); for deeper levels, the
//! parent path's docs.

use std::any::Any;
use std::sync::Arc;

use search_codec::NO_MORE_DOCS;

use crate::error::Result;
use crate::query::basic::FieldExistsQuery;
use crate::schema::{PRIMARY_TERM_FIELD, Schema};
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::util::bitset::FixedBitSet;

/// The bit set of a filter query's matches in one segment (deleted docs included: block
/// structure ignores deletions), cached on the segment by the query's Lucene string.
pub fn segment_bits(s: &Searcher, seg: &SegmentContext, filter: &QueryRef) -> Result<Arc<FixedBitSet>> {
    let key_str = filter.lucene_string(s.schema());
    let key = crate::buffer::termhash::hash_bytes(key_str.as_bytes());
    let mut err = None;
    let bits = seg.core.cached("bitset_filter", key, || {
        let mut b = FixedBitSet::new(seg.max_doc());
        // Bit sets of filters are not part of the profiled query tree (Lucene's BitSetProducer).
        match s.rewrite(filter).and_then(|f| f.weight(s, ScoreMode::CompleteNoScores, 1.0)).and_then(|w| w.scorer(seg)) {
            Ok(Some(mut sc)) => loop {
                let d = sc.next();
                if d == NO_MORE_DOCS {
                    break;
                }
                b.set(d);
            },
            Ok(None) => {}
            Err(e) => err = Some(e),
        }
        b
    });
    match err {
        Some(e) => Err(e),
        None => Ok(bits),
    }
}

/// The filter of root (non-nested) docs.
pub fn root_filter() -> QueryRef {
    Arc::new(FieldExistsQuery { field: PRIMARY_TERM_FIELD })
}

/// How child scores combine into the parent's (`score_mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinScoreMode {
    None,
    Avg,
    Max,
    /// Elasticsearch's `sum`.
    Total,
    Min,
}

impl JoinScoreMode {
    fn name(self) -> &'static str {
        match self {
            JoinScoreMode::None => "None",
            JoinScoreMode::Avg => "Avg",
            JoinScoreMode::Max => "Max",
            JoinScoreMode::Total => "Total",
            JoinScoreMode::Min => "Min",
        }
    }
}

/// Parents of children matching `child` (`nested` query).
#[derive(Clone, Debug)]
pub struct ToParentQuery {
    pub child: QueryRef,
    pub parent_filter: QueryRef,
    pub score_mode: JoinScoreMode,
}

struct ToParentWeight {
    child: Box<dyn Weight>,
    parent_filter: QueryRef,
    searcher: Searcher,
    mode: JoinScoreMode,
    needs_scores: bool,
}

/// Iterates parents whose block contains a matching child.
struct ToParentScorer {
    child: Box<dyn Scorer>,
    parents: Arc<FixedBitSet>,
    doc: u32,
    score: f32,
    freq: u32,
    mode: JoinScoreMode,
    needs_scores: bool,
    error: Option<String>,
}

impl ToParentScorer {
    /// Moves to the parent of the child scorer's current doc and aggregates its block.
    fn settle(&mut self) -> u32 {
        let c = self.child.doc();
        if c == NO_MORE_DOCS {
            self.doc = NO_MORE_DOCS;
            return NO_MORE_DOCS;
        }
        let p = self.parents.next_set_bit(c);
        if p == NO_MORE_DOCS {
            self.doc = NO_MORE_DOCS;
            return NO_MORE_DOCS;
        }
        if p == c {
            self.error = Some(format!(
                "Child query must not match same docs with parent filter. Combine them as must clauses (+) to find a problem doc. docId={c}"
            ));
            self.doc = NO_MORE_DOCS;
            return NO_MORE_DOCS;
        }
        self.doc = p;
        let first = if self.mode == JoinScoreMode::None || !self.needs_scores { 0.0 } else { self.child.score() as f64 };
        let mut score = first;
        let mut freq = 1u32;
        loop {
            let d = self.child.next();
            if d >= p {
                if d == p {
                    self.error = Some(format!(
                        "Child query must not match same docs with parent filter. Combine them as must clauses (+) to find a problem doc. docId={p}"
                    ));
                }
                break;
            }
            freq += 1;
            if self.mode != JoinScoreMode::None && self.needs_scores {
                let s = self.child.score();
                match self.mode {
                    JoinScoreMode::Total | JoinScoreMode::Avg => score += s as f64,
                    JoinScoreMode::Min => score = score.min(s as f64),
                    JoinScoreMode::Max => score = score.max(s as f64),
                    JoinScoreMode::None => {}
                }
            }
        }
        if self.mode == JoinScoreMode::Avg {
            score /= freq as f64;
        }
        self.freq = freq;
        self.score = score as f32;
        p
    }
}

impl Scorable for ToParentScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl Scorer for ToParentScorer {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn next(&mut self) -> u32 {
        if self.doc == u32::MAX {
            self.child.next();
        }
        self.settle()
    }
    fn advance(&mut self, t: u32) -> u32 {
        if self.doc != u32::MAX && t <= self.doc {
            return self.doc;
        }
        // Children of parent `t` start after the previous parent.
        let start = if t == 0 { 0 } else { self.parents.prev_set_bit(t - 1).map_or(0, |p| p + 1) };
        let c = self.child.doc();
        if c == u32::MAX || c < start {
            self.child.advance(start);
        }
        self.settle()
    }
    fn cost(&self) -> u64 {
        self.child.cost()
    }
    fn freq(&mut self) -> f32 {
        self.freq as f32
    }
}

impl Weight for ToParentWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(child) = self.child.scorer(seg)? else { return Ok(None) };
        let parents = segment_bits(&self.searcher, seg, &self.parent_filter)?;
        Ok(Some(Box::new(ToParentScorer {
            child,
            parents,
            doc: u32::MAX,
            score: 0.0,
            freq: 0,
            mode: self.mode,
            needs_scores: self.needs_scores,
            error: None,
        })))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let Some(mut s) = self.scorer(seg)? else { return Ok(Explanation::no_match("Not a match", vec![])) };
        if s.advance(doc) != doc {
            return Ok(Explanation::no_match("Not a match", vec![]));
        }
        let parents = segment_bits(&self.searcher, seg, &self.parent_filter)?;
        let prev = if doc == 0 { None } else { parents.prev_set_bit(doc - 1) };
        let start = seg.base as i64 + prev.map_or(-1, |p| p as i64) + 1;
        let end = seg.base as i64 + doc as i64 - 1;
        let mut best: Option<Explanation> = None;
        let mut matches = 0;
        let lo = prev.map_or(0, |p| p + 1);
        for c in lo..doc {
            let e = self.child.explain(seg, c)?;
            if e.is_match() {
                matches += 1;
                // The child shown is the one deciding the score: the lowest for `min`.
                let better = |b: &Explanation| {
                    if self.mode == JoinScoreMode::Min { e.value.as_f64() < b.value.as_f64() } else { e.value.as_f64() > b.value.as_f64() }
                };
                if best.as_ref().is_none_or(better) {
                    best = Some(e);
                }
            }
        }
        Ok(Explanation::matched(
            s.score(),
            format!("Score based on {matches} child docs in range from {start} to {end}, using score mode {}", self.mode.name()),
            best.into_iter().collect(),
        ))
    }
}

impl Query for ToParentQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let child = if self.score_mode == JoinScoreMode::None || !mode.needs_scores() {
            // As Lucene: children only match, through a zero-boost constant-score wrapper.
            let cs: QueryRef = Arc::new(crate::query::basic::ConstantScoreQuery::new(self.child.clone()));
            s.weight_of(&s.rewrite(&cs)?, mode, 0.0)?
        } else {
            s.weight_of(&s.rewrite(&self.child)?, ScoreMode::Complete, boost)?
        };
        Ok(Box::new(ToParentWeight {
            child,
            parent_filter: self.parent_filter.clone(),
            searcher: s.clone(),
            mode: self.score_mode,
            needs_scores: mode.needs_scores(),
        }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        let c = s.rewrite(&self.child)?;
        if !Arc::ptr_eq(&c, &self.child) {
            return Ok(Some(Arc::new(ToParentQuery { child: c, parent_filter: self.parent_filter.clone(), score_mode: self.score_mode })));
        }
        Ok(None)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        format!("ToParentBlockJoinQuery ({})", self.child.lucene_string(schema))
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.child.visit(v)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Children of parents matching `parent` (Lucene's `ToChildBlockJoinQuery`): every doc of a
/// matching parent's block, scored with the parent's score.
#[derive(Clone, Debug)]
pub struct ToChildQuery {
    pub parent: QueryRef,
    pub parent_filter: QueryRef,
}

struct ToChildWeight {
    parent: Box<dyn Weight>,
    parent_filter: QueryRef,
    searcher: Searcher,
}

struct ToChildScorer {
    parent: Box<dyn Scorer>,
    parents: Arc<FixedBitSet>,
    doc: u32,
    /// Current parent (its children are being returned).
    cur_parent: u32,
    score: f32,
}

impl Scorable for ToChildScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl ToChildScorer {
    fn first_child_of(&self, p: u32) -> u32 {
        if p == 0 { 0 } else { self.parents.prev_set_bit(p - 1).map_or(0, |x| x + 1) }
    }
}

impl Scorer for ToChildScorer {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc + 1 };
        self.advance(t)
    }
    fn advance(&mut self, t: u32) -> u32 {
        loop {
            if self.cur_parent != u32::MAX && self.cur_parent != NO_MORE_DOCS && t < self.cur_parent {
                let d = t.max(self.first_child_of(self.cur_parent));
                if d < self.cur_parent {
                    self.doc = d;
                    return d;
                }
            }
            // The next matching parent after both `t` and the current parent.
            let mut target = t.saturating_add(1);
            if self.cur_parent != u32::MAX {
                target = target.max(self.cur_parent.saturating_add(1));
            }
            let pd = self.parent.doc();
            let p = if pd != u32::MAX && pd >= target { pd } else { self.parent.advance(target.min(NO_MORE_DOCS)) };
            if p == NO_MORE_DOCS {
                self.cur_parent = NO_MORE_DOCS;
                self.doc = NO_MORE_DOCS;
                return NO_MORE_DOCS;
            }
            self.cur_parent = p;
            self.score = self.parent.score();
        }
    }
    fn cost(&self) -> u64 {
        self.parent.cost()
    }
}

impl Weight for ToChildWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(parent) = self.parent.scorer(seg)? else { return Ok(None) };
        let parents = segment_bits(&self.searcher, seg, &self.parent_filter)?;
        Ok(Some(Box::new(ToChildScorer { parent, parents, doc: u32::MAX, cur_parent: u32::MAX, score: 0.0 })))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let parents = segment_bits(&self.searcher, seg, &self.parent_filter)?;
        if parents.get(doc) {
            return Ok(Explanation::no_match("Not a match", vec![]));
        }
        let p = parents.next_set_bit(doc);
        if p == NO_MORE_DOCS {
            return Ok(Explanation::no_match("Not a match", vec![]));
        }
        let e = self.parent.explain(seg, p)?;
        if !e.is_match() {
            return Ok(Explanation::no_match("Not a match", vec![]));
        }
        Ok(Explanation::matched(
            e.value,
            format!("Score based on parent document {}", seg.base + p),
            vec![e],
        ))
    }
}

impl Query for ToChildQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let parent = s.weight_of(&s.rewrite(&self.parent)?, mode, boost)?;
        Ok(Box::new(ToChildWeight { parent, parent_filter: self.parent_filter.clone(), searcher: s.clone() }))
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        format!("ToChildBlockJoinQuery ({})", self.parent.lucene_string(schema))
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.parent.visit(v)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
