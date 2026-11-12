//! `_termStats` of score scripts (Elasticsearch's `ScriptTermStats`).
//!
//! A `script_score` query, or a `function_score` script function, whose script reads
//! `_termStats` collects the terms of its query once (`QueryVisitor.termCollector`: every
//! clause but `must_not`), with their shard statistics: `docFreq` and `totalTermFreq` through
//! the searcher, so a `dfs_query_then_fetch` search sees the statistics summed over every
//! shard. Per segment it opens one postings cursor per term and answers, for the document
//! being scored: how many terms it matches, their frequencies and their (one-based)
//! positions. Fields indexed without positions report position 0 for every occurrence, as
//! Lucene's `nextPosition()` returns -1 there.
//!
//! The script wrapper only exists for scripts that read the variable, so other score scripts
//! pay nothing. A document's statistics are computed once and kept until the next document:
//! the script may load `_termStats` several times.

use std::sync::Arc;

use search_index::SegmentContext;
use search_index::QueryRef;
use search_index::query::ScoreScript;
use search_index::query::function_score::LeafScoreScript;
use search_index::search::{QueryVisitor, Searcher};

use super::scripts::{DocHost, Painless};

/// The query terms and their shard statistics.
#[derive(Debug, Default)]
pub struct QueryTerms {
    /// `(field, term)`, distinct.
    terms: Vec<(u32, Vec<u8>)>,
    doc_freq: Vec<f64>,
    total_term_freq: Vec<f64>,
}

/// Collects distinct `(field, term)` pairs in visiting order; a set keeps a terms query with
/// thousands of values linear.
struct Collector<'a> {
    terms: &'a mut Vec<(u32, Vec<u8>)>,
    seen: std::collections::HashSet<(u32, Vec<u8>)>,
}

impl QueryVisitor for Collector<'_> {
    fn term(&mut self, field: u32, term: &[u8]) {
        let key = (field, term.to_vec());
        if !self.seen.contains(&key) {
            self.terms.push(key.clone());
            self.seen.insert(key);
        }
    }

    /// Lucene's `termCollector` only implements `consumeTerms`: a `TermInSetQuery` of
    /// several terms contributes none.
    fn term_set(&mut self, _field: u32, _terms: &[Vec<u8>]) {}
}

impl QueryTerms {
    /// The terms `q` matches on (rewritten first, as the weight sees it), with statistics from
    /// `searcher`.
    pub fn collect(searcher: &Searcher, q: &QueryRef) -> QueryTerms {
        let q = searcher.rewrite(q).unwrap_or_else(|_| q.clone());
        let mut terms = Vec::new();
        q.visit(&mut Collector { terms: &mut terms, seen: std::collections::HashSet::new() });
        let mut doc_freq = Vec::with_capacity(terms.len());
        let mut total_term_freq = Vec::with_capacity(terms.len());
        for (f, t) in &terms {
            let s = searcher.term_stats(*f, t);
            doc_freq.push(s.doc_freq as f64);
            total_term_freq.push(s.total_term_freq as f64);
        }
        QueryTerms { terms, doc_freq, total_term_freq }
    }
}

/// One segment's cursors over the query terms.
pub struct SegTermStats {
    shared: Arc<QueryTerms>,
    seg: SegmentContext,
    cursors: Vec<Option<(search_codec::PostingsCursor, bool)>>,
    /// The statistics of `doc`, once computed.
    cached: Option<(u32, search_painless::TermStats)>,
}

impl SegTermStats {
    pub fn new(shared: Arc<QueryTerms>, seg: &SegmentContext) -> SegTermStats {
        let cursors = shared.terms.iter().map(|(f, t)| open(seg, *f, t)).collect();
        SegTermStats { shared, seg: seg.clone(), cursors, cached: None }
    }

    /// The statistics of segment doc `doc`.
    pub fn get(&mut self, doc: u32) -> search_painless::TermStats {
        if let Some((d, s)) = &self.cached
            && *d == doc
        {
            return s.clone();
        }
        let n = self.shared.terms.len();
        let mut out = search_painless::TermStats {
            unique_terms_count: n as i32,
            matched_terms_count: 0,
            doc_freq: self.shared.doc_freq.clone(),
            total_term_freq: self.shared.total_term_freq.clone(),
            term_freq: Vec::with_capacity(n),
            term_positions: Vec::with_capacity(n),
        };
        for i in 0..n {
            // Cursors only move forward: a document before the current one reopens it.
            if self.cursors[i].as_ref().is_some_and(|(c, _)| c.doc() != u32::MAX && c.doc() > doc) {
                let (f, t) = &self.shared.terms[i];
                self.cursors[i] = open(&self.seg, *f, t);
            }
            let Some((c, positions)) = self.cursors[i].as_mut() else {
                out.term_freq.push(0.0);
                continue;
            };
            if c.advance(doc) != doc {
                out.term_freq.push(0.0);
                continue;
            }
            out.matched_terms_count += 1;
            let freq = c.freq();
            out.term_freq.push(freq as f64);
            let positions = *positions;
            let pos = (0..freq).map(|_| if positions { c.next_position() as f64 + 1.0 } else { 0.0 }).collect();
            out.term_positions.push(pos);
        }
        self.cached = Some((doc, out.clone()));
        out
    }
}

fn open(seg: &SegmentContext, field: u32, term: &[u8]) -> Option<(search_codec::PostingsCursor, bool)> {
    let terms = seg.reader.terms(field)?;
    let st = terms.seek_exact(term)?;
    let positions = terms.has_positions();
    let flags = if positions { search_codec::PostingsFlags::POSITIONS } else { search_codec::PostingsFlags::FREQS };
    Some((terms.postings(&st, flags), positions))
}

/// Whether a score script reads `_termStats` (`needs_termStats`).
pub fn needs_term_stats(p: &Painless) -> bool {
    p.bound.painless().is_some_and(|s| s.uses_var("_termStats"))
}

/// A score script that reads `_termStats`, over the terms of its query.
pub struct TermStatsScript {
    pub inner: Painless,
    pub terms: Arc<QueryTerms>,
}

impl std::fmt::Debug for TermStatsScript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TermStatsScript").field("terms", &self.terms.terms.len()).finish()
    }
}

impl ScoreScript for TermStatsScript {
    fn needs_score(&self) -> bool {
        ScoreScript::needs_score(&self.inner)
    }

    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafScoreScript>> {
        let mut host: DocHost = self.inner.host(seg);
        host.term_stats = Some(Box::new(SegTermStats::new(self.terms.clone(), seg)));
        self.inner.score_leaf(host)
    }

    fn describe(&self) -> String {
        ScoreScript::describe(&self.inner)
    }
}

/// `script` as an engine score script: with `_termStats` over `query`'s terms when it reads
/// them.
pub fn score_script(script: Painless, searcher: &Searcher, query: &QueryRef) -> Arc<dyn ScoreScript> {
    if needs_term_stats(&script) {
        Arc::new(TermStatsScript { inner: script, terms: Arc::new(QueryTerms::collect(searcher, query)) })
    } else {
        Arc::new(script)
    }
}
