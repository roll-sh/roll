//! `SourceConfirmedQuery`: Elasticsearch's `SourceConfirmedTextQuery`, how `match_only_text`
//! fields (indexed without frequencies, positions or norms, the default `message` of log data
//! streams) run phrase, multi-phrase, phrase-prefix, span and interval queries.
//!
//! The main index only narrows candidates: the approximation is Elasticsearch's (`approximate`:
//! the phrase's terms as a conjunction, a disjunction per multi-phrase position, prefix
//! queries for a lone phrase prefix, everything otherwise). Each candidate is confirmed the way
//! Elasticsearch's `MemoryIndex` does it: the caller re-analyzes the doc's values (from
//! `_source` or stored fields) through [`SourceValues`], they are analyzed with the field's
//! index analyzer (and its position increment gap between values) into a one-document
//! in-memory segment with positions and offsets, and the wrapped query runs there; its
//! frequency there is the phrase frequency. Scores are the field similarity's at that frequency with norm 1 (the field
//! has no norms) — Elasticsearch wraps these queries in `ConstantScoreQuery` anyway. A
//! candidate's segment is built once even when the scorer is asked twice.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use search_analysis::Analyzer;
use search_codec::{IndexOptions, NO_MORE_DOCS, SegmentReader, Slice, SpillOptions, WriteOptions, write_segment_with};

use crate::buffer::source::BufferSource;
use crate::buffer::{Buffer, DocMeta};
use crate::doc::{IndexableDoc, IndexableField};
use crate::error::Result;
use crate::query::basic::{BoostQuery, ConstantScoreQuery, MatchAllQuery, MatchNoneQuery};
use crate::query::boolean::BooleanQuery;
use crate::query::intervals::{BI, II, IntervalQuery, IntervalsSource};
use crate::query::multiterm::MultiTermQuery;
use crate::query::phrase::{MultiPhrasePrefixQuery, MultiPhraseQuery, PhraseQuery};
use crate::query::term::TermQuery;
use crate::schema::{FieldSchema, Schema};
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, SegmentCore, Searcher, SegmentContext, Weight};
use crate::similarity::{SimScorer, TermStats};
use crate::util::java::float_to_string;

/// The original values of the confirmed field (Elasticsearch's value fetcher: `_source`, or
/// the stored field of synthetic source).
pub trait SourceValues: Send + Sync {
    fn leaf(&self, seg: &SegmentContext) -> Result<Box<dyn LeafSourceValues>>;
}

/// [`SourceValues`] bound to a segment.
pub trait LeafSourceValues: Send {
    /// The field's values in `doc` (segment-local); empty when it has none.
    fn values(&mut self, doc: u32) -> Result<Vec<String>>;
}

/// `query` on a field indexed without positions, confirmed per candidate against the doc's
/// values analyzed with `analyzer` (the field's index analyzer).
#[derive(Clone)]
pub struct SourceConfirmedQuery {
    pub query: QueryRef,
    pub field: u32,
    pub analyzer: Analyzer,
    pub source: Arc<dyn SourceValues>,
}

impl std::fmt::Debug for SourceConfirmedQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SourceConfirmedQuery({:?})", self.query)
    }
}

impl SourceConfirmedQuery {
    pub fn new(query: QueryRef, field: u32, analyzer: Analyzer, source: Arc<dyn SourceValues>) -> SourceConfirmedQuery {
        SourceConfirmedQuery { query, field, analyzer, source }
    }

    fn wrap(&self, query: QueryRef) -> QueryRef {
        Arc::new(SourceConfirmedQuery { query, ..self.clone() })
    }
}

/// Elasticsearch's `SourceConfirmedTextQuery.approximate`: a superset of the query's matches
/// from the main index.
pub fn approximate(q: &QueryRef) -> QueryRef {
    let a = q.as_any();
    if a.is::<TermQuery>() {
        return q.clone();
    }
    if let Some(p) = a.downcast_ref::<PhraseQuery>() {
        let mut b = BooleanQuery::new();
        for t in &p.terms {
            b = b.filter(Arc::new(TermQuery::new(p.field, t.clone())));
        }
        return Arc::new(b);
    }
    if let Some(p) = a.downcast_ref::<MultiPhraseQuery>() {
        let mut b = BooleanQuery::new();
        for ts in &p.terms {
            let mut any = BooleanQuery::new();
            for t in ts {
                any = any.should(Arc::new(TermQuery::new(p.field, t.clone())));
            }
            b = b.filter(Arc::new(any));
        }
        return Arc::new(b);
    }
    if let Some(p) = a.downcast_ref::<MultiPhrasePrefixQuery>() {
        let n = p.terms.len();
        if n == 0 {
            return Arc::new(MatchNoneQuery::new(""));
        }
        let mut b = BooleanQuery::new();
        if n == 1 {
            for t in &p.terms[0] {
                b = b.filter(Arc::new(MultiTermQuery::prefix(p.field, t.clone())));
            }
            return Arc::new(b);
        }
        for ts in &p.terms[..n - 1] {
            let mut any = BooleanQuery::new();
            for t in ts {
                any = any.should(Arc::new(TermQuery::new(p.field, t.clone())));
            }
            b = b.filter(Arc::new(any));
        }
        return Arc::new(b);
    }
    if let Some(iq) = a.downcast_ref::<IntervalQuery>() {
        return approximate_intervals(iq.field, &iq.source).unwrap_or_else(|| Arc::new(MatchAllQuery));
    }
    Arc::new(MatchAllQuery)
}

/// A superset of the docs an intervals source matches in `field` (`None`: any doc), from
/// the terms it needs: conjunctions for sources needing all their parts, disjunctions for
/// alternatives, the first part of exclusions. (Elasticsearch approximates term, prefix and
/// fuzzy sources this way and scans for the others.)
pub(crate) fn approximate_intervals(field: u32, src: &IntervalsSource) -> Option<QueryRef> {
    use IntervalsSource as S;
    let all = |v: &[&IntervalsSource]| -> Option<QueryRef> {
        let parts: Vec<QueryRef> = v.iter().filter_map(|s| approximate_intervals(field, s)).collect();
        match parts.len() {
            0 => None,
            1 => parts.into_iter().next(),
            _ => Some(Arc::new(parts.into_iter().fold(BooleanQuery::new(), |b, q| b.filter(q)))),
        }
    };
    let any = |v: &[&IntervalsSource]| -> Option<QueryRef> {
        let parts: Option<Vec<QueryRef>> = v.iter().map(|s| approximate_intervals(field, s)).collect();
        let parts = parts?;
        if parts.len() == 1 {
            return parts.into_iter().next();
        }
        Some(Arc::new(parts.into_iter().fold(BooleanQuery::new(), |b, q| b.should(q))))
    };
    match src {
        S::Term(t) => Some(Arc::new(TermQuery::new(field, t.clone()))),
        S::MultiTerm { query, .. } => Some(Arc::new(query.as_ref().clone().with_rewrite(crate::query::RewriteMethod::ConstantScoreBlended))),
        S::Block(v) | S::Ordered(v) | S::Unordered(v) => all(&v.iter().collect::<Vec<_>>()),
        S::Or(v, _) | S::AtLeast(v, _) => any(&v.iter().collect::<Vec<_>>()),
        S::Repeating(x, ..) | S::MaxGaps(x, _) | S::MaxWidth(x, _) | S::Extend(x, ..) | S::Offset(x, _) => approximate_intervals(field, x),
        S::Containing(a, b) | S::ContainedBy(a, b) | S::Overlapping(a, b) => all(&[a, b]),
        S::NotContaining(a, _) | S::NotContainedBy(a, _) | S::NonOverlapping(a, _) => approximate_intervals(field, a),
        S::NoMatch(r) => Some(Arc::new(MatchNoneQuery::new(r.clone()))),
        S::Source(spec, x) => approximate_intervals(spec.field, x),
        S::Field(..) | S::Error(_) => None,
    }
}

/// Terms the query mentions, in order, without repeats (Lucene's `termCollector`).
#[derive(Default)]
struct Terms(Vec<(u32, Vec<u8>)>);

impl QueryVisitor for Terms {
    fn term(&mut self, field: u32, term: &[u8]) {
        if !self.0.iter().any(|(f, t)| *f == field && t == term) {
            self.0.push((field, term.to_vec()));
        }
    }
}

impl Query for SourceConfirmedQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        if !mode.needs_scores() && self.query.as_any().is::<TermQuery>() {
            // A term needs no positions.
            return s.weight_of(&self.query, mode, boost);
        }
        let mut terms = Terms::default();
        self.query.visit(&mut terms);
        let field = terms.0.first().map_or(self.field, |t| t.0);
        let mut mem = Schema::new();
        if let Some(f) = s.schema().field(field) {
            let mut fs = FieldSchema::new(field, f.name.clone()).indexed(IndexOptions::DocsAndFreqsAndPositionsAndOffsets, false);
            fs.similarity = f.similarity.clone();
            mem.add_field(fs)?;
        } else {
            mem.add_field(FieldSchema::new(field, s.schema().name(field)).indexed(IndexOptions::DocsAndFreqsAndPositionsAndOffsets, false))?;
        }
        // Intervals score with their own function of the interval frequency: no statistics,
        // and no terms needed (prefix and wildcard sources have none).
        let own = self.query.as_any().is::<IntervalQuery>();
        let coll = if mode.needs_scores() { s.collection_stats(field) } else { crate::query::term::scoring_stats(s, field, mode) };
        let (sim, approx) = match coll {
            _ if own => (None, Some(s.weight_of(&s.rewrite(&approximate(&self.query))?, ScoreMode::CompleteNoScores, 1.0)?)),
            None => (None, None),
            Some(coll) => {
                let stats: Vec<TermStats> = if mode.needs_scores() {
                    terms.0.iter().map(|(f, t)| s.term_stats(*f, t)).filter(|t| t.doc_freq > 0).collect()
                } else {
                    terms.0.iter().map(|_| TermStats { doc_freq: 1, total_term_freq: 1 }).collect()
                };
                if stats.is_empty() {
                    (None, None)
                } else {
                    let sim = s.schema().similarity(field).scorer(boost, &coll, &stats);
                    (Some(Arc::new(sim)), Some(s.weight_of(&s.rewrite(&approximate(&self.query))?, ScoreMode::CompleteNoScores, 1.0)?))
                }
            }
        };
        Ok(Box::new(SourceConfirmedWeight {
            fast: FastPhrase::of(&self.query).map(Arc::new),
            own: own.then_some(boost),
            q: self.clone(),
            mem: Arc::new(mem),
            sim,
            approx,
            desc: self.query.lucene_string(s.schema()),
            sim_name: crate::query::term::SIMILARITY_NAME,
        }))
    }

    /// Rewrites the wrapped query against the index (phrase prefixes expand there), and lifts
    /// constant-score and boost wrappers out, as Elasticsearch does.
    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        if let Some(iq) = self.query.as_any().downcast_ref::<IntervalQuery>() {
            // Elasticsearch confirms the field's own sources (`SourceIntervalsSource`); sources
            // of other fields (`use_field`) read the index.
            let spec = Arc::new(SourceSpec::new(self.field, s.schema().name(self.field), self.analyzer.clone(), self.source.clone()));
            return Ok(Some(Arc::new(IntervalQuery { source: confirm_sources(&iq.source, &spec), ..iq.clone() })));
        }
        let r = s.rewrite(&self.query)?;
        if !Arc::ptr_eq(&r, &self.query) {
            return Ok(Some(self.wrap(r)));
        }
        let a = self.query.as_any();
        if let Some(c) = a.downcast_ref::<ConstantScoreQuery>() {
            return Ok(Some(Arc::new(ConstantScoreQuery::new(self.wrap(c.query.clone())))));
        }
        if let Some(b) = a.downcast_ref::<BoostQuery>() {
            return Ok(Some(Arc::new(BoostQuery::new(self.wrap(b.query.clone()), b.boost))));
        }
        if a.is::<MatchNoneQuery>() {
            return Ok(Some(self.query.clone()));
        }
        Ok(None)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        self.query.lucene_string(schema)
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.query.visit(v);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Phrases and multi-phrases skip the in-memory segment: the doc's analyzed positions of the
/// phrase's terms feed the index's phrase matcher directly.
/// A phrase slot: its terms and its offset in the phrase.
type Slot = (Vec<Vec<u8>>, i64);

struct FastPhrase {
    slots: Vec<Slot>,
    slop: u32,
    /// Distinct terms; `slot_terms[k]` indexes them.
    terms: Vec<Vec<u8>>,
    slot_terms: Vec<Vec<usize>>,
}

impl FastPhrase {
    fn of(q: &QueryRef) -> Option<FastPhrase> {
        let a = q.as_any();
        let (slots, slop): (Vec<Slot>, u32) = match a.downcast_ref::<PhraseQuery>() {
            Some(p) => (p.terms.iter().zip(&p.positions).map(|(t, &o)| (vec![t.clone()], o as i64)).collect(), p.slop),
            None => {
                let p = a.downcast_ref::<MultiPhraseQuery>()?;
                (p.terms.iter().cloned().zip(p.positions.iter().map(|&o| o as i64)).collect(), p.slop)
            }
        };
        if slots.len() < 2 {
            return None;
        }
        let mut terms: Vec<Vec<u8>> = Vec::new();
        let slot_terms = slots
            .iter()
            .map(|(ts, _)| {
                ts.iter()
                    .map(|t| match terms.iter().position(|x| x == t) {
                        Some(i) => i,
                        None => {
                            terms.push(t.clone());
                            terms.len() - 1
                        }
                    })
                    .collect()
            })
            .collect();
        Some(FastPhrase { slots, slop, terms, slot_terms })
    }

    /// The phrase frequency of `values` analyzed with `analyzer`.
    fn freq(&self, analyzer: &Analyzer, values: &[String]) -> Result<f32> {
        let mut positions: Vec<Vec<u32>> = vec![Vec::new(); self.terms.len()];
        let refs: Vec<&str> = values.iter().map(String::as_str).collect();
        analyzer
            .try_analyze_values(&refs, &mut |t| {
                if let Some(i) = self.terms.iter().position(|x| x.as_slice() == t.term.as_bytes()) {
                    positions[i].push(t.position);
                }
            })
            .map_err(|e| crate::error::Error::illegal(e.to_string()))?;
        let slots = self
            .slots
            .iter()
            .zip(&self.slot_terms)
            .map(|((ts, off), ids)| {
                let mut p: Vec<u32> = ids.iter().flat_map(|&i| positions[i].iter().copied()).collect();
                p.sort_unstable();
                (ts.clone(), *off, p)
            })
            .collect();
        Ok(crate::query::phrase::memory_phrase_freq(slots, self.slop))
    }
}

struct SourceConfirmedWeight {
    fast: Option<Arc<FastPhrase>>,
    /// The wrapped query scores itself (intervals), with this boost.
    own: Option<f32>,
    q: SourceConfirmedQuery,
    mem: Arc<Schema>,
    sim: Option<Arc<SimScorer>>,
    approx: Option<Box<dyn Weight>>,
    desc: String,
    sim_name: &'static str,
}

/// The query in a one-doc segment of `values`.
struct Confirmed {
    /// 0: no match.
    freq: f32,
    /// The query's own score with `boost` (queries scoring themselves).
    score: f32,
    explanation: Option<Explanation>,
}

/// A one-doc segment (Lucene's `MemoryIndex`) of `values` analyzed into `field` (positions and
/// offsets).
fn memory_searcher(mem: &Arc<Schema>, field: u32, analyzer: &Analyzer, values: Vec<String>) -> Result<Searcher> {
    let mut buf = Buffer::new(0, mem.clone());
    let doc = IndexableDoc::new(b"0".to_vec()).field(IndexableField::Text { field, analyzer: analyzer.clone(), values });
    buf.add_block(&doc, DocMeta { seq_no: 0, version: 1, primary_term: 1 })?;
    let src = BufferSource { buf: &buf, id: [0; 16], attributes: BTreeMap::new() };
    let opts = WriteOptions { spill: SpillOptions { dir: None, memory_bytes: usize::MAX }, ..WriteOptions::default() };
    let mut out = Vec::with_capacity(1024);
    write_segment_with(&src, &mut out, &opts)?;
    let reader = SegmentReader::open(Slice::from(out))?;
    Ok(Searcher::new(vec![(Arc::new(SegmentCore::new(0, reader)), None)], mem.clone(), 0))
}

/// Runs `query` over a one-doc segment of `values`.
fn confirm(mem: &Arc<Schema>, query: &QueryRef, field: u32, analyzer: &Analyzer, values: Vec<String>, boost: f32, explain: bool) -> Result<Confirmed> {
    let none = Confirmed { freq: 0.0, score: 0.0, explanation: None };
    if values.is_empty() {
        return Ok(none);
    }
    let s = memory_searcher(mem, field, analyzer, values)?;
    let q = s.rewrite(query)?;
    let w = s.weight_of(&q, ScoreMode::Complete, boost)?;
    let seg = &s.segments()[0];
    let Some(mut sc) = w.scorer(seg)? else { return Ok(none) };
    if sc.advance(0) != 0 {
        return Ok(none);
    }
    let f = sc.freq();
    Ok(Confirmed {
        // A match always counts, whatever frequency the query reports.
        freq: if f > 0.0 { f } else { 1.0 },
        score: sc.score(),
        explanation: if explain { Some(w.explain(seg, 0)?) } else { None },
    })
}

struct SourceConfirmedScorer {
    fast: Option<Arc<FastPhrase>>,
    approx: Box<dyn Scorer>,
    values: Box<dyn LeafSourceValues>,
    q: QueryRef,
    field: u32,
    analyzer: Analyzer,
    mem: Arc<Schema>,
    sim: Option<Arc<SimScorer>>,
    own: Option<f32>,
    /// Doc whose frequency is in `freq` (and own score in `score`).
    at: u32,
    freq: f32,
    score: f32,
}

impl SourceConfirmedScorer {
    fn try_freq(&mut self) -> Result<f32> {
        let d = self.approx.doc();
        if self.at != d {
            self.freq = 0.0;
            self.at = d;
            let v = self.values.values(d)?;
            match &self.fast {
                Some(f) => self.freq = f.freq(&self.analyzer, &v)?,
                None => {
                    let c = confirm(&self.mem, &self.q, self.field, &self.analyzer, v, self.own.unwrap_or(1.0), false)?;
                    self.freq = c.freq;
                    self.score = c.score;
                }
            }
        }
        Ok(self.freq)
    }

    /// The frequency, a failure (unreadable source, analysis error) surfacing after the search.
    fn freq_now(&mut self) -> f32 {
        match self.try_freq() {
            Ok(f) => f,
            Err(e) => {
                crate::search::defer_error(e);
                0.0
            }
        }
    }
}

impl Scorable for SourceConfirmedScorer {
    fn score(&mut self) -> f32 {
        let f = self.freq_now();
        match (&self.sim, self.own) {
            (_, Some(_)) => self.score,
            (Some(sim), None) => sim.score(f, 1),
            (None, None) => 0.0,
        }
    }
}

impl Scorer for SourceConfirmedScorer {
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
        self.freq_now() > 0.0
    }
    fn match_cost(&self) -> f32 {
        10_000.0
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        match (&self.sim, self.own) {
            // The saturation function stays below its weight.
            (_, Some(b)) => b,
            (Some(sim), None) => sim.score(f32::MAX, 1),
            (None, None) => 0.0,
        }
    }
    fn freq(&mut self) -> f32 {
        self.freq_now()
    }
}

impl SourceConfirmedWeight {
    fn scorer_impl(&self, seg: &SegmentContext) -> Result<Option<SourceConfirmedScorer>> {
        let Some(approx) = &self.approx else { return Ok(None) };
        if self.sim.is_none() && self.own.is_none() {
            return Ok(None);
        }
        let Some(a) = approx.scorer(seg)? else { return Ok(None) };
        // Values usually come from stored `_source`.
        seg.ensure_stored()?;
        Ok(Some(SourceConfirmedScorer {
            fast: self.fast.clone(),
            approx: a,
            values: self.q.source.leaf(seg)?,
            q: self.q.query.clone(),
            field: self.q.field,
            analyzer: self.q.analyzer.clone(),
            mem: self.mem.clone(),
            sim: self.sim.clone(),
            own: self.own,
            at: u32::MAX,
            freq: 0.0,
            score: 0.0,
        }))
    }
}

impl Weight for SourceConfirmedWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.scorer_impl(seg)?.map(|s| Box::new(s) as Box<dyn Scorer>))
    }

    /// Elasticsearch's text: `weight(<query> in <doc>) [<similarity>], result of:` over the
    /// similarity's explanation of `phraseFreq=<freq>` with norm 1.
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let no = || Explanation::no_match("No matching phrase", vec![]);
        let Some(mut s) = self.scorer_impl(seg)? else { return Ok(no()) };
        if s.approx.advance(doc) != doc {
            return Ok(no());
        }
        if let Some(boost) = self.own {
            // The wrapped query explains itself over the doc's values.
            let v = s.values.values(doc)?;
            let c = confirm(&self.mem, &self.q.query, self.q.field, &self.q.analyzer, v, boost, true)?;
            return Ok(match c.explanation {
                Some(e) if c.freq > 0.0 => e,
                _ => Explanation::no_match("no matching intervals", vec![]),
            });
        }
        let f = s.try_freq()?;
        if f <= 0.0 {
            return Ok(no());
        }
        let freq = Explanation::matched(f, format!("phraseFreq={}", float_to_string(f)), vec![]);
        let Some(sim) = &s.sim else { return Ok(no()) };
        let inner = sim.explain(freq, 1);
        Ok(Explanation::matched(inner.value(), format!("weight({} in {doc}) [{}], result of:", self.desc, self.sim_name), vec![inner]))
    }
}

// ------------------------------------------------------------------ intervals

/// What an [`IntervalsSource::source_confirmed`] source re-analyzes: the field, its index
/// analyzer and its values. One spec can serve every source of the field in a query: the
/// candidate doc's in-memory segment is built once and shared.
pub struct SourceSpec {
    pub field: u32,
    pub analyzer: Analyzer,
    pub source: Arc<dyn SourceValues>,
    mem: Arc<Schema>,
    /// The last doc's in-memory segment: (segment, doc, searcher; `None` when the doc has no
    /// values).
    last: parking_lot::Mutex<Option<MemDoc>>,
}

/// A candidate doc's in-memory segment: (segment, doc, searcher; `None` without values).
type MemDoc = (Arc<SegmentCore>, u32, Option<Searcher>);

impl std::fmt::Debug for SourceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SourceSpec({})", self.field)
    }
}

impl SourceSpec {
    pub fn new(field: u32, field_name: impl Into<String>, analyzer: Analyzer, source: Arc<dyn SourceValues>) -> SourceSpec {
        let mut mem = Schema::new();
        // A fresh schema with one field cannot fail.
        let _ = mem.add_field(FieldSchema::new(field, field_name.into()).indexed(IndexOptions::DocsAndFreqsAndPositionsAndOffsets, false));
        SourceSpec { field, analyzer, source, mem: Arc::new(mem), last: parking_lot::Mutex::new(None) }
    }

    /// The doc's in-memory segment, `None` when it has no values.
    fn searcher(&self, seg: &SegmentContext, values: &mut dyn LeafSourceValues, doc: u32) -> Result<Option<Searcher>> {
        if let Some((core, d, s)) = &*self.last.lock()
            && *d == doc
            && Arc::ptr_eq(core, &seg.core)
        {
            return Ok(s.clone());
        }
        let v = values.values(doc)?;
        let s = if v.is_empty() { None } else { Some(memory_searcher(&self.mem, self.field, &self.analyzer, v)?) };
        *self.last.lock() = Some((seg.core.clone(), doc, s.clone()));
        Ok(s)
    }
}

/// Wraps the maximal subtrees of `src` that read the query's own field (no `use_field` inside)
/// in [`IntervalsSource::Source`].
fn confirm_sources(src: &IntervalsSource, spec: &Arc<SourceSpec>) -> IntervalsSource {
    use IntervalsSource as S;
    fn reads_other_field(s: &IntervalsSource) -> bool {
        match s {
            S::Field(..) | S::Source(..) => true,
            S::Block(v) | S::Ordered(v) | S::Unordered(v) | S::Or(v, _) | S::AtLeast(v, _) => v.iter().any(reads_other_field),
            S::Repeating(x, ..) | S::MaxGaps(x, _) | S::MaxWidth(x, _) | S::Extend(x, ..) | S::Offset(x, _) => reads_other_field(x),
            S::Containing(a, b)
            | S::ContainedBy(a, b)
            | S::NotContaining(a, b)
            | S::NotContainedBy(a, b)
            | S::NonOverlapping(a, b)
            | S::Overlapping(a, b) => reads_other_field(a) || reads_other_field(b),
            S::Term(_) | S::MultiTerm { .. } | S::NoMatch(_) | S::Error(_) => false,
        }
    }
    if matches!(src, S::NoMatch(_) | S::Error(_)) {
        return src.clone();
    }
    if !reads_other_field(src) {
        return S::Source(spec.clone(), Box::new(src.clone()));
    }
    let b = |x: &IntervalsSource| Box::new(confirm_sources(x, spec));
    let v = |xs: &[IntervalsSource]| xs.iter().map(|x| confirm_sources(x, spec)).collect::<Vec<_>>();
    match src {
        S::Field(..) | S::Source(..) => src.clone(),
        S::Block(xs) => S::Block(v(xs)),
        S::Ordered(xs) => S::Ordered(v(xs)),
        S::Unordered(xs) => S::Unordered(v(xs)),
        S::Or(xs, p) => S::Or(v(xs), *p),
        S::AtLeast(xs, n) => S::AtLeast(v(xs), *n),
        S::Repeating(x, n, name) => S::Repeating(b(x), *n, *name),
        S::MaxGaps(x, n) => S::MaxGaps(b(x), *n),
        S::MaxWidth(x, n) => S::MaxWidth(b(x), *n),
        S::Extend(x, l, r) => S::Extend(b(x), *l, *r),
        S::Offset(x, before) => S::Offset(b(x), *before),
        S::Containing(x, y) => S::Containing(b(x), b(y)),
        S::ContainedBy(x, y) => S::ContainedBy(b(x), b(y)),
        S::NotContaining(x, y) => S::NotContaining(b(x), b(y)),
        S::NotContainedBy(x, y) => S::NotContainedBy(b(x), b(y)),
        S::NonOverlapping(x, y) => S::NonOverlapping(b(x), b(y)),
        S::Overlapping(x, y) => S::Overlapping(b(x), b(y)),
        // Leaves never read another field: wrapped above.
        S::Term(_) | S::MultiTerm { .. } | S::NoMatch(_) | S::Error(_) => src.clone(),
    }
}

/// The iterator of a confirmed source in a segment: the docs of its approximation, each with
/// the source's intervals over the doc's in-memory segment (docs without any are skipped).
pub(crate) fn source_iter(spec: &Arc<SourceSpec>, inner: &IntervalsSource, seg: &SegmentContext, s: &Searcher) -> Result<Option<BI>> {
    let approx: QueryRef = approximate_intervals(spec.field, inner).unwrap_or_else(|| Arc::new(MatchAllQuery));
    let w = s.weight_of(&s.rewrite(&approx)?, ScoreMode::CompleteNoScores, 1.0)?;
    let Some(approx) = w.scorer(seg)? else { return Ok(None) };
    seg.ensure_stored()?;
    Ok(Some(Box::new(SourceII {
        spec: spec.clone(),
        inner: inner.clone(),
        approx,
        values: spec.source.leaf(seg)?,
        core: seg.clone(),
        cur: None,
        doc: -1,
    })))
}

struct SourceII {
    spec: Arc<SourceSpec>,
    inner: IntervalsSource,
    approx: Box<dyn Scorer>,
    values: Box<dyn LeafSourceValues>,
    core: SegmentContext,
    cur: Option<BI>,
    doc: i64,
}

impl SourceII {
    fn position(&mut self, mut d: u32) -> i64 {
        loop {
            if d == NO_MORE_DOCS {
                self.cur = None;
                self.doc = NO_MORE_DOCS as i64;
                return self.doc;
            }
            match self.set(d) {
                Ok(true) => {
                    self.doc = d as i64;
                    return self.doc;
                }
                Ok(false) => {}
                Err(e) => crate::search::defer_error(e),
            }
            d = self.approx.next();
        }
    }

    fn set(&mut self, d: u32) -> Result<bool> {
        self.cur = None;
        let Some(mem) = self.spec.searcher(&self.core, self.values.as_mut(), d)? else { return Ok(false) };
        let Some(mut it) = self.inner.iter(self.spec.field, &mem.segments()[0], &mem)? else { return Ok(false) };
        if it.next_doc() != 0 {
            return Ok(false);
        }
        self.cur = Some(it);
        Ok(true)
    }
}

impl II for SourceII {
    fn doc(&self) -> i64 {
        self.doc
    }
    fn next_doc(&mut self) -> i64 {
        let d = self.approx.next();
        self.position(d)
    }
    fn advance(&mut self, target: i64) -> i64 {
        let d = self.approx.advance(target.clamp(0, NO_MORE_DOCS as i64) as u32);
        self.position(d)
    }
    fn cost(&self) -> u64 {
        self.approx.cost()
    }
    fn start(&self) -> i32 {
        self.cur.as_ref().map_or(-1, |c| c.start())
    }
    fn end(&self) -> i32 {
        self.cur.as_ref().map_or(-1, |c| c.end())
    }
    fn gaps(&self) -> i32 {
        self.cur.as_ref().map_or(0, |c| c.gaps())
    }
    fn next_interval(&mut self) -> i32 {
        self.cur.as_mut().map_or(crate::query::intervals::NO_MORE_INTERVALS, |c| c.next_interval())
    }
    fn match_cost(&self) -> f32 {
        // Parsing and analyzing the source dominates.
        10_000.0
    }
}
