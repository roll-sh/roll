//! Parent/child joins over global ordinals (Lucene's `JoinUtil.createJoinQuery` with an
//! `OrdinalMap`, as Elasticsearch's `has_child` and `has_parent` queries run it through their
//! `LateParsingQuery`).
//!
//! Every doc of the relation carries the join value in one sorted doc values field (the parent
//! id: a parent's own id, a child's parent's id). Weighting runs the *from* query (children for
//! `has_child`, parents for `has_parent`) over the whole searcher, recording per global ordinal
//! of the join value that it was seen, how often, and the aggregated score (`max`, `min`,
//! `sum`, `avg`; summed in `f32` in collection order, as Lucene does). The *to* query (the
//! other side's type filter) then becomes a two-phase scorer whose matches are the docs whose
//! join value was recorded — and, with `min`/`max` children, seen that many times — scoring
//! the aggregated score times the boost (or the boost with score mode `none`).

use std::any::Any;
use std::sync::Arc;

use search_codec::{DocValuesReader, NO_MORE_DOCS, SortedDocValues};

use crate::error::Result;
use crate::query::basic::MatchNoneQuery;
use crate::query::nested::JoinScoreMode;
use crate::query::term::term_text;
use crate::schema::Schema;
use crate::search::ordinals::GlobalOrdinals;
use crate::search::{
    Collector, Explanation, LeafCollector, Query, QueryRef, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight,
};
use crate::util::bitset::FixedBitSet;
use crate::util::java::float_to_string;

/// Elasticsearch's `LateParsingQuery`: docs of `to` whose join value some doc of `from` has.
#[derive(Clone, Debug)]
pub struct JoinQuery {
    /// The sorted doc values field holding the join value (`<join field>#<parent type>`).
    pub join_field: u32,
    /// The query whose docs' join values are collected (filtered to its type by the caller).
    pub from: QueryRef,
    /// The docs that can match (the other type's filter).
    pub to: QueryRef,
    pub score_mode: JoinScoreMode,
    /// Matches need between `min` and `max` collected docs (`has_child`'s `min_children`,
    /// `max_children`; 1 and `i32::MAX` disable the check).
    pub min: u32,
    pub max: u32,
}

impl JoinQuery {
    pub fn new(join_field: u32, from: QueryRef, to: QueryRef, score_mode: JoinScoreMode) -> JoinQuery {
        JoinQuery { join_field, from, to, score_mode, min: 1, max: i32::MAX as u32 }
    }

    fn min_max(&self) -> bool {
        self.min > 1 || self.max < i32::MAX as u32
    }
}

/// What the from-side search recorded per global ordinal.
struct Collected {
    seen: FixedBitSet,
    scores: Option<Vec<f32>>,
    counts: Option<Vec<u32>>,
    mode: JoinScoreMode,
    min: u32,
    max: u32,
    min_max: bool,
}

impl Collected {
    fn new(n: u32, mode: JoinScoreMode, min: u32, max: u32, min_max: bool) -> Collected {
        let unset = match mode {
            JoinScoreMode::Min => f32::INFINITY,
            JoinScoreMode::Max => f32::NEG_INFINITY,
            _ => 0.0,
        };
        Collected {
            seen: FixedBitSet::new(n),
            scores: (mode != JoinScoreMode::None).then(|| vec![unset; n as usize]),
            counts: (mode == JoinScoreMode::Avg || min_max).then(|| vec![0; n as usize]),
            mode,
            min,
            max,
            min_max,
        }
    }

    #[inline]
    fn add(&mut self, ord: u32, score: f32) {
        self.seen.set(ord);
        if let Some(s) = &mut self.scores {
            let e = &mut s[ord as usize];
            *e = match self.mode {
                JoinScoreMode::Min => e.min(score),
                JoinScoreMode::Max => e.max(score),
                _ => *e + score,
            };
        }
        if let Some(c) = &mut self.counts {
            c[ord as usize] += 1;
        }
    }

    #[inline]
    fn matches(&self, ord: u32) -> bool {
        if !self.seen.get(ord) {
            return false;
        }
        if self.min_max {
            let n = self.counts.as_ref().unwrap()[ord as usize];
            return n >= self.min && n <= self.max;
        }
        true
    }

    #[inline]
    fn score(&self, ord: u32) -> f32 {
        match (&self.scores, self.mode) {
            (None, _) => 1.0,
            (Some(s), JoinScoreMode::Avg) => s[ord as usize] / self.counts.as_ref().unwrap()[ord as usize] as f32,
            (Some(s), _) => s[ord as usize],
        }
    }
}

fn sorted_values(seg: &SegmentContext, field: u32) -> Option<SortedDocValues> {
    match seg.reader.doc_values(field)? {
        DocValuesReader::Sorted(v) => Some(v.clone()),
        _ => None,
    }
}

/// Records the join values of the from-side docs.
struct JoinCollector<'a> {
    c: &'a mut Collected,
    ords: &'a GlobalOrdinals,
    field: u32,
    scores: bool,
}

struct JoinLeaf<'a> {
    c: &'a mut Collected,
    ords: &'a GlobalOrdinals,
    seg: u32,
    values: Option<SortedDocValues>,
    scores: bool,
}

impl LeafCollector for JoinLeaf<'_> {
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> Result<()> {
        if let Some(v) = &self.values
            && let Some(ord) = v.ord(doc)
        {
            let g = self.ords.global(self.seg, ord) as u32;
            let score = if self.scores { scorer.score() } else { 0.0 };
            self.c.add(g, score);
        }
        Ok(())
    }
}

impl Collector for JoinCollector<'_> {
    fn score_mode(&self) -> ScoreMode {
        if self.scores { ScoreMode::Complete } else { ScoreMode::CompleteNoScores }
    }
    fn leaf<'b>(&'b mut self, seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'b>> {
        Ok(Box::new(JoinLeaf { c: self.c, ords: self.ords, seg: seg.ord, values: sorted_values(seg, self.field), scores: self.scores }))
    }
}

/// The resolved join, shared by the weight and its scorers.
struct JoinState {
    joined: Arc<Joined>,
    boost: f32,
    /// Score mode `none` without a children count: a constant `boost` (Lucene's
    /// `GlobalOrdinalsQuery`).
    constant: bool,
}

struct JoinWeight {
    to: Box<dyn Weight>,
    field: u32,
    state: Arc<JoinState>,
}

struct JoinScorer {
    approx: Box<dyn Scorer>,
    values: SortedDocValues,
    state: Arc<JoinState>,
    seg: u32,
    score: f32,
}

impl Scorable for JoinScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl Scorer for JoinScorer {
    fn doc(&self) -> u32 {
        self.approx.doc()
    }
    fn next(&mut self) -> u32 {
        loop {
            let d = self.approx.next();
            if d == NO_MORE_DOCS || self.matches() {
                return d;
            }
        }
    }
    fn advance(&mut self, t: u32) -> u32 {
        let d = self.approx.advance(t);
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
    fn approx_next(&mut self) -> u32 {
        self.approx.next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        self.approx.advance(t)
    }
    fn matches(&mut self) -> bool {
        let doc = self.approx.doc();
        let Some(ord) = self.values.ord(doc) else { return false };
        let w = &*self.state;
        let j = &*w.joined;
        let g = j.ords.global(self.seg, ord) as u32;
        let hit = if w.constant { j.collected.seen.get(g) } else { j.collected.matches(g) };
        if !hit {
            return false;
        }
        self.score = if w.constant { w.boost } else { j.collected.score(g) * w.boost };
        true
    }
    fn match_cost(&self) -> f32 {
        100.0
    }
}

impl Weight for JoinWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(values) = sorted_values(seg, self.field) else { return Ok(None) };
        let Some(approx) = self.to.scorer(seg)? else { return Ok(None) };
        Ok(Some(Box::new(JoinScorer { approx, values, state: self.state.clone(), seg: seg.ord, score: 0.0 })))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let Some(values) = sorted_values(seg, self.field) else { return Ok(Explanation::no_match("Not a match", vec![])) };
        let Some(ord) = values.ord(doc) else { return Ok(Explanation::no_match("Not a match", vec![])) };
        let mut term = Vec::new();
        values.lookup_ord(ord, &mut term);
        let value = term_text(&term);
        let st = &*self.state;
        let j = &*st.joined;
        let g = j.ords.global(seg.ord, ord) as u32;
        // Lucene checks the collected value only, not the to-side filter.
        let matched = if st.constant { j.collected.seen.get(g) } else { j.collected.matches(g) };
        if !matched {
            return Ok(Explanation::no_match(format!("Not a match, join value {value}"), vec![]));
        }
        if st.constant {
            return Ok(Explanation::leaf(st.boost, format!("A match, join value {value}")));
        }
        let score = j.collected.score(g);
        if st.boost == 1.0 {
            Ok(Explanation::leaf(score, format!("A match, join value {value}")))
        } else {
            Ok(Explanation::leaf(score * st.boost, format!("A match, join value {value}^{}", float_to_string(st.boost))))
        }
    }
}

impl JoinQuery {
    /// Runs the from side (Lucene's `JoinUtil.createJoinQuery`): the resolved join, or match
    /// nothing without segments or join values.
    fn resolve(&self, s: &Searcher) -> Result<QueryRef> {
        if s.segments().is_empty() {
            return Ok(Arc::new(MatchNoneQuery::new("JoinUtil.createJoinQuery with no segments")));
        }
        let Some(ords) = s.global_ordinals(self.join_field) else {
            return Ok(Arc::new(MatchNoneQuery::new("JoinUtil.createJoinQuery: no join values")));
        };
        let from = s.rewrite(&self.from)?;
        let to = s.rewrite(&self.to)?;
        let min_max = self.min_max();
        let scored = self.score_mode != JoinScoreMode::None;
        let n = ords.value_count().min(i32::MAX as u64) as u32;
        let mut collected = Collected::new(n, self.score_mode, self.min, self.max, min_max);
        {
            let mut c = JoinCollector { c: &mut collected, ords: &ords, field: self.join_field, scores: scored };
            s.search(&from, &mut c)?;
        }
        Ok(Arc::new(ResolvedJoin {
            q: JoinQuery { from, to, ..self.clone() },
            joined: Arc::new(Joined { ords, collected }),
        }))
    }
}

impl Query for JoinQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        self.resolve(s)?.weight(s, mode, boost)
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        Ok(Some(self.resolve(s)?))
    }

    /// Elasticsearch's `LateParsingQuery` string.
    fn lucene_string(&self, schema: &Schema) -> String {
        format!("LateParsingQuery {{joinField={}}}", schema.name(self.join_field))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The from side's collected join values.
struct Joined {
    ords: Arc<GlobalOrdinals>,
    collected: Collected,
}

/// A join with its from side run (Lucene's `GlobalOrdinalsQuery` /
/// `GlobalOrdinalsWithScoreQuery`): valid for the searcher it was resolved on.
#[derive(Clone)]
pub struct ResolvedJoin {
    q: JoinQuery,
    joined: Arc<Joined>,
}

impl std::fmt::Debug for ResolvedJoin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ResolvedJoin({:?})", self.q)
    }
}

impl Query for ResolvedJoin {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let q = &self.q;
        let min_max = q.min_max();
        // Without scores (and without a children count) only membership matters.
        let constant = !min_max && (q.score_mode == JoinScoreMode::None || !mode.needs_scores());
        Ok(Box::new(JoinWeight {
            to: s.weight_of(&q.to, ScoreMode::CompleteNoScores, 1.0)?,
            field: q.join_field,
            state: Arc::new(JoinState { joined: self.joined.clone(), boost, constant }),
        }))
    }

    /// Lucene's `toString`: `GlobalOrdinalsQuery{joinField=…}` without scores and children
    /// counts, else with `min`, `max` and the from query.
    fn lucene_string(&self, schema: &Schema) -> String {
        let q = &self.q;
        let field = schema.name(q.join_field);
        if q.score_mode == JoinScoreMode::None && !q.min_max() {
            format!("GlobalOrdinalsQuery{{joinField={field}}}")
        } else {
            format!("GlobalOrdinalsQuery{{joinField={field},min={},max={},fromQuery={}}}", q.min, q.max, q.from.lucene_string(schema))
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
