//! Multi-term queries (prefix, wildcard, regexp, fuzzy, case-insensitive term) on fields that
//! have doc values but no index (`index: false`): Lucene's `MultiTermQuery.DOC_VALUES_REWRITE`,
//! which Elasticsearch picks for doc-values-only `keyword`, `text` and `match_only_text`
//! fields.
//!
//! Per segment, the doc values term dictionary is matched once against the query (an automaton
//! intersection skips dead prefixes for automaton matchers; other matchers test each term), and
//! the matching ordinals go into a bit set sized by the dictionary. Docs are then a two-phase
//! iteration: the field's docs as the approximation, one ordinal lookup per candidate as the
//! confirmation, so conjunctions let cheaper clauses lead. Scores are constant, as with every
//! doc-values rewrite.

use std::any::Any;
use std::sync::Arc;

use search_codec::{DocValuesReader, NO_MORE_DOCS, TermDict};
use search_index::query::basic::explain_const;
use search_index::query::{MultiTermQuery, TermsMatcher};
use search_index::search::{Explanation, Query, QueryRef, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use search_index::{Result, Schema};

/// A [`MultiTermQuery`] answered from sorted (set) doc values instead of postings.
#[derive(Clone, Debug)]
pub struct DocValuesMultiTermQuery {
    pub inner: MultiTermQuery,
}

impl DocValuesMultiTermQuery {
    pub fn query(inner: MultiTermQuery) -> QueryRef {
        Arc::new(DocValuesMultiTermQuery { inner })
    }
}

/// Bit set of matching ordinals of one segment's dictionary; `None` when nothing matches.
fn matching_ords(q: &MultiTermQuery, dict: &TermDict) -> Option<Arc<Vec<u64>>> {
    let n = dict.len();
    if n == 0 {
        return None;
    }
    let mut bits = vec![0u64; n.div_ceil(64) as usize];
    let mut any = false;
    let mut set = |o: u64| {
        bits[(o >> 6) as usize] |= 1 << (o & 63);
        any = true;
    };
    match &q.matcher {
        TermsMatcher::Automaton { aut, .. } => {
            let mut it = dict.intersect(aut.as_ref());
            while let Some(o) = it.next() {
                set(o);
            }
        }
        TermsMatcher::Fuzzy { aut, .. } => {
            let mut it = dict.intersect(aut.as_ref());
            while let Some(o) = it.next() {
                set(o);
            }
        }
        TermsMatcher::Prefix(p) => {
            let start = dict.lower_bound(p);
            let mut it = dict.iter_from(start);
            while let Some(t) = it.next() {
                if !t.starts_with(p) {
                    break;
                }
                set(it.ord());
            }
        }
        TermsMatcher::Set(terms) => {
            for t in terms.iter() {
                if let Ok(o) = dict.seek(t) {
                    set(o);
                }
            }
        }
        TermsMatcher::Range { .. } => {
            let mut it = dict.iter_from(0);
            while let Some(t) = it.next() {
                if q.matches_term(t) {
                    set(it.ord());
                }
            }
        }
    }
    any.then(|| Arc::new(bits))
}

#[inline]
fn has(bits: &[u64], o: u64) -> bool {
    bits[(o >> 6) as usize] & (1 << (o & 63)) != 0
}

enum Values {
    Sorted(search_codec::SortedDocValues),
    SortedSet(search_codec::SortedSetDocValues),
}

struct DvMtScorer {
    docs: search_codec::DocSetCursor,
    values: Values,
    bits: Arc<Vec<u64>>,
    score: f32,
    cost: u64,
    doc: u32,
}

impl Scorable for DvMtScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl Scorer for DvMtScorer {
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
        self.cost
    }
    fn has_two_phase(&self) -> bool {
        true
    }
    fn approx_next(&mut self) -> u32 {
        self.doc = self.docs.next();
        self.doc
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        self.doc = self.docs.advance(t);
        self.doc
    }
    fn matches(&mut self) -> bool {
        // The cursor's index addresses the value directly: no second doc lookup.
        let i = self.docs.index();
        match &self.values {
            Values::Sorted(v) => has(&self.bits, v.ord_at(i)),
            Values::SortedSet(v) => v.ords_at(i).any(|o| has(&self.bits, o)),
        }
    }
    fn match_cost(&self) -> f32 {
        3.0
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        self.score
    }
}

struct DvMtWeight {
    q: MultiTermQuery,
    score: f32,
    desc: String,
}

impl Weight for DvMtWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(dv) = seg.reader.doc_values(self.q.field) else { return Ok(None) };
        let (docs, values, bits) = match dv.clone() {
            DocValuesReader::Sorted(v) => {
                let Some(bits) = matching_ords(&self.q, v.dict()) else { return Ok(None) };
                (v.docs().clone(), Values::Sorted(v), bits)
            }
            DocValuesReader::SortedSet(v) => {
                let Some(bits) = matching_ords(&self.q, v.dict()) else { return Ok(None) };
                (v.docs().clone(), Values::SortedSet(v), bits)
            }
            _ => return Ok(None),
        };
        Ok(Some(Box::new(DvMtScorer { cost: docs.len() as u64, docs: docs.cursor(), values, bits, score: self.score, doc: u32::MAX })))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for DocValuesMultiTermQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(DvMtWeight { q: self.inner.clone(), score: boost, desc: self.lucene_string(s.schema()) }))
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        self.inner.lucene_string(schema)
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        h.write(b"dv_multi_term");
        self.inner.cache_identity(h)
    }
    fn profile_name(&self) -> String {
        self.inner.profile_name()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
