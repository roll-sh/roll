//! Highlighting: Elasticsearch's `unified` (default), `plain` and `fvh` highlighters.
//!
//! Inputs are the field's values (from `_source`), the query (or `highlight_query`), and the
//! field's index analyzer; the engine re-analyzes the values to find match offsets (identical
//! to postings or term vector offsets, which come from the same analysis). Matches follow
//! Lucene's *weight matches* mode that Elasticsearch enables: a phrase highlights as one
//! span (`<em>quick brown fox</em>`), terms and multi-term expansions as single tokens.
//!
//! Unified: values are joined with `\0` (Elasticsearch's `MULTIVAL_SEP_CHAR`); passages are
//! built with Lucene's `FieldHighlighter` algorithm over a break iterator (sentences bounded by
//! `fragment_size` — Elasticsearch's `BoundedBreakIteratorScanner` — words, or one passage per
//! value with `number_of_fragments: 0`), scored with Lucene's `PassageScorer` (BM25 over the
//! passage), the best `number_of_fragments` kept, ordered by position (or score), and
//! formatted like Elasticsearch's `CustomPassageFormatter`. Plain: Lucene's `Highlighter` with
//! the `span` (default), `simple` or null fragmenter and `QueryScorer` scores, per value. Fvh:
//! Lucene's `FastVectorHighlighter` (see [`fvh`]). Unified and plain use the first of
//! `pre_tags`/`post_tags`; fvh cycles through them by query clause (`tags_schema: styled`).
//! Offsets are UTF-16 code units, as in Java.

pub mod breaks;
pub mod fvh;
pub mod plain;
pub mod unified;

use std::collections::HashSet;

use search_analysis::Analyzer;

use crate::error::{Error, Result};
use crate::search::{QueryRef, QueryVisitor, Searcher, TermMatcher};

/// Which highlighter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HighlighterType {
    #[default]
    Unified,
    Plain,
    /// The fast vector highlighter (fields with `term_vector: with_positions_offsets`).
    Fvh,
}

/// `boundary_scanner`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoundaryScanner {
    #[default]
    Sentence,
    Word,
    /// `chars` (fvh's default): the nearest of `boundary_chars` within `boundary_max_scan`.
    /// The unified highlighter rejects it unless `number_of_fragments` is 0.
    Chars,
}

/// `fragmenter` (plain highlighter).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Fragmenter {
    #[default]
    Span,
    Simple,
}

/// `encoder`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoder {
    #[default]
    Default,
    Html,
}

impl Encoder {
    pub fn encode(self, s: &str, out: &mut String) {
        match self {
            Encoder::Default => out.push_str(s),
            Encoder::Html => {
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("&quot;"),
                        '&' => out.push_str("&amp;"),
                        '<' => out.push_str("&lt;"),
                        '>' => out.push_str("&gt;"),
                        '\'' => out.push_str("&#x27;"),
                        '/' => out.push_str("&#x2F;"),
                        c => out.push(c),
                    }
                }
            }
        }
    }
}

/// Highlighting options of one field (Elasticsearch defaults).
#[derive(Clone, Debug)]
pub struct HighlightOptions {
    pub kind: HighlighterType,
    /// `pre_tags` / `post_tags` (`<em>` / `</em>`).
    pub pre_tags: Vec<String>,
    pub post_tags: Vec<String>,
    /// `fragment_size` (100).
    pub fragment_size: usize,
    /// `number_of_fragments` (5; 0 = whole field).
    pub number_of_fragments: usize,
    /// `no_match_size` (0).
    pub no_match_size: usize,
    pub encoder: Encoder,
    /// `order: score`.
    pub order_by_score: bool,
    /// `boundary_scanner`: `None` is the highlighter's default (sentence for unified, chars
    /// for fvh).
    pub boundary_scanner: Option<BoundaryScanner>,
    pub boundary_chars: String,
    pub boundary_max_scan: usize,
    pub fragmenter: Fragmenter,
    /// `fragment_offset` (fvh): margin before the first match (6).
    pub fragment_offset: Option<usize>,
    /// `phrase_limit` (fvh): most matches considered (256).
    pub phrase_limit: usize,
    /// The field has term vectors with positions and offsets (fvh requires them).
    pub term_vectors_with_offsets: bool,
    /// The field's analyzer produces offsets out of order (n-grams, word delimiters):
    /// Elasticsearch's `containsBrokenAnalysis`, which makes fvh sort matches by offset.
    pub fix_broken_analysis: bool,
    /// Only terms of the highlighted field count (`require_field_match`, true).
    pub require_field_match: bool,
    /// Other fields whose matches also highlight this one (`matched_fields`).
    pub matched_fields: Vec<u32>,
    /// `index.highlight.max_analyzed_offset` (1 000 000).
    pub index_max_analyzed_offset: usize,
    /// The query's `max_analyzed_offset` (truncates instead of failing).
    pub max_analyzed_offset: Option<usize>,
    /// Whether the field is tokenized (keyword fields highlight whole values).
    pub tokenized: bool,
    /// For error messages: field name, doc id and index name.
    pub field_name: String,
    pub doc_id: String,
    pub index_name: String,
}

impl HighlightOptions {
    /// The first pre tag (unified and plain use only it).
    pub fn pre_tag(&self) -> &str {
        self.pre_tags.first().map_or("", |s| s.as_str())
    }

    pub fn post_tag(&self) -> &str {
        self.post_tags.first().map_or("", |s| s.as_str())
    }

    /// Elasticsearch's `tags_schema: styled` tags.
    pub fn styled_tags() -> (Vec<String>, Vec<String>) {
        let pre = (1..=10).map(|i| format!("<em class=\"hlt{i}\">")).collect();
        (pre, vec!["</em>".into()])
    }
}

impl Default for HighlightOptions {
    fn default() -> Self {
        HighlightOptions {
            kind: HighlighterType::Unified,
            pre_tags: vec!["<em>".into()],
            post_tags: vec!["</em>".into()],
            fragment_size: 100,
            number_of_fragments: 5,
            no_match_size: 0,
            encoder: Encoder::Default,
            order_by_score: false,
            boundary_scanner: None,
            boundary_chars: ".,!? \t\n".into(),
            boundary_max_scan: 20,
            fragmenter: Fragmenter::Span,
            fragment_offset: None,
            phrase_limit: 256,
            term_vectors_with_offsets: false,
            fix_broken_analysis: false,
            require_field_match: true,
            matched_fields: Vec::new(),
            index_max_analyzed_offset: 1_000_000,
            max_analyzed_offset: None,
            tokenized: true,
            field_name: String::new(),
            doc_id: String::new(),
            index_name: String::new(),
        }
    }
}

/// A phrase's slots: relative position and the terms accepted there.
pub type PhraseSlots = Vec<(u32, Vec<Vec<u8>>)>;

/// What a query asks to highlight in one field.
#[derive(Default)]
pub struct HighlightTerms {
    pub terms: HashSet<Vec<u8>>,
    pub multi: Vec<TermMatcher>,
    /// Phrases: `(relative position, accepted terms)` slots and slop.
    pub phrases: Vec<(PhraseSlots, u32)>,
}

impl HighlightTerms {
    /// Extracts the terms of `query` that apply to `fields` (`None`: any field).
    pub fn of(query: &QueryRef, fields: Option<&[u32]>) -> HighlightTerms {
        struct V<'a> {
            fields: Option<&'a [u32]>,
            out: HighlightTerms,
        }
        impl V<'_> {
            fn ok(&self, f: u32) -> bool {
                self.fields.is_none_or(|fs| fs.contains(&f))
            }
        }
        impl QueryVisitor for V<'_> {
            fn term(&mut self, field: u32, term: &[u8]) {
                if self.ok(field) {
                    self.out.terms.insert(term.to_vec());
                }
            }
            fn multi_term(&mut self, field: u32, m: TermMatcher) {
                if self.ok(field) {
                    self.out.multi.push(m);
                }
            }
            fn phrase(&mut self, field: u32, slots: &[(u32, Vec<Vec<u8>>)], slop: u32) {
                if self.ok(field) {
                    self.out.phrases.push((slots.to_vec(), slop));
                }
            }
        }
        let mut v = V { fields, out: HighlightTerms::default() };
        query.visit(&mut v);
        v.out
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.multi.is_empty() && self.phrases.is_empty()
    }
}

/// A token of the analyzed content (UTF-16 offsets).
#[derive(Clone, Debug)]
pub struct HlToken {
    pub term: Vec<u8>,
    pub position: u32,
    pub start: u32,
    pub end: u32,
    pub position_increment: u32,
}

/// A match: offsets plus the key it is scored by.
#[derive(Clone, Debug)]
pub struct HlMatch {
    pub start: u32,
    pub end: u32,
    pub key: Vec<u8>,
}

/// Analyzes the values as one field instance (position/offset gaps between values).
pub fn analyze(values: &[&str], analyzer: Option<&Analyzer>, limit: Option<usize>, tokenized: bool) -> Vec<HlToken> {
    let mut out = Vec::new();
    match analyzer {
        Some(a) if tokenized => {
            a.analyze_values(values, &mut |t| {
                if let Some(l) = limit
                    && t.start_offset as usize > l
                {
                    return;
                }
                out.push(HlToken {
                    term: t.term.as_bytes().to_vec(),
                    position: t.position,
                    start: t.start_offset,
                    end: t.end_offset,
                    position_increment: t.position_increment,
                });
            });
        }
        _ => {
            // Keyword-like: each value is one token.
            let mut off = 0u32;
            for (i, v) in values.iter().enumerate() {
                let len = v.encode_utf16().count() as u32;
                out.push(HlToken { term: v.as_bytes().to_vec(), position: i as u32, start: off, end: off + len, position_increment: 1 });
                off += len + 1;
            }
        }
    }
    out
}

/// Match intervals in the tokens, in offset order (weight matches: phrases as one span).
pub fn find_matches(tokens: &[HlToken], q: &HighlightTerms) -> Vec<HlMatch> {
    let mut out = Vec::new();
    for t in tokens {
        if q.terms.contains(&t.term) {
            out.push(HlMatch { start: t.start, end: t.end, key: t.term.clone() });
        } else if let Some(i) = q.multi.iter().position(|m| m(&t.term)) {
            // Lucene keys multi-term matches by the query, not the expanded term.
            out.push(HlMatch { start: t.start, end: t.end, key: format!("\u{0}multi{i}").into_bytes() });
        }
    }
    for (pi, (slots, slop)) in q.phrases.iter().enumerate() {
        if slots.is_empty() {
            continue;
        }
        let key = format!("\u{0}phrase{pi}").into_bytes();
        for (lo, hi) in phrase_match_offsets(tokens, slots, *slop) {
            out.push(HlMatch { start: lo, end: hi, key: key.clone() });
        }
    }
    out.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    out.dedup_by(|a, b| a.start == b.start && a.end == b.end);
    out
}

/// The offsets of every match of a phrase (Lucene's weight matches: one interval per match,
/// from the first matched term's start to the last one's end). Matches come from the
/// engine's own exact / sloppy phrase matcher, so a sloppy match spans all slots within the
/// slop together and no token serves two slots, as in the index.
fn phrase_match_offsets(tokens: &[HlToken], slots: &PhraseSlots, slop: u32) -> Vec<(u32, u32)> {
    let mut mslots = Vec::with_capacity(slots.len());
    for (rel, terms) in slots {
        let mut positions: Vec<u32> = tokens.iter().filter(|t| terms.contains(&t.term)).map(|t| t.position).collect();
        if positions.is_empty() {
            return Vec::new();
        }
        positions.sort_unstable();
        mslots.push((terms.clone(), *rel as i64, positions));
    }
    let matches = crate::query::phrase::memory_phrase_matches(mslots, slop);
    if matches.is_empty() {
        return Vec::new();
    }
    // Offsets of the phrase's tokens at a position: the first start and the last end.
    let in_phrase = |t: &HlToken| slots.iter().any(|(_, ts)| ts.contains(&t.term));
    let at = |p: u32| tokens.iter().filter(move |t| t.position == p && in_phrase(t));
    let mut out = Vec::with_capacity(matches.len());
    for (s, e) in matches {
        let (Some(lo), Some(hi)) = (at(s).map(|t| t.start).min(), at(e).map(|t| t.end).max()) else { continue };
        if lo < hi {
            out.push((lo, hi));
        }
    }
    out
}

/// Highlights one field of one doc; returns the fragments (empty: no highlight).
pub fn highlight(query: &QueryRef, values: &[&str], analyzer: Option<&Analyzer>, field: u32, opts: &HighlightOptions) -> Result<Vec<String>> {
    highlight_impl(None, query, values, analyzer, field, opts)
}

/// [`highlight`] with the searcher the hit came from: the fast vector highlighter expands
/// multi-term queries against the index as Elasticsearch does (which numbers clauses for
/// `tags_schema: styled` and sums boosts for `order: score`).
pub fn highlight_in(searcher: &Searcher, query: &QueryRef, values: &[&str], analyzer: Option<&Analyzer>, field: u32, opts: &HighlightOptions) -> Result<Vec<String>> {
    highlight_impl(Some(searcher), query, values, analyzer, field, opts)
}

fn highlight_impl(
    searcher: Option<&Searcher>,
    query: &QueryRef,
    values: &[&str],
    analyzer: Option<&Analyzer>,
    field: u32,
    opts: &HighlightOptions,
) -> Result<Vec<String>> {
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let mut fields = vec![field];
    fields.extend(opts.matched_fields.iter().copied());
    let only = if opts.require_field_match { Some(fields.as_slice()) } else { None };
    if opts.kind == HighlighterType::Fvh {
        return fvh::highlight(searcher, query, values, analyzer, only, opts);
    }
    if opts.kind == HighlighterType::Unified && opts.boundary_scanner == Some(BoundaryScanner::Chars) && opts.number_of_fragments != 0 {
        return Err(Error::illegal("Invalid boundary scanner type: chars"));
    }
    let terms = HighlightTerms::of(query, only);
    let total: usize = values.iter().map(|v| v.encode_utf16().count()).sum::<usize>() + values.len().saturating_sub(1);
    let limit = opts.max_analyzed_offset;
    if limit.is_none_or(|l| l > opts.index_max_analyzed_offset) && total > opts.index_max_analyzed_offset && opts.tokenized {
        return Err(Error::illegal(format!(
            "The length [{total}] of field [{}] in doc[{}]/index[{}] exceeds the [index.highlight.max_analyzed_offset] limit [{}]. To avoid this error, set the query parameter [max_analyzed_offset] to a value less than index setting [{}] and this will tolerate long field values by truncating them.",
            opts.field_name, opts.doc_id, opts.index_name, opts.index_max_analyzed_offset, opts.index_max_analyzed_offset
        )));
    }
    match opts.kind {
        HighlighterType::Plain => plain::highlight(values, analyzer, &terms, opts),
        _ => unified::highlight(values, analyzer, &terms, opts),
    }
}
