//! `terms_set` (Lucene's `CoveringQuery`): docs matching at least a per-document number of
//! the clauses, read from a numeric doc-values field, a script, or a constant. Scores sum the
//! matching clauses. A constant minimum rewrites to a boolean disjunction with
//! `minimum_should_match`.

use std::any::Any;
use std::sync::Arc;

use search_codec::{DocValuesReader, NO_MORE_DOCS};

use crate::error::Result;
use crate::query::basic::MatchNoneQuery;
use crate::query::boolean::BooleanQuery;
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};

/// A per-segment source of the required match count.
pub trait LeafMinMatch: Send {
    /// The doc's value (`None`: the doc can't match).
    fn value(&mut self, doc: u32) -> Option<i64>;
}

/// The script form of the minimum (`minimum_should_match_script`), implemented by the caller.
pub trait MinMatchScript: Send + Sync + std::fmt::Debug {
    fn leaf(&self, seg: &SegmentContext) -> Result<Box<dyn LeafMinMatch>>;
    /// Lucene-style description (`toString`).
    fn describe(&self) -> String;
}

/// Where the required match count comes from.
#[derive(Clone, Debug)]
pub enum MinMatch {
    Constant(i64),
    /// The first value of a numeric / sorted-numeric doc-values field.
    Field { field: u32, name: String },
    Script(Arc<dyn MinMatchScript>),
}

impl MinMatch {
    fn describe(&self) -> String {
        match self {
            MinMatch::Constant(c) => format!("constant({c})"),
            MinMatch::Field { name, .. } => format!("long({name})"),
            MinMatch::Script(s) => s.describe(),
        }
    }

    fn leaf(&self, seg: &SegmentContext) -> Result<Box<dyn LeafMinMatch>> {
        Ok(match self {
            MinMatch::Constant(c) => Box::new(ConstMin(*c)),
            MinMatch::Field { field, .. } => Box::new(FieldMin(seg.reader.doc_values(*field).cloned())),
            MinMatch::Script(s) => s.leaf(seg)?,
        })
    }
}

struct ConstMin(i64);

impl LeafMinMatch for ConstMin {
    fn value(&mut self, _doc: u32) -> Option<i64> {
        Some(self.0)
    }
}

struct FieldMin(Option<DocValuesReader>);

impl LeafMinMatch for FieldMin {
    fn value(&mut self, doc: u32) -> Option<i64> {
        match self.0.as_ref()? {
            DocValuesReader::Numeric(v) => v.get(doc),
            DocValuesReader::SortedNumeric(v) => v.values(doc).next(),
            _ => None,
        }
    }
}

/// The query.
#[derive(Clone, Debug)]
pub struct CoveringQuery {
    pub queries: Vec<QueryRef>,
    pub min: MinMatch,
}

impl Query for CoveringQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        if let Some(r) = self.rewrite(s)? {
            return r.weight(s, mode, boost);
        }
        let mut weights = Vec::with_capacity(self.queries.len());
        for q in &self.queries {
            weights.push(s.weight_of(&s.rewrite(q)?, mode, boost)?);
        }
        Ok(Box::new(CoveringWeight { weights, min: self.min.clone(), needs_scores: mode.needs_scores() }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        if let MinMatch::Constant(c) = self.min {
            if c > self.queries.len() as i64 {
                return Ok(Some(Arc::new(MatchNoneQuery::new("More clauses are required to match than the number of clauses"))));
            }
            let mut b = BooleanQuery::new().with_msm(c.max(1) as usize);
            for q in &self.queries {
                b = b.should(s.rewrite(q)?);
            }
            return Ok(Some(Arc::new(b)));
        }
        Ok(None)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let mut qs: Vec<String> = self.queries.iter().map(|q| q.lucene_string(schema)).collect();
        qs.sort();
        format!("CoveringQuery(queries=[{}], minimumNumberMatch={})", qs.join(", "), self.min.describe())
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        for q in &self.queries {
            q.visit(v);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct CoveringWeight {
    weights: Vec<Box<dyn Weight>>,
    min: MinMatch,
    needs_scores: bool,
}

impl Weight for CoveringWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let mut subs = Vec::with_capacity(self.weights.len());
        for w in &self.weights {
            if let Some(s) = w.scorer(seg)? {
                subs.push(s);
            }
        }
        if subs.is_empty() {
            return Ok(None);
        }
        let docs = vec![u32::MAX; subs.len()];
        Ok(Some(Box::new(CoveringScorer {
            subs,
            docs,
            min: self.min.leaf(seg)?,
            doc: u32::MAX,
            on_doc: Vec::new(),
            needs_scores: self.needs_scores,
            checked: (u32::MAX, false),
        })))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let Some(min) = self.min.leaf(seg)?.value(doc) else {
            return Ok(Explanation::no_match("minimumNumberMatch has no value on the current document", vec![]));
        };
        let min = min.max(1);
        let mut freq = 0i64;
        let mut score = 0f64;
        let mut subs = Vec::with_capacity(self.weights.len());
        for w in &self.weights {
            let e = w.explain(seg, doc)?;
            if e.is_match() {
                freq += 1;
                score += e.value() as f64;
            }
            subs.push(e);
        }
        Ok(if freq >= min {
            Explanation::matched(score as f32, format!("{freq} matches for {min} required matches, sum of:"), subs)
        } else {
            Explanation::no_match(format!("{freq} matches for {min} required matches"), subs)
        })
    }
}

/// Approximation: docs of any clause; confirmation: enough clauses on the doc.
struct CoveringScorer {
    subs: Vec<Box<dyn Scorer>>,
    docs: Vec<u32>,
    min: Box<dyn LeafMinMatch>,
    doc: u32,
    on_doc: Vec<usize>,
    needs_scores: bool,
    checked: (u32, bool),
}

impl CoveringScorer {
    fn approx_to(&mut self, t: u32) -> u32 {
        let mut m = NO_MORE_DOCS;
        for i in 0..self.subs.len() {
            if self.docs[i] == u32::MAX || self.docs[i] < t {
                self.docs[i] = self.subs[i].advance(t);
            }
            m = m.min(self.docs[i]);
        }
        self.doc = m;
        self.on_doc.clear();
        for i in 0..self.subs.len() {
            if self.docs[i] == m {
                self.on_doc.push(i);
            }
        }
        m
    }
}

impl Scorable for CoveringScorer {
    fn score(&mut self) -> f32 {
        if !self.needs_scores {
            return 0.0;
        }
        let mut s = 0f64;
        for k in 0..self.on_doc.len() {
            let i = self.on_doc[k];
            s += self.subs[i].score() as f64;
        }
        s as f32
    }
}

impl Scorer for CoveringScorer {
    fn doc(&self) -> u32 {
        self.doc
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
        self.subs.iter().map(|s| s.cost()).fold(0u64, |a, c| a.saturating_add(c))
    }
    fn has_two_phase(&self) -> bool {
        true
    }
    fn approx_next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc.saturating_add(1) };
        self.approx_to(t)
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.doc != u32::MAX && t <= self.doc {
            return self.doc;
        }
        self.approx_to(t)
    }
    fn matches(&mut self) -> bool {
        if self.checked.0 != self.doc {
            let ok = match self.min.value(self.doc) {
                Some(m) => self.on_doc.len() as i64 >= m.max(1),
                None => false,
            };
            self.checked = (self.doc, ok);
        }
        self.checked.1
    }
    fn match_cost(&self) -> f32 {
        self.subs.len() as f32
    }
}
