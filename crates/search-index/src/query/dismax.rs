//! `DisjunctionMaxQuery` (`dis_max`) and `SynonymQuery` (terms at the same position scored as
//! one term).
//!
//! Dis-max scores a doc by its best clause plus `tie_breaker` times the others (Lucene's
//! arithmetic: the maximum starts at 0, others are summed in double). A synonym group scores
//! like a single term whose frequency is the boosted sum of its terms' frequencies and whose
//! statistics are the group's maximum document frequency and summed total frequency.

use std::any::Any;
use std::sync::Arc;

use search_codec::{NO_MORE_DOCS, PostingsCursor, PostingsFlags};

use crate::error::Result;
use crate::query::basic::MatchNoneQuery;
use crate::query::boolean::{BooleanQuery, Occur};
use crate::query::term::{Norms, SIMILARITY_NAME, TermQuery, TermStates, scoring_stats, term_text};
use crate::schema::Schema;
use crate::search::scorers::DisjunctionScorer;
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::similarity::{SimScorer, TermStats};
use crate::util::java::float_to_string;

/// The best of several queries, plus `tie_breaker` × the others.
#[derive(Clone, Debug)]
pub struct DisMaxQuery {
    pub disjuncts: Vec<QueryRef>,
    pub tie_breaker: f32,
}

impl DisMaxQuery {
    pub fn new(disjuncts: Vec<QueryRef>, tie_breaker: f32) -> DisMaxQuery {
        DisMaxQuery { disjuncts, tie_breaker }
    }
}

struct DisMaxWeight {
    weights: Vec<Box<dyn Weight>>,
    tie: f32,
    needs_scores: bool,
}

impl Weight for DisMaxWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let mut subs = Vec::new();
        for w in &self.weights {
            if let Some(s) = w.scorer(seg)? {
                subs.push(s);
            }
        }
        Ok(match subs.len() {
            0 => None,
            1 => subs.pop(),
            _ => Some(Box::new(DisjunctionScorer::dismax(subs, self.tie, self.needs_scores))),
        })
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let mut matched = false;
        let mut max = 0f64;
        let mut other = 0f64;
        let mut subs = Vec::new();
        for w in &self.weights {
            let e = w.explain(seg, doc)?;
            if e.is_match() {
                matched = true;
                let s = e.value.as_f64();
                subs.push(e);
                if s >= max {
                    other += max;
                    max = s;
                } else {
                    other += s;
                }
            }
        }
        if !matched {
            return Ok(Explanation::no_match("No matching clause", vec![]));
        }
        let score = (max + other * self.tie as f64) as f32;
        let desc = if self.tie == 0.0 {
            "max of:".to_string()
        } else {
            format!("max plus {} times others of:", float_to_string(self.tie))
        };
        Ok(Explanation::matched(score, desc, subs))
    }
}

impl Query for DisMaxQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let mut weights = Vec::with_capacity(self.disjuncts.len());
        for q in &self.disjuncts {
            weights.push(s.weight_of(&s.rewrite(q)?, mode, boost)?);
        }
        Ok(Box::new(DisMaxWeight { weights, tie: self.tie_breaker, needs_scores: mode.needs_scores() }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        if self.disjuncts.is_empty() {
            return Ok(Some(Arc::new(MatchNoneQuery::new("empty DisjunctionMaxQuery"))));
        }
        if self.disjuncts.len() == 1 {
            return Ok(Some(self.disjuncts[0].clone()));
        }
        if self.tie_breaker == 1.0 {
            let mut b = BooleanQuery::new();
            for q in &self.disjuncts {
                b = b.add(Occur::Should, q.clone());
            }
            return Ok(Some(Arc::new(b)));
        }
        let mut changed = false;
        let mut out = Vec::with_capacity(self.disjuncts.len());
        for q in &self.disjuncts {
            let r = s.rewrite(q)?;
            changed |= !Arc::ptr_eq(&r, q);
            out.push(r);
        }
        Ok(changed.then(|| Arc::new(DisMaxQuery { disjuncts: out, tie_breaker: self.tie_breaker }) as QueryRef))
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let mut b = String::from("(");
        for (i, q) in self.disjuncts.iter().enumerate() {
            if q.as_any().is::<BooleanQuery>() {
                b.push('(');
                b.push_str(&q.lucene_string(schema));
                b.push(')');
            } else {
                b.push_str(&q.lucene_string(schema));
            }
            if i + 1 != self.disjuncts.len() {
                b.push_str(" | ");
            }
        }
        b.push(')');
        if self.tie_breaker != 0.0 {
            b.push('~');
            b.push_str(&float_to_string(self.tie_breaker));
        }
        b
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        for q in &self.disjuncts {
            q.visit(v);
        }
    }

    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        // The tie breaker only changes scores.
        crate::search::hash_tag(h, "dis_max");
        h.write_usize(self.disjuncts.len());
        self.disjuncts.iter().all(|d| d.cache_identity(h))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Terms scored as one (`SynonymQuery`), each with a frequency boost.
#[derive(Clone, Debug, PartialEq)]
pub struct SynonymQuery {
    pub field: u32,
    pub terms: Vec<(Vec<u8>, f32)>,
}

impl SynonymQuery {
    pub fn new(field: u32, terms: Vec<Vec<u8>>) -> SynonymQuery {
        SynonymQuery { field, terms: terms.into_iter().map(|t| (t, 1.0)).collect() }
    }
}

struct SynWeight {
    field: u32,
    states: Vec<(TermStates, f32)>,
    sim: Option<Arc<SimScorer>>,
    desc: String,
}

struct SynSub {
    c: PostingsCursor,
    boost: f32,
}

/// Union of the synonyms' postings with a combined frequency.
struct SynScorer {
    subs: Vec<SynSub>,
    doc: u32,
    norms: Norms,
    sim: Arc<SimScorer>,
    cost: u64,
}

impl SynScorer {
    fn freq_now(&mut self) -> f32 {
        let mut f = 0f32;
        let mut first = true;
        for s in self.subs.iter_mut() {
            if s.c.doc() == self.doc {
                let v = s.boost * s.c.freq() as f32;
                if first {
                    f = v;
                    first = false;
                } else {
                    f += v;
                }
            }
        }
        f
    }
}

impl Scorable for SynScorer {
    fn score(&mut self) -> f32 {
        let f = self.freq_now();
        self.sim.score(f, self.norms.get(self.doc))
    }
}

impl Scorer for SynScorer {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc + 1 };
        self.advance(t)
    }
    fn advance(&mut self, t: u32) -> u32 {
        let mut m = NO_MORE_DOCS;
        for s in self.subs.iter_mut() {
            let d = s.c.doc();
            let d = if d == u32::MAX || d < t { s.c.advance(t) } else { d };
            m = m.min(d);
        }
        self.doc = m;
        m
    }
    fn cost(&self) -> u64 {
        self.cost
    }
    fn advance_shallow(&mut self, t: u32) -> u32 {
        let mut up = NO_MORE_DOCS;
        for s in self.subs.iter_mut() {
            s.c.advance_shallow(t);
            up = up.min(s.c.impact_doc_upto(0));
        }
        up
    }
    fn max_score(&mut self, upto: u32) -> f32 {
        if !self.sim.is_monotonic() {
            return f32::INFINITY;
        }
        // Valid bound: every doc's combined frequency is at most the sum of the terms' best
        // frequencies, with a norm no smaller than the smallest one seen.
        let mut freq = 0f32;
        let mut norm = u8::MAX;
        for s in self.subs.iter_mut() {
            let levels = s.c.impact_levels();
            let level = (0..levels).find(|&l| s.c.impact_doc_upto(l) >= upto);
            let Some(level) = level else { return self.sim.score(f32::MAX, 1) };
            let mut mf = 0u32;
            for imp in s.c.impacts(level) {
                mf = mf.max(imp.freq);
                norm = norm.min(imp.norm);
            }
            freq += s.boost * mf as f32;
        }
        self.sim.score(freq, norm)
    }
    fn freq(&mut self) -> f32 {
        self.freq_now()
    }
}

impl Weight for SynWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(sim) = &self.sim else { return Ok(None) };
        let Some(tr) = seg.reader.terms(self.field) else { return Ok(None) };
        seg.ensure_postings(self.field)?;
        let mut subs = Vec::new();
        for (ts, b) in &self.states {
            if let Some(st) = ts.get(seg) {
                subs.push(SynSub { c: tr.postings(st, PostingsFlags::FREQS), boost: *b });
            }
        }
        if subs.is_empty() {
            return Ok(None);
        }
        let cost = subs.iter().map(|s| s.c.cost()).sum();
        Ok(Some(Box::new(SynScorer { subs, doc: u32::MAX, norms: Norms::of(seg, self.field), sim: sim.clone(), cost })))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        if let Some(mut s) = self.scorer(seg)?
            && s.advance(doc) == doc
        {
            let freq = s.freq();
            let fe = Explanation::leaf(freq, format!("termFreq={}", float_to_string(freq)));
            let norm = Norms::of(seg, self.field).get(doc);
            let se = self.sim.as_ref().unwrap().explain(fe, norm);
            return Ok(Explanation::matched(
                se.value(),
                format!("weight({} in {doc}) [{SIMILARITY_NAME}], result of:", self.desc),
                vec![se],
            ));
        }
        Ok(Explanation::no_match("no matching term", vec![]))
    }
}

impl Query for SynonymQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        if !mode.needs_scores() {
            let mut b = BooleanQuery::new();
            for (t, _) in &self.terms {
                b = b.add(Occur::Should, Arc::new(TermQuery::new(self.field, t.clone())));
            }
            let q: QueryRef = Arc::new(b);
            return s.rewrite(&q)?.weight(s, ScoreMode::CompleteNoScores, boost);
        }
        let mut df = 0u64;
        let mut ttf = 0u64;
        let mut states = Vec::new();
        for (t, b) in &self.terms {
            let ts = TermStates::build(s, self.field, t);
            if ts.doc_freq > 0 {
                df = df.max(ts.doc_freq);
                ttf += ts.total_term_freq;
            }
            states.push((ts, *b));
        }
        let sim = if df > 0 {
            scoring_stats(s, self.field, mode).map(|c| {
                Arc::new(s.schema().similarity(self.field).scorer(boost, &c, &[TermStats { doc_freq: df, total_term_freq: ttf }]))
            })
        } else {
            None
        };
        Ok(Box::new(SynWeight { field: self.field, states, sim, desc: self.lucene_string(s.schema()) }))
    }

    fn rewrite(&self, _s: &Searcher) -> Result<Option<QueryRef>> {
        match self.terms.len() {
            0 => Ok(Some(Arc::new(BooleanQuery::new()))),
            1 => {
                let (t, b) = &self.terms[0];
                let tq: QueryRef = Arc::new(TermQuery::new(self.field, t.clone()));
                Ok(Some(if *b == 1.0 { tq } else { Arc::new(crate::query::basic::BoostQuery::new(tq, *b)) }))
            }
            _ => Ok(None),
        }
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let f = schema.name(self.field);
        let mut b = String::from("Synonym(");
        for (i, (t, boost)) in self.terms.iter().enumerate() {
            if i != 0 {
                b.push(' ');
            }
            b.push_str(&format!("{f}:{}", term_text(t)));
            if *boost != 1.0 {
                b.push('^');
                b.push_str(&float_to_string(*boost));
            }
        }
        b.push(')');
        b
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        for (t, _) in &self.terms {
            v.term(self.field, t);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
