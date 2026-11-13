//! `CombinedFieldQuery` (Lucene; Elasticsearch's `combined_fields` query): BM25F, one term
//! scored across several fields as if they were a single field.
//!
//! Each field has a weight `>= 1`. A doc's frequency is `Σ weight·freq` over the fields
//! holding the term, and its length `Σ weight·length` over the fields it has (decoded from
//! the norms, re-encoded to a norm byte). Scoring uses the index's default similarity with
//! pseudo statistics: the largest doc frequency and the weighted sum of total term
//! frequencies for the term; the largest doc count, max doc and sum of doc frequencies, and
//! the weighted sum of total term frequencies for the field. Without scores the query is a
//! disjunction of term queries. Lucene gives the scorer no block-max bound, so neither do we:
//! top-k searches walk every match.

use std::any::Any;
use std::sync::Arc;

use search_codec::norms::{byte4_to_int, int_to_byte4};
use search_codec::{NO_MORE_DOCS, PostingsCursor, PostingsFlags};

use crate::error::{Error, Result};
use crate::query::boolean::BooleanQuery;
use crate::query::term::{Norms, TermQuery, TermStates, term_text};
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::similarity::{CollectionStats, SimScorer, TermStats};
use crate::util::java::float_to_string;

/// One term over several weighted fields.
#[derive(Clone, Debug, PartialEq)]
pub struct CombinedFieldQuery {
    /// `(field, weight)`, one entry per field.
    pub fields: Vec<(u32, f32)>,
    pub term: Vec<u8>,
}

impl CombinedFieldQuery {
    /// Lucene's builder: weights must be at least 1; a field given twice keeps its last weight.
    pub fn new(fields: &[(u32, f32)], term: impl Into<Vec<u8>>) -> Result<CombinedFieldQuery> {
        let mut out: Vec<(u32, f32)> = Vec::with_capacity(fields.len());
        for &(f, w) in fields {
            if w.is_nan() || w < 1.0 {
                return Err(Error::illegal("weight must be greater or equal to 1"));
            }
            match out.iter_mut().find(|(g, _)| *g == f) {
                Some(e) => e.1 = w,
                None => out.push((f, w)),
            }
        }
        Ok(CombinedFieldQuery { fields: out, term: term.into() })
    }

    /// The fields in Lucene's order (by name: a `TreeMap`).
    fn sorted(&self, schema: &Schema) -> Vec<(u32, f32, String)> {
        let mut v: Vec<(u32, f32, String)> = self.fields.iter().map(|&(f, w)| (f, w, schema.name(f))).collect();
        v.sort_by(|a, b| a.2.encode_utf16().cmp(b.2.encode_utf16()));
        v
    }

    fn disjunction(&self) -> BooleanQuery {
        self.fields.iter().fold(BooleanQuery::new(), |b, &(f, _)| b.should(Arc::new(TermQuery::new(f, self.term.clone())) as QueryRef))
    }
}

/// Lucene's `validateConsistentNorms`: every field present in the index has norms, or none has.
fn check_norms(s: &Searcher, fields: &[(u32, f32)]) -> Result<()> {
    let (mut all, mut none) = (true, true);
    for seg in s.segments() {
        for &(f, _) in fields {
            if seg.reader.terms(f).is_some() {
                let has = seg.reader.norms(f).is_some();
                all &= has;
                none &= !has;
            }
        }
    }
    if !all && !none {
        return Err(Error::illegal(
            "CombinedFieldQuery requires norms to be consistent across fields: some fields cannot  have norms enabled, while others have norms disabled",
        ));
    }
    Ok(())
}

struct CombinedWeight {
    fields: Vec<(u32, f32)>,
    states: Vec<TermStates>,
    sim: Option<Arc<SimScorer>>,
    desc: String,
    mode: ScoreMode,
}

impl CombinedWeight {
    fn ensure(&self, seg: &SegmentContext) -> Result<()> {
        for &(f, _) in &self.fields {
            seg.ensure_postings(f)?;
        }
        Ok(())
    }

    fn scorer_impl(&self, seg: &SegmentContext) -> Option<CombinedScorer> {
        let sim = self.sim.as_ref()?;
        let mut subs = Vec::new();
        for (&(f, w), ts) in self.fields.iter().zip(&self.states) {
            if let (Some(st), Some(tr)) = (ts.get(seg), seg.reader.terms(f)) {
                subs.push(Sub { c: tr.postings(st, PostingsFlags::FREQS), w });
            }
        }
        if subs.is_empty() {
            return None;
        }
        let norms = if self.mode.needs_scores() {
            self.fields.iter().map(|&(f, w)| (Norms::of(seg, f), w)).filter(|(n, _)| n.has()).collect()
        } else {
            Vec::new()
        };
        Some(CombinedScorer { subs, norms, sim: sim.clone(), doc: u32::MAX, max: sim.score(f32::INFINITY, 1) })
    }
}

struct Sub {
    c: PostingsCursor,
    w: f32,
}

/// The disjunction of a term's postings in each field, scored as one field.
struct CombinedScorer {
    subs: Vec<Sub>,
    norms: Vec<(Norms, f32)>,
    sim: Arc<SimScorer>,
    doc: u32,
    max: f32,
}

impl CombinedScorer {
    /// `Σ weight·freq` over the fields on the current doc (Lucene's float arithmetic, int max on
    /// overflow).
    fn freq_now(&mut self) -> f32 {
        let d = self.doc;
        let mut freq = 0f32;
        for s in self.subs.iter_mut() {
            if s.c.doc() == d {
                freq += s.c.freq() as f32 * s.w;
                if freq < 0.0 {
                    return i32::MAX as f32;
                }
            }
        }
        freq
    }

    /// Lucene's `MultiFieldNormValues`: the weighted sum of the fields' decoded lengths,
    /// rounded and encoded; 1 without norms.
    fn norm_now(&self) -> u8 {
        if self.norms.is_empty() {
            return 1;
        }
        let mut len = 0f32;
        for (n, w) in &self.norms {
            let b = match n {
                Norms::Sparse(s) => s.get_or(self.doc, 0),
                n => n.get(self.doc),
            };
            len += w * byte4_to_int(b) as f32;
        }
        // Java `Math.round(float)`: floor(x + 0.5), saturating.
        int_to_byte4((len + 0.5).floor().clamp(0.0, i32::MAX as f32) as u32)
    }

    fn position(&mut self) -> u32 {
        self.doc = self.subs.iter().map(|s| s.c.doc()).min().unwrap_or(NO_MORE_DOCS);
        self.doc
    }
}

impl Scorable for CombinedScorer {
    fn score(&mut self) -> f32 {
        let f = self.freq_now();
        let n = self.norm_now();
        self.sim.score(f, n)
    }
}

impl Scorer for CombinedScorer {
    fn doc(&self) -> u32 {
        self.doc
    }

    fn next(&mut self) -> u32 {
        if self.doc == u32::MAX {
            for s in self.subs.iter_mut() {
                s.c.next();
            }
            return self.position();
        }
        let d = self.doc;
        for s in self.subs.iter_mut() {
            if s.c.doc() == d {
                s.c.next();
            }
        }
        self.position()
    }

    fn advance(&mut self, t: u32) -> u32 {
        if self.doc != u32::MAX && t <= self.doc {
            return self.doc;
        }
        for s in self.subs.iter_mut() {
            if s.c.doc() == u32::MAX || s.c.doc() < t {
                s.c.advance(t);
            }
        }
        self.position()
    }

    fn cost(&self) -> u64 {
        self.subs.iter().map(|s| s.c.cost()).sum()
    }

    fn max_score(&mut self, _upto: u32) -> f32 {
        self.max
    }

    fn freq(&mut self) -> f32 {
        self.freq_now()
    }
}

impl Weight for CombinedWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        self.ensure(seg)?;
        Ok(self.scorer_impl(seg).map(|s| Box::new(s) as Box<dyn Scorer>))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        self.ensure(seg)?;
        if let Some(mut s) = self.scorer_impl(seg)
            && s.advance(doc) == doc
        {
            let freq = s.freq_now();
            let fe = Explanation::leaf(freq, format!("termFreq={}", float_to_string(freq)));
            let se = s.sim.explain(fe, s.norm_now());
            return Ok(Explanation::matched(se.value(), format!("weight({} in {doc}), result of:", self.desc), vec![se]));
        }
        Ok(Explanation::no_match("no matching term", vec![]))
    }
}

impl Query for CombinedFieldQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let max = s.limits().max_clause_count;
        if self.fields.len() > max {
            return Err(Error::too_many_clauses(max));
        }
        check_norms(s, &self.fields)?;
        if !mode.needs_scores() {
            return self.disjunction().weight(s, mode, boost);
        }
        let mut doc_freq = 0u64;
        let mut ttf = 0i64;
        let mut states = Vec::with_capacity(self.fields.len());
        for &(f, w) in &self.fields {
            let ts = TermStates::build(s, f, &self.term);
            if ts.doc_freq > 0 {
                doc_freq = doc_freq.max(ts.doc_freq);
                // Java `long += double`: the sum in double, truncated.
                ttf = (ttf as f64 + w as f64 * ts.total_term_freq as f64) as i64;
            }
            states.push(ts);
        }
        let sim = if doc_freq > 0 {
            let mut coll = CollectionStats { max_doc: 0, doc_count: 0, sum_total_term_freq: 0, sum_doc_freq: 0 };
            let mut sttf = 0i64;
            for &(f, w) in &self.fields {
                if let Some(c) = s.collection_stats(f) {
                    coll.max_doc = coll.max_doc.max(c.max_doc);
                    coll.doc_count = coll.doc_count.max(c.doc_count);
                    coll.sum_doc_freq = coll.sum_doc_freq.max(c.sum_doc_freq);
                    sttf = (sttf as f64 + w as f64 * c.sum_total_term_freq as f64) as i64;
                }
            }
            coll.sum_total_term_freq = sttf.max(0) as u64;
            let ts = TermStats { doc_freq, total_term_freq: ttf.max(1) as u64 };
            // "pseudo_field" has no mapping: the index's default similarity scores it.
            Some(Arc::new(s.schema().similarity.scorer(boost, &coll, &[ts])))
        } else {
            None
        };
        // Sorted like Lucene's field map, so per-field iteration order is stable.
        let fields: Vec<(u32, f32)> = self.sorted(s.schema()).into_iter().map(|(f, w, _)| (f, w)).collect();
        let states = fields.iter().map(|&(f, _)| states[self.fields.iter().position(|&(g, _)| g == f).unwrap()].clone()).collect();
        Ok(Box::new(CombinedWeight { fields, states, sim, desc: self.lucene_string(s.schema()), mode }))
    }

    fn rewrite(&self, _s: &Searcher) -> Result<Option<QueryRef>> {
        if self.fields.is_empty() {
            return Ok(Some(Arc::new(BooleanQuery::new())));
        }
        Ok(None)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let fields: Vec<String> = self
            .sorted(schema)
            .into_iter()
            .map(|(_, w, n)| if w != 1.0 { format!("{n}^{}", float_to_string(w)) } else { n })
            .collect();
        format!("CombinedFieldQuery(({})({}))", fields.join(" "), term_text(&self.term))
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        for &(f, _) in &self.fields {
            v.term(f, &self.term);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
