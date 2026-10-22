//! `BooleanQuery`: must / filter / should / must_not clauses with `minimum_should_match`.
//!
//! Rewriting applies Lucene 10's simplifications in Lucene's order (single clauses, pure
//! negatives, duplicate clauses merged into boosts, filters subsumed by musts, nested pure
//! disjunctions and conjunctions flattened, `msm` equal to the should count turned into musts),
//! so the executed tree — and therefore every explanation — is the one Elasticsearch runs.
//! Query identity for these rules is the query type plus its Lucene string.
//!
//! Execution picks the cheapest strategy per segment, like Lucene's `BooleanScorerSupplier`:
//! - top-k disjunctions: [`MaxScoreBulk`] (block-max MaxScore), with a filter when the query is
//!   filters + shoulds;
//! - exhaustive disjunctions (counting, sorting by a field): a windowed bulk scorer;
//! - top-k conjunctions: block-max conjunction; exhaustive ones (counting, exact totals,
//!   sorting): a bounded conjunction that filters the lead's postings blocks clause by clause;
//!   nested ones: leap-frog conjunction;
//! - `minimum_should_match` > 1 and nested top-k disjunctions: WAND;
//! - must_not: exclusion (scorer or exclusion windows around a bulk scorer).

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::query::basic::{BoostQuery, ConstScorer, ConstantScoreQuery, MatchAllQuery, MatchNoneQuery};
use crate::schema::Schema;
use crate::search::scorers::{
    BlockMaxConjunction, BlockMaxConjunctionBulk, ConjunctionBulk, ConjunctionScorer, DisjunctionScorer, MaxScoreBulk, ReqExclBulk, ReqExclScorer, ReqOptScorer,
    WandScorer, WindowedDisjunctionBulk, cost_with_msm,
};
use crate::search::{
    BulkScorer, DefaultBulkScorer, Explanation, Query, QueryRef, QueryVisitor, ScoreMode, Scorer, ScorerSupplier, Searcher,
    SegmentContext, Weight,
};

/// How a clause participates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Occur {
    Must,
    Filter,
    Should,
    MustNot,
}

impl Occur {
    /// Lucene's `Occur.toString()`.
    pub fn symbol(self) -> &'static str {
        match self {
            Occur::Must => "+",
            Occur::Filter => "#",
            Occur::Should => "",
            Occur::MustNot => "-",
        }
    }

    pub fn is_scoring(self) -> bool {
        matches!(self, Occur::Must | Occur::Should)
    }

    pub fn is_required(self) -> bool {
        matches!(self, Occur::Must | Occur::Filter)
    }
}

#[derive(Clone, Debug)]
pub struct BooleanClause {
    pub occur: Occur,
    pub query: QueryRef,
}

/// A boolean combination of queries.
#[derive(Clone, Debug, Default)]
pub struct BooleanQuery {
    pub clauses: Vec<BooleanClause>,
    pub minimum_should_match: usize,
}

/// Identity of a query for rewrite rules (type + Lucene string).
pub(crate) fn query_key(q: &QueryRef, schema: &Schema) -> (std::any::TypeId, String) {
    (q.as_any().type_id(), q.lucene_string(schema))
}

fn is<T: 'static>(q: &QueryRef) -> bool {
    q.as_any().is::<T>()
}

impl BooleanQuery {
    pub fn new() -> BooleanQuery {
        BooleanQuery::default()
    }

    pub fn add(mut self, occur: Occur, q: QueryRef) -> Self {
        self.clauses.push(BooleanClause { occur, query: q });
        self
    }

    pub fn must(self, q: QueryRef) -> Self {
        self.add(Occur::Must, q)
    }
    pub fn should(self, q: QueryRef) -> Self {
        self.add(Occur::Should, q)
    }
    pub fn filter(self, q: QueryRef) -> Self {
        self.add(Occur::Filter, q)
    }
    pub fn must_not(self, q: QueryRef) -> Self {
        self.add(Occur::MustNot, q)
    }
    pub fn with_msm(mut self, msm: usize) -> Self {
        self.minimum_should_match = msm;
        self
    }

    fn count(&self, o: Occur) -> usize {
        self.clauses.iter().filter(|c| c.occur == o).count()
    }

    fn of(&self, o: Occur) -> impl Iterator<Item = &QueryRef> {
        self.clauses.iter().filter(move |c| c.occur == o).map(|c| &c.query)
    }

    /// Lucene's `rewriteNoScoring` (what `ConstantScoreQuery` runs on its inner boolean):
    /// must clauses become filters, should clauses go unless they are needed for matching,
    /// boosts and constant-score wrappers of clauses are dropped, nested booleans recurse.
    /// `None` when nothing changes.
    pub fn rewrite_no_scoring(&self) -> Option<BooleanQuery> {
        let keep_should = self.minimum_should_match > 0 || self.count(Occur::Must) + self.count(Occur::Filter) == 0;
        let mut changed = false;
        let mut clauses = Vec::with_capacity(self.clauses.len());
        for c in &self.clauses {
            let mut q = c.query.clone();
            if let Some(b) = q.as_any().downcast_ref::<BoostQuery>() {
                q = b.query.clone();
            }
            if let Some(cs) = q.as_any().downcast_ref::<ConstantScoreQuery>() {
                q = cs.query.clone();
            }
            if let Some(b) = q.as_any().downcast_ref::<BooleanQuery>()
                && let Some(nb) = b.rewrite_no_scoring()
            {
                q = Arc::new(nb);
            }
            match c.occur {
                Occur::Should if !keep_should => changed = true,
                Occur::Must => {
                    clauses.push(BooleanClause { occur: Occur::Filter, query: q });
                    changed = true;
                }
                occur => {
                    changed |= !Arc::ptr_eq(&q, &c.query);
                    clauses.push(BooleanClause { occur, query: q });
                }
            }
        }
        changed.then(|| BooleanQuery { clauses, minimum_should_match: self.minimum_should_match })
    }

    /// A pure disjunction: only should clauses, msm ≤ 1.
    pub fn is_pure_disjunction(&self) -> bool {
        self.clauses.len() == self.count(Occur::Should) && self.minimum_should_match <= 1
    }

    fn rebuilt(&self, clauses: Vec<BooleanClause>, msm: usize) -> QueryRef {
        Arc::new(BooleanQuery { clauses, minimum_should_match: msm })
    }

    /// Clause sets as Lucene keeps them: must/should multisets, filter/must_not sets.
    fn clause_sets(&self, schema: &Schema) -> Vec<BooleanClause> {
        let mut out = Vec::new();
        for o in [Occur::Must, Occur::Filter, Occur::Should, Occur::MustNot] {
            let mut seen = std::collections::HashSet::new();
            for q in self.of(o) {
                if matches!(o, Occur::Filter | Occur::MustNot) && !seen.insert(query_key(q, schema)) {
                    continue;
                }
                out.push(BooleanClause { occur: o, query: q.clone() });
            }
        }
        out
    }

    fn do_rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        let schema = s.schema();
        let msm = self.minimum_should_match;
        if self.clauses.is_empty() {
            return Ok(Some(Arc::new(MatchNoneQuery::new("empty BooleanQuery"))));
        }
        if self.clauses.len() == self.count(Occur::MustNot) {
            return Ok(Some(Arc::new(MatchNoneQuery::new("pure negative BooleanQuery"))));
        }
        if self.clauses.len() == 1 {
            let c = &self.clauses[0];
            if msm == 1 && c.occur == Occur::Should {
                return Ok(Some(c.query.clone()));
            } else if msm == 0 {
                match c.occur {
                    Occur::Should | Occur::Must => return Ok(Some(c.query.clone())),
                    Occur::Filter => {
                        return Ok(Some(Arc::new(BoostQuery::new(
                            Arc::new(ConstantScoreQuery::new(c.query.clone())),
                            0.0,
                        ))));
                    }
                    Occur::MustNot => {}
                }
            }
        }
        // Rewrite each clause.
        {
            let mut out = Vec::with_capacity(self.clauses.len());
            let mut changed = false;
            for c in &self.clauses {
                let q = &c.query;
                let r = if matches!(c.occur, Occur::Filter | Occur::MustNot) {
                    let cs: QueryRef = Arc::new(ConstantScoreQuery::new(q.clone()));
                    let r = s.rewrite(&cs)?;
                    match r.as_any().downcast_ref::<ConstantScoreQuery>() {
                        Some(c) => c.query.clone(),
                        None => r,
                    }
                } else {
                    s.rewrite(q)?
                };
                if !Arc::ptr_eq(&r, q) || is::<MatchNoneQuery>(q) {
                    changed = true;
                    if is::<MatchNoneQuery>(&r) {
                        match c.occur {
                            Occur::Should | Occur::MustNot => {}
                            Occur::Must | Occur::Filter => return Ok(Some(r)),
                        }
                    } else {
                        out.push(BooleanClause { occur: c.occur, query: r });
                    }
                } else {
                    out.push(c.clone());
                }
            }
            if changed {
                return Ok(Some(self.rebuilt(out, msm)));
            }
        }
        // Deduplicated filter / must_not clauses.
        let sets = self.clause_sets(schema);
        if sets.len() != self.clauses.len() {
            return Ok(Some(self.rebuilt(sets, msm)));
        }
        let keys = |o: Occur| -> Vec<(std::any::TypeId, String)> { self.of(o).map(|q| query_key(q, schema)).collect() };
        if self.count(Occur::MustNot) > 0 {
            let mn = keys(Occur::MustNot);
            let mf: Vec<_> = keys(Occur::Must).into_iter().chain(keys(Occur::Filter)).collect();
            if mn.iter().any(|k| mf.contains(k)) {
                return Ok(Some(Arc::new(MatchNoneQuery::new("FILTER or MUST clause also in MUST_NOT"))));
            }
            if self.of(Occur::MustNot).any(is::<MatchAllQuery>) {
                return Ok(Some(Arc::new(MatchNoneQuery::new("MUST_NOT clause is MatchAllDocsQuery"))));
            }
        }
        if self.count(Occur::Filter) > 0 {
            let musts = keys(Occur::Must);
            let mut filters: Vec<QueryRef> = self.of(Occur::Filter).cloned().collect();
            let before = filters.len();
            if filters.len() > 1 || !musts.is_empty() {
                filters.retain(|q| !is::<MatchAllQuery>(q));
            }
            filters.retain(|q| !musts.contains(&query_key(q, schema)));
            if filters.len() != before {
                let mut out: Vec<BooleanClause> = self.clauses.iter().filter(|c| c.occur != Occur::Filter).cloned().collect();
                out.extend(filters.into_iter().map(|q| BooleanClause { occur: Occur::Filter, query: q }));
                return Ok(Some(self.rebuilt(out, msm)));
            }
        }
        if self.count(Occur::Should) > 0 && self.count(Occur::Filter) > 0 {
            let fk = keys(Occur::Filter);
            let inter: Vec<_> = keys(Occur::Should).into_iter().filter(|k| fk.contains(k)).collect();
            if !inter.is_empty() {
                let mut out = Vec::new();
                let mut m = msm as isize;
                for c in &self.clauses {
                    if inter.contains(&query_key(&c.query, schema)) {
                        if c.occur == Occur::Should {
                            out.push(BooleanClause { occur: Occur::Must, query: c.query.clone() });
                            m -= 1;
                        }
                    } else {
                        out.push(c.clone());
                    }
                }
                return Ok(Some(self.rebuilt(out, m.max(0) as usize)));
            }
        }
        // Duplicate should / must clauses become one clause with summed boosts.
        for occ in [Occur::Should, Occur::Must] {
            if self.count(occ) == 0 || (occ == Occur::Should && msm > 1) {
                continue;
            }
            let mut order: Vec<(std::any::TypeId, String)> = Vec::new();
            let mut merged: HashMap<(std::any::TypeId, String), (QueryRef, f64)> = HashMap::new();
            for q in self.of(occ) {
                let mut boost = 1f64;
                let mut inner = q.clone();
                while let Some(b) = inner.as_any().downcast_ref::<BoostQuery>() {
                    boost *= b.boost as f64;
                    let n = b.query.clone();
                    inner = n;
                }
                let k = query_key(&inner, schema);
                match merged.get_mut(&k) {
                    Some(e) => e.1 += boost,
                    None => {
                        order.push(k.clone());
                        merged.insert(k, (inner, boost));
                    }
                }
            }
            if merged.len() != self.count(occ) {
                let mut out = Vec::new();
                for k in order {
                    let (q, b) = merged.remove(&k).unwrap();
                    let b = b as f32;
                    let q: QueryRef = if b != 1.0 { Arc::new(BoostQuery::new(q, b)) } else { q };
                    out.push(BooleanClause { occur: occ, query: q });
                }
                out.extend(self.clauses.iter().filter(|c| c.occur != occ).cloned());
                return Ok(Some(self.rebuilt(out, msm)));
            }
        }
        // A match-all must with filters: constant score of the filters.
        if self.count(Occur::Must) == 1 && self.count(Occur::Filter) > 0 {
            let mut must = self.of(Occur::Must).next().unwrap().clone();
            let mut boost = 1f32;
            if let Some(b) = must.as_any().downcast_ref::<BoostQuery>() {
                boost = b.boost;
                let n = b.query.clone();
                must = n;
            }
            if is::<MatchAllQuery>(&must) {
                let inner: Vec<BooleanClause> = self
                    .clauses
                    .iter()
                    .filter(|c| matches!(c.occur, Occur::Filter | Occur::MustNot))
                    .cloned()
                    .collect();
                let mut r: QueryRef = Arc::new(ConstantScoreQuery::new(Arc::new(BooleanQuery {
                    clauses: inner,
                    minimum_should_match: 0,
                })));
                if boost != 1.0 {
                    r = Arc::new(BoostQuery::new(r, boost));
                }
                let mut out = vec![BooleanClause { occur: Occur::Must, query: r }];
                out.extend(self.of(Occur::Should).map(|q| BooleanClause { occur: Occur::Should, query: q.clone() }));
                return Ok(Some(self.rebuilt(out, msm)));
            }
        }
        // Flatten nested pure disjunctions in should clauses.
        if msm <= 1 {
            let mut out = Vec::new();
            let mut changed = false;
            for c in &self.clauses {
                if c.occur == Occur::Should
                    && let Some(b) = c.query.as_any().downcast_ref::<BooleanQuery>()
                    && b.is_pure_disjunction()
                {
                    changed = true;
                    out.extend(b.clauses.iter().cloned());
                    continue;
                }
                out.push(c.clone());
            }
            if changed {
                return Ok(Some(self.rebuilt(out, msm)));
            }
        }
        // Flatten nested conjunctions in required clauses.
        {
            let mut out = Vec::new();
            let mut changed = false;
            for c in &self.clauses {
                if c.occur.is_required()
                    && let Some(b) = c.query.as_any().downcast_ref::<BooleanQuery>()
                    && b.minimum_should_match == 0
                    && b.count(Occur::Should) == 0
                {
                    changed = true;
                    for ic in &b.clauses {
                        if matches!(ic.occur, Occur::Filter | Occur::MustNot) || c.occur == Occur::Must {
                            out.push(ic.clone());
                        } else {
                            out.push(BooleanClause { occur: Occur::Filter, query: ic.query.clone() });
                        }
                    }
                    continue;
                }
                out.push(c.clone());
            }
            if changed {
                return Ok(Some(self.rebuilt(out, msm)));
            }
        }
        let shoulds = self.count(Occur::Should);
        if shoulds < msm {
            return Ok(Some(Arc::new(MatchNoneQuery::new("SHOULD clause count less than minimumNumberShouldMatch"))));
        }
        if shoulds > 0 && shoulds == msm {
            let out = self
                .clauses
                .iter()
                .map(|c| {
                    if c.occur == Occur::Should {
                        BooleanClause { occur: Occur::Must, query: c.query.clone() }
                    } else {
                        c.clone()
                    }
                })
                .collect();
            return Ok(Some(self.rebuilt(out, 0)));
        }
        if shoulds == 0 && self.count(Occur::Must) == 1 {
            let m = self.of(Occur::Must).next().unwrap();
            if let Some(inner) = m.as_any().downcast_ref::<BooleanQuery>()
                && inner.clauses.len() == inner.count(Occur::Should)
            {
                let mut out: Vec<BooleanClause> = self.clauses.iter().filter(|c| c.occur != Occur::Must).cloned().collect();
                out.extend(inner.clauses.iter().cloned());
                return Ok(Some(self.rebuilt(out, inner.minimum_should_match.max(1))));
            }
        }
        Ok(None)
    }
}

impl Query for BooleanQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let max = s.limits().max_clause_count;
        if self.clauses.len() > max {
            return Err(Error::too_many_clauses(max));
        }
        let mut clauses = Vec::with_capacity(self.clauses.len());
        for c in &self.clauses {
            let m = if c.occur.is_scoring() { mode } else { ScoreMode::CompleteNoScores };
            let q = s.rewrite(&c.query)?;
            clauses.push((c.occur, s.filter_weight(&q, m, boost)?, q.lucene_string(s.schema())));
        }
        Ok(Box::new(BooleanWeight { clauses, msm: self.minimum_should_match, mode, pure_disjunction: self.is_pure_disjunction() }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        self.do_rewrite(s)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let msm = self.minimum_should_match;
        let mut b = String::new();
        if msm > 0 {
            b.push('(');
        }
        for (i, c) in self.clauses.iter().enumerate() {
            b.push_str(c.occur.symbol());
            if c.query.as_any().is::<BooleanQuery>() {
                b.push('(');
                b.push_str(&c.query.lucene_string(schema));
                b.push(')');
            } else {
                b.push_str(&c.query.lucene_string(schema));
            }
            if i + 1 != self.clauses.len() {
                b.push(' ');
            }
        }
        if msm > 0 {
            b.push(')');
            b.push('~');
            b.push_str(&msm.to_string());
        }
        b
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        for c in &self.clauses {
            if c.occur == Occur::MustNot && !v.wants_negated() {
                continue;
            }
            c.query.visit(v);
        }
    }

    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_tag(h, "bool");
        h.write_usize(self.minimum_should_match);
        h.write_usize(self.clauses.len());
        for c in &self.clauses {
            crate::search::hash_debug(h, "occur", &c.occur);
            if !c.query.cache_identity(h) {
                return false;
            }
        }
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct BooleanWeight {
    clauses: Vec<(Occur, Box<dyn Weight>, String)>,
    msm: usize,
    mode: ScoreMode,
    pure_disjunction: bool,
}

/// Per-segment suppliers grouped by occur.
struct Subs {
    must: Vec<Box<dyn ScorerSupplier>>,
    filter: Vec<Box<dyn ScorerSupplier>>,
    should: Vec<Box<dyn ScorerSupplier>>,
    must_not: Vec<Box<dyn ScorerSupplier>>,
    msm: usize,
}

impl BooleanWeight {
    fn subs(&self, seg: &SegmentContext) -> Result<Option<Subs>> {
        let mut s = Subs { must: Vec::new(), filter: Vec::new(), should: Vec::new(), must_not: Vec::new(), msm: self.msm };
        for (o, w, _) in &self.clauses {
            match w.scorer_supplier(seg)? {
                None => {
                    if o.is_required() {
                        return Ok(None);
                    }
                }
                Some(ss) => match o {
                    Occur::Must => s.must.push(ss),
                    Occur::Filter => s.filter.push(ss),
                    Occur::Should => s.should.push(ss),
                    Occur::MustNot => s.must_not.push(ss),
                },
            }
        }
        if s.should.len() == s.msm {
            let sh = std::mem::take(&mut s.should);
            s.must.extend(sh);
            s.msm = 0;
        }
        if s.filter.is_empty() && s.must.is_empty() && s.should.is_empty() {
            return Ok(None);
        }
        if s.should.len() < s.msm {
            return Ok(None);
        }
        if !self.mode.needs_scores() && s.msm == 0 && s.must.len() + s.filter.len() > 0 {
            s.should.clear();
        }
        Ok(Some(s))
    }
}

fn get_all(v: Vec<Box<dyn ScorerSupplier>>, lead: u64) -> Result<Vec<Box<dyn Scorer>>> {
    v.into_iter().map(|s| s.get(lead)).collect()
}

/// The supplier of one segment's boolean scorer.
struct BoolSupplier {
    subs: Subs,
    mode: ScoreMode,
    max_doc: u32,
    cost: u64,
}

impl BoolSupplier {
    fn new(subs: Subs, mode: ScoreMode, max_doc: u32) -> BoolSupplier {
        let should_cost = {
            let mut c: Vec<u64> = subs.should.iter().map(|s| s.cost()).collect();
            if c.is_empty() { u64::MAX } else { cost_with_msm(&mut c, subs.msm.max(1)) }
        };
        let min_req = subs.must.iter().chain(&subs.filter).map(|s| s.cost()).min();
        let cost = match min_req {
            Some(m) if subs.msm == 0 => m,
            _ => min_req.unwrap_or(u64::MAX).min(should_cost),
        };
        BoolSupplier { subs, mode, max_doc, cost }
    }

    fn req(&self, filter: Vec<Box<dyn ScorerSupplier>>, must: Vec<Box<dyn ScorerSupplier>>, lead: u64, top_level: bool) -> Result<Box<dyn Scorer>> {
        let needs = self.mode.needs_scores();
        if filter.len() + must.len() == 1 {
            let only_filter = must.is_empty();
            let s = filter.into_iter().chain(must).next().unwrap().get(lead)?;
            if !needs {
                return Ok(s);
            }
            if only_filter {
                return Ok(Box::new(ConstScorer { inner: s, score: 0.0 }));
            }
            return Ok(s);
        }
        let filters = get_all(filter, lead)?;
        let mut scoring = get_all(must, lead)?;
        if self.mode == ScoreMode::TopScores && scoring.len() > 1 && top_level {
            let bm: Box<dyn Scorer> = Box::new(BlockMaxConjunction::new(scoring));
            if filters.is_empty() {
                return Ok(bm);
            }
            scoring = vec![bm];
        }
        let mut all: Vec<(Box<dyn Scorer>, bool)> = filters.into_iter().map(|s| (s, false)).collect();
        all.extend(scoring.into_iter().map(|s| (s, needs)));
        Ok(Box::new(ConjunctionScorer::new(all)))
    }

    fn opt(&self, opt: Vec<Box<dyn ScorerSupplier>>, msm: usize, mode: ScoreMode, lead: u64, top_level: bool) -> Result<Box<dyn Scorer>> {
        if opt.len() == 1 {
            return opt.into_iter().next().unwrap().get(lead);
        }
        let subs = get_all(opt, lead)?;
        if (mode == ScoreMode::TopScores && top_level) || msm > 1 {
            Ok(Box::new(WandScorer::new(subs, msm, mode == ScoreMode::TopScores, mode.needs_scores(), lead)))
        } else {
            Ok(Box::new(DisjunctionScorer::new(subs, mode.needs_scores())))
        }
    }

    fn excl(&self, main: Box<dyn Scorer>, prohibited: Vec<Box<dyn ScorerSupplier>>, lead: u64) -> Result<Box<dyn Scorer>> {
        if prohibited.is_empty() {
            return Ok(main);
        }
        let ex = self.opt(prohibited, 1, ScoreMode::CompleteNoScores, lead, false)?;
        Ok(Box::new(ReqExclScorer::new(main, ex)))
    }

    fn scorer(self, lead: u64, top_level: bool) -> Result<Box<dyn Scorer>> {
        let lead = lead.min(self.cost);
        let mode = self.mode;
        let Subs { must, filter, should, must_not, msm } = Subs {
            must: self.subs.must,
            filter: self.subs.filter,
            should: self.subs.should,
            must_not: self.subs.must_not,
            msm: self.subs.msm,
        };
        let me = BoolSupplier {
            subs: Subs { must: Vec::new(), filter: Vec::new(), should: Vec::new(), must_not: Vec::new(), msm },
            mode,
            max_doc: self.max_doc,
            cost: self.cost,
        };
        let pure_filter = should.is_empty() && must.is_empty();
        let s: Box<dyn Scorer> = if should.is_empty() {
            let r = me.req(filter, must, lead, top_level)?;
            me.excl(r, must_not, lead)?
        } else if filter.is_empty() && must.is_empty() {
            let o = me.opt(should, msm, mode, lead, top_level)?;
            me.excl(o, must_not, lead)?
        } else if msm > 0 {
            let r = me.req(filter, must, lead, false)?;
            let r = me.excl(r, must_not, lead)?;
            let o = me.opt(should, msm, mode, lead, false)?;
            Box::new(ConjunctionScorer::new(vec![(r, true), (o, true)]))
        } else {
            let r = me.req(filter, must, lead, false)?;
            let r = me.excl(r, must_not, lead)?;
            let o = me.opt(should, msm, mode, lead, false)?;
            Box::new(ReqOptScorer::new(r, o, mode == ScoreMode::TopScores))
        };
        if mode == ScoreMode::TopScores && pure_filter {
            return Ok(Box::new(ConstScorer { inner: s, score: 0.0 }));
        }
        Ok(s)
    }

    /// Lucene's `booleanScorer()`: a specialized bulk scorer, or `None` to iterate the scorer.
    fn bulk(self) -> Result<std::result::Result<Box<dyn BulkScorer>, BoolSupplier>> {
        let n_opt = self.subs.should.len();
        let n_must = self.subs.must.len();
        let n_req = n_must + self.subs.filter.len();
        let msm = self.subs.msm;
        let mode = self.mode;
        let positive: Box<dyn BulkScorer>;
        let mut me = self;
        if n_req == 0 {
            let threshold = if msm <= 1 { 0 } else { me.max_doc as u64 / 3 };
            if msm > 1 && me.cost < threshold {
                return Ok(Err(me));
            }
            if n_opt == 1 && msm <= 1 {
                return Ok(Err(me));
            }
            if mode == ScoreMode::TopScores {
                if msm > 1 {
                    return Ok(Err(me));
                }
                let subs = get_all(std::mem::take(&mut me.subs.should), u64::MAX)?;
                positive = Box::new(MaxScoreBulk::new(me.max_doc, subs, None));
            } else {
                let mut c: Vec<u64> = me.subs.should.iter().map(|s| s.cost()).collect();
                let sc = cost_with_msm(&mut c, msm.max(1));
                let subs = get_all(std::mem::take(&mut me.subs.should), sc)?;
                positive = Box::new(WindowedDisjunctionBulk::new(subs, msm.max(1), mode.needs_scores(), me.max_doc));
            }
        } else if n_must == 0 && n_opt > 1 && msm == 1 && mode == ScoreMode::TopScores {
            let cost = me.cost;
            let subs = get_all(std::mem::take(&mut me.subs.should), cost)?;
            let mut filters = get_all(std::mem::take(&mut me.subs.filter), cost)?;
            let f: Box<dyn Scorer> = if filters.len() == 1 {
                filters.pop().unwrap()
            } else {
                Box::new(ConjunctionScorer::new(filters.into_iter().map(|s| (s, false)).collect()))
            };
            positive = Box::new(MaxScoreBulk::new(me.max_doc, subs, Some(f)));
        } else if n_opt == 0 && msm == 0 && n_must > 1 && mode == ScoreMode::TopScores {
            let lead = me.cost;
            let filters = get_all(std::mem::take(&mut me.subs.filter), lead)?;
            let scoring = get_all(std::mem::take(&mut me.subs.must), lead)?;
            if filters.iter().chain(&scoring).any(|s| s.has_two_phase()) {
                // What `req` builds, driven doc at a time.
                let bm: Box<dyn Scorer> = Box::new(BlockMaxConjunction::new(scoring));
                let scorer: Box<dyn Scorer> = if filters.is_empty() {
                    bm
                } else {
                    let mut all: Vec<(Box<dyn Scorer>, bool)> = filters.into_iter().map(|s| (s, false)).collect();
                    all.push((bm, true));
                    Box::new(ConjunctionScorer::new(all))
                };
                positive = Box::new(DefaultBulkScorer::new(scorer));
            } else {
                // Filters join as clauses scoring 0.
                let mut all = scoring;
                all.extend(filters.into_iter().map(|f| Box::new(ConstScorer { inner: f, score: 0.0 }) as Box<dyn Scorer>));
                positive = Box::new(BlockMaxConjunctionBulk::new(me.max_doc, all));
            }
        } else if n_opt == 0 && msm == 0 && n_req > 1 && mode != ScoreMode::TopScores {
            // Every match visited: a bounded conjunction (batched unless a clause is two-phase).
            let lead = me.cost;
            let needs = mode.needs_scores();
            let mut all: Vec<(Box<dyn Scorer>, bool)> =
                get_all(std::mem::take(&mut me.subs.filter), lead)?.into_iter().map(|s| (s, false)).collect();
            all.extend(get_all(std::mem::take(&mut me.subs.must), lead)?.into_iter().map(|s| (s, needs)));
            positive = if all.iter().any(|(s, _)| s.has_two_phase()) {
                Box::new(DefaultBulkScorer::new(Box::new(ConjunctionScorer::new(all))))
            } else {
                Box::new(ConjunctionBulk::new(all))
            };
        } else {
            return Ok(Err(me));
        }
        if me.subs.must_not.is_empty() {
            return Ok(Ok(positive));
        }
        let pc = positive.cost();
        let mut prohibited = get_all(std::mem::take(&mut me.subs.must_not), pc)?;
        let excl: Box<dyn Scorer> = if prohibited.len() == 1 {
            prohibited.pop().unwrap()
        } else {
            Box::new(DisjunctionScorer::new(prohibited, false))
        };
        Ok(Ok(Box::new(ReqExclBulk { req: positive, excl })))
    }
}

impl ScorerSupplier for BoolSupplier {
    fn cost(&self) -> u64 {
        self.cost
    }
    fn get(self: Box<Self>, lead: u64) -> Result<Box<dyn Scorer>> {
        (*self).scorer(lead, false)
    }
}

impl Weight for BooleanWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(subs) = self.subs(seg)? else { return Ok(None) };
        // Parents (dis_max, function_score, joins…) combine this score with others and don't
        // pass a minimum competitive score down: no top-level pruning structures here (the
        // search loop builds those through `bulk_scorer`).
        Ok(Some(BoolSupplier::new(subs, self.mode, seg.max_doc()).scorer(u64::MAX, false)?))
    }

    fn scorer_supplier(&self, seg: &SegmentContext) -> Result<Option<Box<dyn ScorerSupplier>>> {
        let Some(subs) = self.subs(seg)? else { return Ok(None) };
        Ok(Some(Box::new(BoolSupplier::new(subs, self.mode, seg.max_doc()))))
    }

    fn bulk_scorer(&self, seg: &SegmentContext, _mode: ScoreMode) -> Result<Option<Box<dyn BulkScorer>>> {
        let Some(subs) = self.subs(seg)? else { return Ok(None) };
        let sup = BoolSupplier::new(subs, self.mode, seg.max_doc());
        match sup.bulk()? {
            Ok(b) => Ok(Some(b)),
            Err(sup) => {
                let scorer = sup.scorer(u64::MAX, true)?;
                Ok(Some(Box::new(DefaultBulkScorer::new(scorer))))
            }
        }
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let mut subs = Vec::new();
        let mut failing = Vec::new();
        let mut fail = false;
        let mut match_count = 0;
        let mut should_count = 0;
        for (o, w, desc) in &self.clauses {
            let mut e = w.explain(seg, doc)?;
            if !o.is_scoring() || !self.mode.needs_scores() {
                // Non-scoring clauses go through Lucene's query cache, whose wrapper weight
                // explains as a constant score of 1 (Elasticsearch enables the cache).
                e = if e.is_match() {
                    Explanation::leaf(1f32, desc.clone())
                } else {
                    Explanation::no_match(format!("{desc} doesn't match id {doc}"), Vec::new())
                };
            }
            if e.is_match() {
                if o.is_scoring() {
                    subs.push(e);
                } else if o.is_required() {
                    subs.push(Explanation::matched(
                        0f32,
                        "match on required clause, product of:",
                        vec![Explanation::leaf(0f32, "# clause"), e],
                    ));
                } else {
                    subs.push(Explanation::no_match(format!("match on prohibited clause ({desc})"), vec![e]));
                    fail = true;
                }
                if *o != Occur::MustNot {
                    match_count += 1;
                }
                if *o == Occur::Should {
                    should_count += 1;
                }
            } else if o.is_required() {
                subs.push(Explanation::no_match(format!("no match on required clause ({desc})"), vec![e]));
                fail = true;
            } else if *o == Occur::Should {
                failing.push(Explanation::no_match(format!("no match on optional clause ({desc})"), vec![e]));
            }
        }
        if fail {
            return Ok(Explanation::no_match("Failure to meet condition(s) of required/prohibited clause(s)", subs));
        }
        if match_count == 0 {
            subs.extend(failing);
            return Ok(Explanation::no_match("No matching clauses", subs));
        }
        if should_count < self.msm {
            subs.extend(failing);
            return Ok(Explanation::no_match(
                format!("Failure to match minimum number of optional clauses: {}, matched: {should_count}", self.msm),
                subs,
            ));
        }
        let mut score = 0f32;
        if let Some(mut s) = self.scorer(seg)?
            && s.advance(doc) == doc
        {
            score = s.score();
        }
        Ok(Explanation::matched(score, "sum of:", subs))
    }

    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        let num_docs = seg.num_docs;
        let opt_count = |occ: Occur| -> Result<Option<u32>> {
            let mut c = 0u32;
            let mut unknown = false;
            for (o, w, _) in &self.clauses {
                if *o != occ {
                    continue;
                }
                match w.count(seg)? {
                    None => unknown = true,
                    Some(n) if n == num_docs => return Ok(Some(n)),
                    Some(0) => {}
                    Some(n) => {
                        if c == 0 {
                            c = n;
                        } else {
                            unknown = true;
                        }
                    }
                }
            }
            Ok(if unknown { None } else { Some(c) })
        };
        if self.pure_disjunction {
            return opt_count(Occur::Should);
        }
        let has_req = self.clauses.iter().any(|(o, _, _)| o.is_required());
        let positive = if has_req && self.msm == 0 {
            let mut req = num_docs;
            let mut res = Some(num_docs);
            for (o, w, _) in &self.clauses {
                if !o.is_required() {
                    continue;
                }
                match w.count(seg)? {
                    None => {
                        res = None;
                        break;
                    }
                    Some(0) => {
                        res = Some(0);
                        break;
                    }
                    Some(n) if n == num_docs => {}
                    Some(n) if req == num_docs => {
                        req = n;
                        res = Some(n);
                    }
                    Some(_) => {
                        res = None;
                        break;
                    }
                }
            }
            res
        } else {
            None
        };
        if positive == Some(0) {
            return Ok(Some(0));
        }
        match opt_count(Occur::MustNot)? {
            None => Ok(None),
            Some(0) => Ok(positive),
            Some(p) if p == num_docs => Ok(Some(0)),
            Some(p) => match positive {
                Some(n) if n == num_docs => Ok(Some(num_docs - p)),
                _ => Ok(None),
            },
        }
    }
}
