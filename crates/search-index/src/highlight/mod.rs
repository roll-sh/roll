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
pub mod spans;
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
    /// Offsets are indexed (postings with offsets, or term vectors with offsets): Lucene's
    /// unified highlighter reads them instead of re-analyzing (see [`find_matches`]).
    pub offsets_indexed: bool,
    /// The field's analyzer produces offsets out of order (n-grams, word delimiters):
    /// Elasticsearch's `containsBrokenAnalysis`, which makes fvh sort matches by offset.
    pub fix_broken_analysis: bool,
    /// Only terms of the highlighted field count (`require_field_match`, true).
    pub require_field_match: bool,
    /// Other fields whose matches also highlight this one (`matched_fields`).
    pub matched_fields: Vec<u32>,
    /// The index analyzers of `matched_fields` (same order). When set, the unified
    /// highlighter analyzes the values with each of them and finds that field's matches in
    /// its tokens; otherwise their terms match in the highlighted field's own tokens.
    pub matched_analyzers: Vec<Option<Analyzer>>,
    /// `index.highlight.max_analyzed_offset` (1 000 000).
    pub index_max_analyzed_offset: usize,
    /// The query's `max_analyzed_offset` (truncates instead of failing).
    pub max_analyzed_offset: Option<usize>,
    /// OpenSearch's offset limits (its field option `max_analyzer_offset`, carried in
    /// `max_analyzed_offset`): a limit above the index's fails outright, the plain highlighter
    /// cuts each value at the limit, and the errors use OpenSearch's texts.
    pub opensearch_offsets: bool,
    /// Whether the field is tokenized (keyword fields highlight whole values).
    pub tokenized: bool,
    /// For error messages: field name, doc id and index name.
    pub field_name: String,
    pub doc_id: String,
    pub index_name: String,
    /// Lucene's weight matches mode (`index.highlight.weight_matches_mode.enabled`): a
    /// phrase highlights as one span. Elasticsearch turns it off for `require_field_match:
    /// false` and for queries the matches API can't serve (knn, nested, joins, runtime fields).
    pub weight_matches: bool,
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
            offsets_indexed: false,
            fix_broken_analysis: false,
            require_field_match: true,
            matched_fields: Vec::new(),
            matched_analyzers: Vec::new(),
            index_max_analyzed_offset: 1_000_000,
            max_analyzed_offset: None,
            opensearch_offsets: false,
            tokenized: true,
            field_name: String::new(),
            doc_id: String::new(),
            index_name: String::new(),
            weight_matches: true,
        }
    }
}

/// OpenSearch's `max_analyzer_offset` rules (`PlainHighlighter` and
/// `CustomUnifiedHighlighter`): a field limit above `index.highlight.max_analyzed_offset`
/// fails; the plain highlighter cuts each value at the field limit (Java's `substring`, in
/// UTF-16 units) and otherwise fails on a value longer than the index limit; the unified
/// highlighter analyzes only up to the field limit and fails on a too long field only without
/// one (offsets from postings or term vectors lift the index limit, see the caller).
/// `None` when the unified highlighter may go ahead (the caller runs it, with any
/// `matched_fields`); the plain highlighter's result otherwise.
fn opensearch_limited(values: &[&str], analyzer: Option<&Analyzer>, terms: &HighlightTerms, opts: &HighlightOptions) -> Result<Option<Vec<String>>> {
    const SETTING: &str = "index.highlight.max_analyzed_offset";
    let max = opts.index_max_analyzed_offset;
    let field_max = opts.max_analyzed_offset;
    if let Some(l) = field_max
        && l > max
    {
        return Err(Error::illegal(format!(
            "max_analyzer_offset has exceeded [{max}] - maximum allowed to be analyzed for highlighting. This maximum can be set by changing the [{SETTING}] index level setting. For large texts, indexing with offsets or term vectors is recommended!"
        )));
    }
    if opts.kind == HighlighterType::Plain {
        let mut cut: Vec<&str> = Vec::with_capacity(values.len());
        for v in values {
            let len = v.encode_utf16().count();
            match field_max {
                Some(l) if len > l => cut.push(prefix_utf16(v, l)),
                None if len > max => {
                    return Err(Error::illegal(format!(
                        "The length of [{}] field of [{}] doc of [{}] index has exceeded [{max}] - maximum allowed to be analyzed for highlighting. This maximum can be set by changing the [{SETTING}] index level setting. For large texts, indexing with offsets or term vectors, and highlighting with unified or fvh highlighter is recommended!",
                        opts.field_name, opts.doc_id, opts.index_name
                    )));
                }
                _ => cut.push(v),
            }
        }
        return plain::highlight(&cut, analyzer, terms, opts).map(Some);
    }
    let total: usize = values.iter().map(|v| v.encode_utf16().count()).sum::<usize>() + values.len().saturating_sub(1);
    if field_max.is_none() && total > max && opts.tokenized {
        return Err(Error::illegal(format!(
            "The length of [{}] field of [{}] doc of [{}] index has exceeded [{max}] - maximum allowed to be analyzed for highlighting. This maximum can be set by changing the [{SETTING}] index level setting. For large texts, indexing with offsets or term vectors is recommended!",
            opts.field_name, opts.doc_id, opts.index_name
        )));
    }
    Ok(None)
}

/// The longest prefix of `s` of at most `units` UTF-16 code units (a surrogate pair the cut
/// would split is left out whole).
fn prefix_utf16(s: &str, units: usize) -> &str {
    let mut n = 0;
    for (i, c) in s.char_indices() {
        n += c.len_utf16();
        if n > units {
            return &s[..i];
        }
    }
    s
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
    /// Span queries: only their terms inside a match of the whole tree highlight
    /// ([`spans::leaves`]).
    pub spans: Vec<crate::query::SpanNode>,
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
            fn span(&mut self, node: &crate::query::SpanNode) {
                if self.ok(node.field()) {
                    self.out.spans.push(node.clone());
                }
            }
        }
        let mut v = V { fields, out: HighlightTerms::default() };
        query.visit(&mut v);
        v.out
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.multi.is_empty() && self.phrases.is_empty() && self.spans.is_empty()
    }

    /// The (position, term) occurrences of the span queries' matches in `tokens`.
    pub fn span_leaves(&self, tokens: &[HlToken]) -> HashSet<(u32, Vec<u8>)> {
        let mut out = HashSet::new();
        for n in &self.spans {
            out.extend(spans::leaves(n, tokens));
        }
        out
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

/// Match intervals in the tokens, in offset order (weight matches: phrases as one span;
/// without them, each term of a phrase match on its own).
///
/// `analysis`: offsets come from re-analysis (no offsets in postings or term vectors), where
/// Lucene's unified highlighter indexes only the tokens of terms the query extracts — so a
/// `span_not` exclude whose terms the query reads nowhere else excludes nothing.
pub fn find_matches(tokens: &[HlToken], q: &HighlightTerms, weight_matches: bool, analysis: bool) -> Vec<HlMatch> {
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
            if weight_matches {
                out.push(HlMatch { start: lo, end: hi, key: key.clone() });
                continue;
            }
            // Lucene's `PhraseHelper`: the phrase's terms inside the match, one by one.
            for t in tokens.iter().filter(|t| t.start >= lo && t.end <= hi && slots.iter().any(|(_, ts)| ts.contains(&t.term))) {
                out.push(HlMatch { start: t.start, end: t.end, key: t.term.clone() });
            }
        }
    }
    // Span queries: each term occurrence a match of the tree uses, on its own (Lucene's
    // sub-matches / `PhraseHelper`).
    if !q.spans.is_empty() {
        let extracted: Vec<HlToken>;
        let tokens = if analysis {
            extracted = tokens
                .iter()
                .filter(|t| {
                    q.terms.contains(&t.term)
                        || q.multi.iter().any(|m| m(&t.term))
                        || q.phrases.iter().any(|(slots, _)| slots.iter().any(|(_, ts)| ts.contains(&t.term)))
                        || q.spans.iter().any(|n| spans::extracts(n, &t.term))
                })
                .cloned()
                .collect();
            &extracted[..]
        } else {
            tokens
        };
        let leaves = q.span_leaves(tokens);
        for t in tokens.iter().filter(|t| leaves.contains(&(t.position, t.term.clone()))) {
            out.push(HlMatch { start: t.start, end: t.end, key: t.term.clone() });
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
    if opts.opensearch_offsets {
        if let Some(out) = opensearch_limited(values, analyzer, &terms, opts)? {
            return Ok(out);
        }
    } else {
        let total: usize = values.iter().map(|v| v.encode_utf16().count()).sum::<usize>() + values.len().saturating_sub(1);
        let limit = opts.max_analyzed_offset;
        if limit.is_none_or(|l| l > opts.index_max_analyzed_offset) && total > opts.index_max_analyzed_offset && opts.tokenized {
            return Err(Error::illegal(format!(
                "The length [{total}] of field [{}] in doc[{}]/index[{}] exceeds the [index.highlight.max_analyzed_offset] limit [{}]. To avoid this error, set the query parameter [max_analyzed_offset] to a value less than index setting [{}] and this will tolerate long field values by truncating them.",
                opts.field_name, opts.doc_id, opts.index_name, opts.index_max_analyzed_offset, opts.index_max_analyzed_offset
            )));
        }
    }
    if opts.kind == HighlighterType::Unified && opts.require_field_match && !opts.matched_analyzers.is_empty() {
        let own = HighlightTerms::of(query, Some(&[field]));
        let others: Vec<HighlightTerms> = opts.matched_fields.iter().map(|f| HighlightTerms::of(query, Some(std::slice::from_ref(f)))).collect();
        let mut sources: Vec<(Option<&Analyzer>, &HighlightTerms)> = vec![(analyzer, &own)];
        for (i, t) in others.iter().enumerate() {
            let a = opts.matched_analyzers.get(i).and_then(|a| a.as_ref()).or(analyzer);
            sources.push((a, t));
        }
        return unified::highlight_sources(values, &sources, opts);
    }
    match opts.kind {
        HighlighterType::Plain => plain::highlight(values, analyzer, &terms, opts),
        _ => unified::highlight(values, analyzer, &terms, opts),
    }
}

#[cfg(test)]
mod opensearch_offset_tests {
    use super::*;

    fn opts(kind: HighlighterType, index_max: usize, field_max: Option<usize>) -> HighlightOptions {
        HighlightOptions {
            kind,
            index_max_analyzed_offset: index_max,
            max_analyzed_offset: field_max,
            opensearch_offsets: true,
            field_name: "f".into(),
            doc_id: "1".into(),
            index_name: "i".into(),
            ..Default::default()
        }
    }

    #[test]
    fn utf16_prefix_never_splits_a_char() {
        assert_eq!(prefix_utf16("abcdef", 3), "abc");
        assert_eq!(prefix_utf16("abc", 10), "abc");
        assert_eq!(prefix_utf16("", 0), "");
        // U+1F600 is two UTF-16 units: a cut through it leaves it out.
        assert_eq!(prefix_utf16("a\u{1F600}b", 2), "a");
        assert_eq!(prefix_utf16("a\u{1F600}b", 3), "a\u{1F600}");
        assert_eq!(prefix_utf16("é漢x", 2), "é漢");
    }

    #[test]
    fn field_limit_above_the_index_limit_fails() {
        let terms = HighlightTerms::default();
        for kind in [HighlighterType::Plain, HighlighterType::Unified] {
            let e = opensearch_limited(&["short"], None, &terms, &opts(kind, 10, Some(11))).unwrap_err();
            assert!(e.to_string().contains("max_analyzer_offset has exceeded [10] - maximum allowed to be analyzed for highlighting."), "{e}");
        }
    }

    #[test]
    fn plain_without_a_field_limit_fails_on_a_long_value() {
        let terms = HighlightTerms::default();
        let e = opensearch_limited(&["0123456789ab"], None, &terms, &opts(HighlighterType::Plain, 10, None)).unwrap_err();
        assert!(e.to_string().contains("The length of [f] field of [1] doc of [i] index has exceeded [10]"), "{e}");
        assert!(e.to_string().ends_with("highlighting with unified or fvh highlighter is recommended!"), "{e}");
        // Exactly the limit is fine.
        assert!(opensearch_limited(&["0123456789"], None, &terms, &opts(HighlighterType::Plain, 10, None)).is_ok());
    }

    #[test]
    fn unified_with_a_field_limit_tolerates_long_values() {
        let terms = HighlightTerms::default();
        // The unified highlighter itself is left to the caller (which adds `matched_fields`).
        assert!(opensearch_limited(&["0123456789abcdef"], None, &terms, &opts(HighlighterType::Unified, 10, Some(10))).unwrap().is_none());
        // The plain one is run here, on the cut values.
        assert!(opensearch_limited(&["0123456789abcdef"], None, &terms, &opts(HighlighterType::Plain, 10, Some(10))).unwrap().is_some());
        let mut o = opts(HighlighterType::Unified, 10, None);
        let e = opensearch_limited(&["0123456789abcdef"], None, &terms, &o).unwrap_err();
        assert!(e.to_string().ends_with("For large texts, indexing with offsets or term vectors is recommended!"), "{e}");
        // Untokenized fields are never re-analyzed.
        o.tokenized = false;
        assert!(opensearch_limited(&["0123456789abcdef"], None, &terms, &o).is_ok());
    }
}
