//! Multi-term queries — prefix, wildcard, regexp, fuzzy and term ranges — and Lucene's rewrite
//! methods (Elasticsearch's `rewrite` parameter).
//!
//! Terms are enumerated by intersecting the term dictionary with an automaton (or a byte
//! range), which skips dead prefixes in the dictionary. What happens with the terms depends on
//! the rewrite method:
//!
//! | method | result |
//! | ------ | ------ |
//! | `constant_score_blended` (default) | per segment: ≤ 16 terms → disjunction of their postings; more → docs of rare terms (df ≤ 512) in a bit set, the 16 most frequent kept as postings iterators |
//! | `constant_score` | per segment: ≤ 16 terms → disjunction; more → one bit set |
//! | `constant_score_boolean` | `ConstantScore(BooleanQuery)` of term queries (max clause count applies) |
//! | `scoring_boolean` | `BooleanQuery` of scored term queries |
//! | `top_terms_N` | the N best terms (fuzzy: by similarity, then term order) as scored term queries |
//! | `top_terms_boost_N` | the N best terms as constant-score queries boosted by their similarity |
//! | `top_terms_blended_freqs_N` | the N best terms scored with blended statistics (fuzzy's default) |
//!
//! Explanations of the constant-score methods describe the query itself (`body:qu*`), as
//! Lucene's `ConstantScoreWeight` does.

use std::any::Any;
use std::ops::Bound;
use std::sync::Arc;

use search_codec::automaton::{Automaton, Dfa, Prefix};
use search_codec::{NO_MORE_DOCS, PostingsCursor, PostingsFlags, TermState, TermsIter, TermsReader};

use crate::error::{Error, Result};
use crate::query::basic::{BoostQuery, ConstScorer, ConstantScoreQuery, SetScorer, explain_const};
use crate::query::boolean::{BooleanQuery, Occur};
use crate::query::term::{TermQuery, term_text};
use crate::schema::Schema;
use crate::search::scorers::DisjunctionScorer;
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::similarity::TermStats;
use crate::util::bitset::DocIdSetBuilder;

/// Lucene's `BOOLEAN_REWRITE_TERM_COUNT_THRESHOLD`.
const BOOLEAN_REWRITE_TERMS: usize = 16;
/// Terms with at most this many docs go to the bit set (blended rewrite).
const POSTINGS_PRE_PROCESS_THRESHOLD: u32 = 512;

/// The `rewrite` parameter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RewriteMethod {
    #[default]
    ConstantScoreBlended,
    ConstantScore,
    ConstantScoreBoolean,
    ScoringBoolean,
    TopTerms(usize),
    TopTermsBoost(usize),
    TopTermsBlendedFreqs(usize),
}

impl RewriteMethod {
    /// Parses Elasticsearch's names (`constant_score`, `top_terms_10`, …).
    pub fn parse(s: &str) -> Option<RewriteMethod> {
        Some(match s {
            "constant_score_blended" => RewriteMethod::ConstantScoreBlended,
            "constant_score" => RewriteMethod::ConstantScore,
            "constant_score_boolean" => RewriteMethod::ConstantScoreBoolean,
            "scoring_boolean" => RewriteMethod::ScoringBoolean,
            _ => {
                let num = |p: &str| s.strip_prefix(p).and_then(|n| n.parse::<usize>().ok());
                if let Some(n) = num("top_terms_blended_freqs_") {
                    RewriteMethod::TopTermsBlendedFreqs(n)
                } else if let Some(n) = num("top_terms_boost_") {
                    RewriteMethod::TopTermsBoost(n)
                } else {
                    RewriteMethod::TopTerms(num("top_terms_")?)
                }
            }
        })
    }
}

/// Which terms a multi-term query matches.
#[derive(Clone)]
pub enum TermsMatcher {
    Prefix(Vec<u8>),
    /// A wildcard / regexp / case-insensitive automaton; `desc` is the query's Lucene string
    /// without the field (`foo*bar`, `/a.c/`).
    Automaton { aut: Arc<dyn Automaton>, desc: String },
    Range { lower: Option<Vec<u8>>, upper: Option<Vec<u8>>, include_lower: bool, include_upper: bool },
    Fuzzy { term: String, max_edits: u32, prefix_len: usize, transpositions: bool, aut: Arc<Dfa> },
    /// A set of exact terms (`TermInSetQuery`: `terms`, `ids`), sorted and unique.
    Set(Arc<Vec<Vec<u8>>>),
}

impl std::fmt::Debug for TermsMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TermsMatcher::Prefix(p) => write!(f, "Prefix({})", term_text(p)),
            TermsMatcher::Automaton { desc, .. } => write!(f, "Automaton({desc})"),
            TermsMatcher::Range { lower, upper, include_lower, include_upper } => {
                write!(f, "Range({lower:?},{upper:?},{include_lower},{include_upper})")
            }
            TermsMatcher::Fuzzy { term, max_edits, prefix_len, transpositions, .. } => {
                write!(f, "Fuzzy({term}~{max_edits},{prefix_len},{transpositions})")
            }
            TermsMatcher::Set(t) => write!(f, "Set({})", t.len()),
        }
    }
}

/// A query matching every term accepted by a [`TermsMatcher`].
#[derive(Clone, Debug)]
pub struct MultiTermQuery {
    pub field: u32,
    pub matcher: TermsMatcher,
    pub rewrite: RewriteMethod,
}

impl MultiTermQuery {
    pub fn prefix(field: u32, prefix: impl Into<Vec<u8>>) -> MultiTermQuery {
        MultiTermQuery { field, matcher: TermsMatcher::Prefix(prefix.into()), rewrite: RewriteMethod::default() }
    }

    /// A wildcard pattern (`*`, `?`, `\` escapes), optionally ASCII case-insensitive.
    pub fn wildcard(field: u32, pattern: &str, case_insensitive: bool) -> Result<MultiTermQuery> {
        let aut = Dfa::wildcard(pattern, case_insensitive, search_codec::automaton::dfa::DEFAULT_WORK_LIMIT)
            .map_err(|e| Error::es("too_complex_to_determinize_exception", e.to_string(), 400))?;
        Ok(MultiTermQuery {
            field,
            matcher: TermsMatcher::Automaton { aut: Arc::new(aut), desc: pattern.to_string() },
            rewrite: RewriteMethod::default(),
        })
    }

    /// A Lucene regular expression with `flags` (`regexp::ALL` = Elasticsearch's default).
    pub fn regexp(field: u32, pattern: &str, flags: u32, case_insensitive: bool, max_determinized_states: u32) -> Result<MultiTermQuery> {
        // Bound the unrolled size before building anything (repetitions multiply).
        if crate::util::regex_size::estimated_states(pattern) > crate::util::regex_size::MAX_EXPANDED_STATES {
            return Err(Error::es(
                "too_complex_to_determinize_exception",
                format!(
                    "Determinizing automaton would result in more than {max_determinized_states} states: the regular expression [{pattern}] expands to too many states"
                ),
                400,
            ));
        }
        let aut = search_codec::automaton::regexp::compile(pattern, flags, case_insensitive, max_determinized_states)
            .map_err(|e| match e {
                search_codec::automaton::regexp::RegexpError::TooComplex(t) => {
                    Error::es("too_complex_to_determinize_exception", t.to_string(), 400)
                }
                other => Error::illegal(other.to_string()),
            })?;
        Ok(MultiTermQuery {
            field,
            matcher: TermsMatcher::Automaton { aut: Arc::new(aut), desc: format!("/{pattern}/") },
            rewrite: RewriteMethod::default(),
        })
    }

    /// Lucene's `FuzzyQuery` (`max_edits` ≤ 2); rewrite defaults to
    /// `top_terms_blended_freqs_{max_expansions}`.
    pub fn fuzzy(field: u32, term: &str, max_edits: u32, prefix_len: usize, max_expansions: usize, transpositions: bool) -> MultiTermQuery {
        let aut = Dfa::levenshtein(term, max_edits.min(2), prefix_len, transpositions);
        MultiTermQuery {
            field,
            matcher: TermsMatcher::Fuzzy { term: term.to_string(), max_edits: max_edits.min(2), prefix_len, transpositions, aut: Arc::new(aut) },
            rewrite: RewriteMethod::TopTermsBlendedFreqs(max_expansions),
        }
    }

    /// A term range (`TermRangeQuery`); `None` bounds are open.
    pub fn range(field: u32, lower: Option<Vec<u8>>, upper: Option<Vec<u8>>, include_lower: bool, include_upper: bool) -> MultiTermQuery {
        MultiTermQuery {
            field,
            matcher: TermsMatcher::Range { lower, upper, include_lower, include_upper },
            rewrite: RewriteMethod::default(),
        }
    }

    /// Lucene's `TermInSetQuery` (terms are sorted and deduplicated).
    pub fn set(field: u32, terms: impl IntoIterator<Item = Vec<u8>>) -> MultiTermQuery {
        let mut v: Vec<Vec<u8>> = terms.into_iter().collect();
        v.sort();
        v.dedup();
        MultiTermQuery { field, matcher: TermsMatcher::Set(Arc::new(v)), rewrite: RewriteMethod::default() }
    }

    /// Any automaton, with the description used in `toString`.
    pub fn automaton(field: u32, aut: Arc<dyn Automaton>, desc: impl Into<String>) -> MultiTermQuery {
        MultiTermQuery { field, matcher: TermsMatcher::Automaton { aut, desc: desc.into() }, rewrite: RewriteMethod::default() }
    }

    pub fn with_rewrite(mut self, r: RewriteMethod) -> Self {
        self.rewrite = r;
        self
    }

    /// Calls `f(term, state)` for every matching term of one segment, in term order, until it
    /// returns false.
    /// Visits the segment's matching terms in order until `f` returns false.
    pub(crate) fn enumerate_terms(&self, t: &TermsReader, f: &mut dyn FnMut(&[u8], TermState) -> bool) {
        self.enumerate(t, f)
    }

    fn enumerate(&self, t: &TermsReader, f: &mut dyn FnMut(&[u8], TermState) -> bool) {
        let mut run = |mut it: TermsIter<'_>| {
            while let Some(term) = it.next() {
                let term = term.to_vec();
                let st = it.state();
                if !f(&term, st) {
                    return;
                }
            }
        };
        match &self.matcher {
            TermsMatcher::Prefix(p) => {
                let a = Prefix::new(p.clone());
                run(t.intersect(&a))
            }
            TermsMatcher::Automaton { aut, .. } => run(t.intersect(aut.as_ref())),
            TermsMatcher::Fuzzy { aut, .. } => run(t.intersect(aut.as_ref())),
            TermsMatcher::Range { lower, upper, include_lower, include_upper } => {
                let lo = match lower {
                    None => Bound::Unbounded,
                    Some(l) if *include_lower => Bound::Included(l.as_slice()),
                    Some(l) => Bound::Excluded(l.as_slice()),
                };
                let hi = match upper {
                    None => Bound::Unbounded,
                    Some(u) if *include_upper => Bound::Included(u.as_slice()),
                    Some(u) => Bound::Excluded(u.as_slice()),
                };
                run(t.range(lo, hi))
            }
            TermsMatcher::Set(terms) => {
                for term in terms.iter() {
                    if let Some(st) = t.seek_exact_hashed(term, search_codec::bloom::hash64(term))
                        && !f(term, st)
                    {
                        return;
                    }
                }
            }
        }
    }

    /// Whether `t` is one of the terms this query matches.
    pub fn matches_term(&self, t: &[u8]) -> bool {
        match &self.matcher {
            TermsMatcher::Set(terms) => terms.binary_search_by(|x| x.as_slice().cmp(t)).is_ok(),
            TermsMatcher::Prefix(p) => t.starts_with(p),
            TermsMatcher::Automaton { aut, .. } => aut.matches(t),
            TermsMatcher::Fuzzy { aut, .. } => aut.matches(t),
            TermsMatcher::Range { lower, upper, include_lower, include_upper } => {
                let lo = match lower {
                    None => true,
                    Some(l) => {
                        if *include_lower { t >= l.as_slice() } else { t > l.as_slice() }
                    }
                };
                let hi = match upper {
                    None => true,
                    Some(u) => {
                        if *include_upper { t <= u.as_slice() } else { t < u.as_slice() }
                    }
                };
                lo && hi
            }
        }
    }

    /// Fuzzy similarity of a matched term (Lucene's `FuzzyTermsEnum` boost).
    fn boost(&self, term: &[u8]) -> f32 {
        let TermsMatcher::Fuzzy { term: q, prefix_len, transpositions, .. } = &self.matcher else { return 1.0 };
        let t = String::from_utf8_lossy(term);
        let qc: Vec<char> = q.chars().collect();
        let tc: Vec<char> = t.chars().collect();
        let p = (*prefix_len).min(qc.len()).min(tc.len());
        let qs: String = qc[p..].iter().collect();
        let ts: String = tc[p..].iter().collect();
        let ed = search_codec::automaton::edit_distance(&qs, &ts, *transpositions);
        if ed == 0 {
            return 1.0;
        }
        let min_len = tc.len().min(qc.len());
        1.0f32 - ed as f32 / min_len as f32
    }

    /// Every matching term across segments: `(term, boost, stats)` in term order.
    fn collect_all(&self, s: &Searcher, max: Option<usize>) -> Result<Vec<(Vec<u8>, f32, TermStats)>> {
        let mut map: std::collections::BTreeMap<Vec<u8>, (f32, TermStats)> = std::collections::BTreeMap::new();
        let mut too_many = false;
        for seg in s.segments() {
            let Some(t) = seg.reader.terms(self.field) else { continue };
            self.enumerate(t, &mut |term, st| {
                let e = map.entry(term.to_vec()).or_insert((0.0, TermStats { doc_freq: 0, total_term_freq: 0 }));
                e.1.doc_freq += st.doc_freq as u64;
                e.1.total_term_freq += st.total_term_freq;
                if let Some(m) = max
                    && map.len() > m
                {
                    too_many = true;
                    return false;
                }
                true
            });
            if too_many {
                return Err(Error::too_many_clauses(s.limits().max_clause_count));
            }
        }
        Ok(map.into_iter().map(|(t, (_, st))| {
            let b = self.boost(&t);
            (t, b, st)
        }).collect())
    }

    /// The `size` best terms: highest boost, then smallest term (Lucene `TopTermsRewrite`).
    pub(crate) fn top_terms(&self, s: &Searcher, size: usize) -> Result<Vec<(Vec<u8>, f32, TermStats)>> {
        let size = size.min(s.limits().max_clause_count);
        let mut all = self.collect_all(s, None)?;
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        all.truncate(size);
        all.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(all)
    }
}

impl Query for MultiTermQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(MtConstWeight {
            q: self.clone(),
            score: boost,
            desc: self.lucene_string(s.schema()),
            blended: self.rewrite == RewriteMethod::ConstantScoreBlended,
        }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        let max = s.limits().max_clause_count;
        let term_q = |t: &[u8]| -> QueryRef { Arc::new(TermQuery::new(self.field, t.to_vec())) };
        Ok(match self.rewrite {
            RewriteMethod::ConstantScoreBlended | RewriteMethod::ConstantScore => None,
            RewriteMethod::ScoringBoolean | RewriteMethod::ConstantScoreBoolean => {
                let terms = self.collect_all(s, Some(max))?;
                let mut b = BooleanQuery::new();
                for (t, boost, _) in &terms {
                    let tq = term_q(t);
                    b = b.add(Occur::Should, if *boost != 1.0 { Arc::new(BoostQuery::new(tq, *boost)) } else { tq });
                }
                let b: QueryRef = Arc::new(b);
                Some(if self.rewrite == RewriteMethod::ConstantScoreBoolean { Arc::new(ConstantScoreQuery::new(b)) } else { b })
            }
            RewriteMethod::TopTerms(n) => {
                let mut b = BooleanQuery::new();
                for (t, boost, _) in self.top_terms(s, n)? {
                    let tq = term_q(&t);
                    b = b.add(Occur::Should, if boost.max(0.0) != 1.0 { Arc::new(BoostQuery::new(tq, boost.max(0.0))) } else { tq });
                }
                Some(Arc::new(b))
            }
            RewriteMethod::TopTermsBoost(n) => {
                let mut b = BooleanQuery::new();
                for (t, boost, _) in self.top_terms(s, n)? {
                    let cq: QueryRef = Arc::new(ConstantScoreQuery::new(term_q(&t)));
                    b = b.add(Occur::Should, if boost.max(0.0) != 1.0 { Arc::new(BoostQuery::new(cq, boost.max(0.0))) } else { cq });
                }
                Some(Arc::new(b))
            }
            RewriteMethod::TopTermsBlendedFreqs(n) => {
                let terms = self.top_terms(s, n)?;
                Some(blended(self.field, &terms))
            }
        })
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let f = schema.name(self.field);
        match &self.matcher {
            TermsMatcher::Prefix(p) => format!("{f}:{}*", term_text(p)),
            TermsMatcher::Automaton { desc, .. } => format!("{f}:{desc}"),
            TermsMatcher::Fuzzy { term, max_edits, .. } => format!("{f}:{term}~{max_edits}"),
            TermsMatcher::Set(terms) => {
                let parts: Vec<String> = terms.iter().map(|t| term_text(t)).collect();
                format!("{f}:({})", parts.join(" "))
            }
            TermsMatcher::Range { lower, upper, include_lower, include_upper } => {
                let b = |t: &Option<Vec<u8>>| match t {
                    None => "*".to_string(),
                    Some(t) => {
                        let s = term_text(t);
                        if s == "*" { "\\*".to_string() } else { s }
                    }
                };
                format!(
                    "{f}:{}{} TO {}{}",
                    if *include_lower { '[' } else { '{' },
                    b(lower),
                    b(upper),
                    if *include_upper { ']' } else { '}' }
                )
            }
        }
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        if let TermsMatcher::Set(terms) = &self.matcher {
            for t in terms.iter() {
                v.term(self.field, t);
            }
            return;
        }
        v.multi_term_query(self);
    }

    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        use crate::search::{hash_bytes, hash_tag};
        hash_tag(h, "multi_term");
        h.write_u32(self.field);
        match &self.matcher {
            TermsMatcher::Prefix(p) => {
                h.write_u8(0);
                hash_bytes(h, p);
            }
            TermsMatcher::Range { lower, upper, include_lower, include_upper } => {
                h.write_u8(1);
                for b in [lower, upper] {
                    match b {
                        Some(b) => hash_bytes(h, b),
                        None => h.write_u8(2),
                    }
                }
                h.write_u8(*include_lower as u8);
                h.write_u8(*include_upper as u8);
            }
            TermsMatcher::Fuzzy { term, max_edits, prefix_len, transpositions, .. } => {
                h.write_u8(3);
                hash_bytes(h, term.as_bytes());
                h.write_u32(*max_edits);
                h.write_usize(*prefix_len);
                h.write_u8(*transpositions as u8);
            }
            TermsMatcher::Set(terms) => {
                h.write_u8(4);
                h.write_usize(terms.len());
                for t in terms.iter() {
                    hash_bytes(h, t);
                }
            }
            // The description does not capture flags or case-insensitivity.
            TermsMatcher::Automaton { .. } => return false,
        }
        true
    }

    /// Lucene's class: the constant-score wrappers the default rewrites produce, else the
    /// query's own class.
    fn profile_name(&self) -> String {
        match self.rewrite {
            RewriteMethod::ConstantScoreBlended => "MultiTermQueryConstantScoreBlendedWrapper",
            RewriteMethod::ConstantScore => "MultiTermQueryConstantScoreWrapper",
            _ => match &self.matcher {
                TermsMatcher::Prefix(_) => "PrefixQuery",
                TermsMatcher::Automaton { desc, .. } if desc.starts_with('/') => "RegexpQuery",
                TermsMatcher::Automaton { .. } => "WildcardQuery",
                TermsMatcher::Range { .. } => "TermRangeQuery",
                TermsMatcher::Fuzzy { .. } => "FuzzyQuery",
                TermsMatcher::Set(_) => "TermInSetQuery",
            },
        }
        .to_string()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Lucene's `BlendedTermQuery` with the boolean rewrite: every term scores with the maximum
/// document frequency and the summed total term frequency of the group.
pub fn blended(field: u32, terms: &[(Vec<u8>, f32, TermStats)]) -> QueryRef {
    let df = terms.iter().map(|t| t.2.doc_freq).max().unwrap_or(0);
    let ttf = terms.iter().map(|t| t.2.total_term_freq).sum();
    let mut b = BooleanQuery::new();
    for (t, boost, _) in terms {
        let tq: QueryRef = Arc::new(TermQuery::with_stats(field, t.clone(), TermStats { doc_freq: df, total_term_freq: ttf }));
        let boost = boost.max(0.0);
        b = b.add(Occur::Should, if boost != 1.0 { Arc::new(BoostQuery::new(tq, boost)) } else { tq });
    }
    Arc::new(b)
}

/// Constant-score execution of a multi-term query, per segment.
struct MtConstWeight {
    q: MultiTermQuery,
    score: f32,
    desc: String,
    blended: bool,
}

/// One term's postings as a non-scoring scorer.
struct PostingsScorer {
    c: PostingsCursor,
}

impl crate::search::Scorable for PostingsScorer {
    fn score(&mut self) -> f32 {
        0.0
    }
}

impl Scorer for PostingsScorer {
    fn doc(&self) -> u32 {
        self.c.doc()
    }
    fn next(&mut self) -> u32 {
        self.c.next()
    }
    fn advance(&mut self, t: u32) -> u32 {
        self.c.advance(t)
    }
    fn cost(&self) -> u64 {
        self.c.cost()
    }
}

/// Collects one segment's matching terms: the first 16 as postings, the rest into a doc set
/// (frequent ones kept as postings with the blended method).
struct Accum<'a> {
    t: &'a TermsReader,
    field_docs: u32,
    max_doc: u32,
    blended: bool,
    first: Vec<TermState>,
    high: Vec<TermState>,
    set: Option<DocIdSetBuilder>,
    cursor: PostingsCursor,
    /// A term matching every doc with the field.
    full: Option<TermState>,
}

impl Accum<'_> {
    fn add_to_set(&mut self, st: &TermState) {
        let set = self.set.get_or_insert_with(|| DocIdSetBuilder::new(self.max_doc));
        self.t.postings_reset(st, PostingsFlags::DOCS, &mut self.cursor);
        let mut d = self.cursor.next();
        if d == NO_MORE_DOCS {
            return;
        }
        if st.doc_freq as usize * 32 > self.max_doc as usize {
            self.cursor.into_bitset(NO_MORE_DOCS, set.bitset().words_mut(), 0);
        } else {
            while d != NO_MORE_DOCS {
                set.add(d);
                d = self.cursor.next();
            }
        }
    }

    fn place(&mut self, st: TermState) {
        if self.blended && st.doc_freq > POSTINGS_PRE_PROCESS_THRESHOLD {
            self.high.push(st);
            if self.high.len() > BOOLEAN_REWRITE_TERMS {
                let (i, _) = self.high.iter().enumerate().min_by_key(|(_, s)| s.doc_freq).unwrap();
                let low = self.high.swap_remove(i);
                self.add_to_set(&low);
            }
        } else {
            self.add_to_set(&st);
        }
    }

    /// Returns false to stop enumerating.
    fn term(&mut self, st: TermState) -> bool {
        if st.doc_freq == self.field_docs {
            self.full = Some(st);
            return false;
        }
        if self.set.is_none() && self.first.len() < BOOLEAN_REWRITE_TERMS {
            self.first.push(st);
            return true;
        }
        if self.set.is_none() {
            self.set = Some(DocIdSetBuilder::new(self.max_doc));
            for s in std::mem::take(&mut self.first) {
                self.place(s);
            }
        }
        self.place(st);
        true
    }
}

impl MtConstWeight {
    fn segment_scorer(&self, seg: &SegmentContext) -> Option<Box<dyn Scorer>> {
        let t = seg.reader.terms(self.q.field)?;
        let mut acc = Accum {
            t,
            field_docs: t.stats().doc_count,
            max_doc: seg.max_doc(),
            blended: self.blended,
            first: Vec::new(),
            high: Vec::new(),
            set: None,
            cursor: PostingsCursor::empty(),
            full: None,
        };
        self.q.enumerate(t, &mut |_, st| acc.term(st));
        let wrap = |inner: Box<dyn Scorer>| -> Box<dyn Scorer> { Box::new(ConstScorer { inner, score: self.score }) };
        if let Some(st) = acc.full {
            return Some(wrap(Box::new(PostingsScorer { c: t.postings(&st, PostingsFlags::DOCS) })));
        }
        let mut subs: Vec<Box<dyn Scorer>> = acc
            .first
            .iter()
            .chain(acc.high.iter())
            .map(|st| Box::new(PostingsScorer { c: t.postings(st, PostingsFlags::DOCS) }) as Box<dyn Scorer>)
            .collect();
        if let Some(set) = acc.set {
            subs.push(Box::new(SetScorer::new(Arc::new(set.build()), 0.0)));
        }
        match subs.len() {
            0 => None,
            1 => Some(wrap(subs.pop().unwrap())),
            _ => Some(wrap(Box::new(DisjunctionScorer::new(subs, false)))),
        }
    }
}

impl Weight for MtConstWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        seg.ensure_postings(self.q.field)?;
        Ok(self.segment_scorer(seg))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

/// The terms a multi-term query expands to (span_multi): every term, or the best N with a
/// top-terms rewrite; fails with too_many_clauses past the limit, as Lucene's span rewrites.
pub fn collect_terms(m: &MultiTermQuery, s: &Searcher) -> Result<Vec<Vec<u8>>> {
    let terms = match m.rewrite {
        RewriteMethod::TopTerms(n) | RewriteMethod::TopTermsBoost(n) | RewriteMethod::TopTermsBlendedFreqs(n) => m.top_terms(s, n)?,
        _ => m.collect_all(s, Some(s.limits().max_clause_count))?,
    };
    Ok(terms.into_iter().map(|t| t.0).collect())
}
