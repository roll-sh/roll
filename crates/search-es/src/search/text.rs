//! Analyzed queries: Lucene's `QueryBuilder` with Elasticsearch's `MatchQueryParser` and
//! `MultiMatchQueryParser` on top — `match`, `match_phrase`, `match_phrase_prefix`,
//! `match_bool_prefix`, `multi_match` (every type), `combined_fields`.
//!
//! The query text is analyzed with the field's search (or quote) analyzer into positions:
//! one term gives a term query, stacked terms a synonym query, several positions a boolean
//! query (`operator`) or a phrase (`slop`, position increments kept). Fuzziness turns terms
//! into fuzzy queries; `lenient` turns type errors into match-none; an empty analysis gives
//! `zero_terms_query`.

use super::query::{QueryCtx, arc, calculate_msm, fuzzy_edits, index_err, match_none, normalize, value_text};
use crate::engine::fields::{FieldInfo, Kind};
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::query::full_text::{FuzzyOpts, Match, MatchBoolPrefix, MatchPhrase, MatchPhrasePrefix, MultiMatch, MultiMatchType, Operator, ZeroTerms};
use crate::query::term_level::Fuzziness;
use search_analysis::Analyzer;
use search_index::query::{
    BooleanQuery, BoostQuery, CombinedFieldQuery, DisMaxQuery, MatchAllQuery, MultiPhrasePrefixQuery, MultiPhraseQuery, MultiTermQuery, Occur,
    PhraseQuery, RewriteMethod, SynonymQuery, TermQuery,
};
use search_index::QueryRef;
use std::sync::Arc;

/// An analyzed token.
#[derive(Clone, Debug)]
pub struct Tok {
    pub term: String,
    /// Absolute position (increments applied).
    pub pos: u32,
    pub pos_len: u32,
}

/// Analyzes `text`; positions start at 0.
pub fn analyze(a: &Analyzer, text: &str) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::new();
    let mut pos: i64 = -1;
    a.analyze(text, &mut |t| {
        pos += t.position_increment.max(if out.is_empty() { 1 } else { 0 }) as i64;
        out.push(Tok { term: t.term.to_string(), pos: pos.max(0) as u32, pos_len: t.position_length.max(1) });
    });
    out
}

/// How a match runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MatchType {
    Boolean,
    BooleanPrefix,
    Phrase,
    PhrasePrefix,
}

/// Options of one field query.
#[derive(Clone, Debug)]
pub struct MatchOpts<'o> {
    pub operator: Operator,
    pub analyzer: Option<&'o str>,
    pub fuzzy: Option<&'o FuzzyOpts>,
    pub slop: i32,
    pub max_expansions: i32,
    pub lenient: bool,
    pub zero_terms: ZeroTerms,
    pub auto_generate_synonyms_phrase_query: bool,
}

impl Default for MatchOpts<'_> {
    fn default() -> Self {
        MatchOpts {
            operator: Operator::Or,
            analyzer: None,
            fuzzy: None,
            slop: 0,
            max_expansions: 50,
            lenient: false,
            zero_terms: ZeroTerms::None,
            auto_generate_synonyms_phrase_query: true,
        }
    }
}

fn zero_terms_query(z: ZeroTerms) -> QueryRef {
    match z {
        ZeroTerms::None => match_none("Matching no documents because no terms present"),
        ZeroTerms::All => arc(MatchAllQuery),
    }
}

/// The analyzer named in a query (`[match] analyzer [x] not found`).
fn named_analyzer(ctx: &QueryCtx, name: &str, query: &str) -> EsResult<Analyzer> {
    if name == "default" {
        return Ok(ctx.table.analysis.default_analyzer());
    }
    ctx.table.analysis.analyzer(name).ok_or_else(|| EsError::new(400, "query_shard_exception", format!("[{query}] analyzer [{name}] not found")).with_meta("index_uuid", ctx.idx.uuid.as_str()).with_meta("index", ctx.idx.name.as_str()))
}

fn has_text_search(f: &FieldInfo) -> bool {
    !matches!(
        f.kind,
        Kind::GeoPoint | Kind::GeoShape | Kind::Point | Kind::Shape | Kind::DenseVector | Kind::Binary | Kind::RankFeature | Kind::Histogram | Kind::AggregateMetric | Kind::Percolator
    ) && !f.kind.is_range()
}

/// `MatchQueryParser.parse(type, field, value)`.
pub fn field_query(ctx: &QueryCtx, field: &str, value: &Value, ty: MatchType, o: &MatchOpts, query_name: &str) -> EsResult<Option<QueryRef>> {
    // Runtime fields (which shadow mapped ones) have no analysis: the text is one term.
    if let Some(rf) = ctx.runtime_target(field, query_name)? {
        return ctx.runtime_match(rf, value, ty, o.lenient).map(Some);
    }
    // The `match*` builders check a named analyzer before looking the field up.
    if let Some(n) = o.analyzer
        && ctx.field(field).is_none()
    {
        named_analyzer(ctx, n, query_name)?;
    }
    let Some(f) = ctx.field(field) else {
        // Keyword-like metadata fields match their whole value (`TermBasedFieldType`).
        if matches!(field, "_id" | "_index" | "_routing" | "_ignored" | "_tier") && matches!(ty, MatchType::Boolean | MatchType::Phrase) {
            return Ok(Some(ctx.unmapped_or_meta_term(field, std::slice::from_ref(value), false, "term")?));
        }
        // `Queries.newUnmappedFieldsQuery`.
        return Ok(Some(match_none(&format!("unmapped fields [{field}]"))));
    };
    if !has_text_search(&f) {
        let e = EsError::illegal_argument(format!("Field [{}] of type [{}] does not support match queries", f.name, f.ty));
        if o.lenient {
            return Ok(Some(lenient_none(&f.name, &e)));
        }
        return Err(e);
    }
    let text = value_text(value);
    // `CompletionFieldType` is term based, searched through its `CompletionAnalyzer`: the whole
    // text becomes one token, the analyzed tokens joined like the suggester's keys (which the
    // field also indexes as terms).
    if f.kind == Kind::Completion && o.analyzer.is_none() {
        let not_text = |what: &str| {
            let e = EsError::illegal_argument(format!("Can only use {what} queries on text fields - not on [{}] which is of type [{}]", f.name, f.ty));
            if o.lenient { Ok(Some(lenient_none(&f.name, &e))) } else { Err(e) }
        };
        match ty {
            MatchType::PhrasePrefix => return not_text("phrase prefix"),
            MatchType::BooleanPrefix => return ctx.prefix_query(&f.name, &text, false, None).map(Some),
            MatchType::Boolean | MatchType::Phrase => {}
        }
        let key = crate::engine::completion::analyzed_key(&f, &text, f.search_analyzer.as_ref().or(f.analyzer.as_ref()));
        if key.is_empty() {
            return Ok(None);
        }
        return term_or_lenient(ctx, &f, &Value::Str(String::from_utf8_lossy(&key).into_owned()), o).map(Some);
    }
    let quoted = matches!(ty, MatchType::Phrase | MatchType::PhrasePrefix);
    // Fields with no analysis (keyword-like, numbers, dates…): one term.
    let analyzer: Option<Analyzer> = match o.analyzer {
        Some(n) => Some(named_analyzer(ctx, n, query_name)?),
        None => {
            if f.kind.is_text() {
                if quoted { f.quote_analyzer().cloned() } else { f.query_analyzer().cloned() }
            } else {
                f.whitespace_analyzer.clone()
            }
        }
    };
    let Some(analyzer) = analyzer else {
        // `version` fields have no phrase support (`MappedFieldType.phrasePrefixQuery`).
        if ty == MatchType::PhrasePrefix && f.kind == Kind::Version {
            return Err(EsError::illegal_argument(format!("Can only use phrase prefix queries on text fields - not on [{}] which is of type [version]", f.name)));
        }
        // `MatchQueryParser`: a keyword-analyzed `bool_prefix` is a prefix query only on text
        // and keyword fields; any other type gets a term query.
        if ty == MatchType::BooleanPrefix && f.kind == Kind::Version {
            return term_or_lenient(ctx, &f, value, o).map(Some);
        }
        if ty == MatchType::PhrasePrefix && f.kind.is_keyword_like() {
            let t = normalize(&f, &text);
            return Ok(Some(arc(MultiTermQuery::prefix(f.number, t.into_bytes()))));
        }
        if ty == MatchType::BooleanPrefix && f.kind.is_keyword_like() {
            let t = normalize(&f, &text);
            return Ok(Some(arc(MultiTermQuery::prefix(f.number, t.into_bytes()))));
        }
        return term_or_lenient(ctx, &f, value, o).map(Some);
    };
    let toks = analyze(&analyzer, &text);
    if toks.is_empty() {
        return Ok(None);
    }
    let q = match ty {
        MatchType::Boolean => boolean(ctx, &f, &toks, o)?,
        MatchType::BooleanPrefix => {
            // `analyzeTerm` / `analyzeMultiBoolean`: the last term is a prefix only when it ends
            // where the analyzed text does (not before trailing punctuation or stop words).
            let mut last_end = 0;
            analyzer.analyze(&text, &mut |t| last_end = t.end_offset);
            let prefix_last = last_end as usize == text.encode_utf16().count();
            boolean_prefix(ctx, &f, &toks, o, prefix_last)?
        }
        MatchType::Phrase => phrase(ctx, &f, &toks, o.slop, o.auto_generate_synonyms_phrase_query)?,
        MatchType::PhrasePrefix => phrase_prefix(ctx, &f, &toks, o.slop, o.max_expansions)?,
    };
    Ok(Some(q))
}

/// `Queries.newLenientFieldQuery`: what a field that can't take the value becomes.
pub fn lenient_none(field: &str, e: &EsError) -> QueryRef {
    match_none(&format!("failed [{field}] query, caused by {}:[{}]", e.kind, e.reason()))
}

fn term_or_lenient(ctx: &QueryCtx, f: &FieldInfo, value: &Value, o: &MatchOpts) -> EsResult<QueryRef> {
    let r = match o.fuzzy.and_then(|fz| fz.fuzziness.as_ref().map(|z| (fz, z))) {
        Some((fz, z)) => fuzzy_term(ctx, f, &value_text(value), z, fz),
        None => ctx.term_query(f, value, false),
    };
    match r {
        Ok(q) => Ok(q),
        Err(e) if o.lenient => Ok(lenient_none(&f.name, &e)),
        Err(e) => Err(e),
    }
}

fn fuzzy_term(ctx: &QueryCtx, f: &FieldInfo, term: &str, z: &Fuzziness, fz: &FuzzyOpts) -> EsResult<QueryRef> {
    if !f.kind.is_text() && !f.kind.is_keyword_like() {
        if f.kind.is_numeric() || f.kind.is_date() {
            return super::range::numeric_fuzzy(ctx, f, &Value::Str(term.to_string()), z);
        }
        return Err(EsError::illegal_argument(format!("Can only use fuzzy queries on keyword and text fields - not on [{}] which is of type [{}]", f.name, f.ty)));
    }
    let t = if f.kind.is_text() { term.to_string() } else { normalize(f, term) };
    let edits = fuzzy_edits(z, &t);
    if f.kind == Kind::Version {
        // `VersionStringFieldType.fuzzyQuery` casts its value to a `BytesRef`; the match query
        // passes a `String`.
        return Err(EsError::java(
            400,
            "class_cast_exception",
            "class java.lang.String cannot be cast to class org.apache.lucene.util.BytesRef (java.lang.String is in module java.base of loader 'bootstrap'; org.apache.lucene.util.BytesRef is in module org.apache.lucene.core@10.5.1 of loader 'app')",
        ));
    }
    let mut q = MultiTermQuery::fuzzy(f.number, &t, edits, fz.prefix_length.max(0) as usize, fz.max_expansions.max(1) as usize, fz.fuzzy_transpositions);
    if let Some(r) = &fz.fuzzy_rewrite
        && let Some(m) = RewriteMethod::parse(r)
    {
        q = q.with_rewrite(m);
    }
    Ok(unscored_if_match_only(f, arc(q)))
}

/// `MatchOnlyTextFieldType` disables scoring of its term and fuzzy queries
/// (`ConstantScoreQuery`): the field keeps no frequencies or norms worth scoring with.
fn unscored_if_match_only(f: &FieldInfo, q: QueryRef) -> QueryRef {
    if f.kind == Kind::MatchOnlyText { arc(search_index::query::ConstantScoreQuery::new(q)) } else { q }
}

/// One analyzed term: a term query, or a fuzzy one.
fn term_q(ctx: &QueryCtx, f: &FieldInfo, term: &str, o: &MatchOpts) -> EsResult<QueryRef> {
    if let Some(fz) = o.fuzzy
        && let Some(z) = &fz.fuzziness
    {
        return fuzzy_term(ctx, f, term, z, fz);
    }
    Ok(unscored_if_match_only(f, crate::opensearch::keyword_term(f, arc(TermQuery::new(f.number, term.as_bytes().to_vec())))))
}

/// Groups tokens by position.
fn positions(toks: &[Tok]) -> Vec<(u32, Vec<&Tok>)> {
    let mut out: Vec<(u32, Vec<&Tok>)> = Vec::new();
    for t in toks {
        match out.last_mut() {
            Some((p, v)) if *p == t.pos => v.push(t),
            _ => out.push((t.pos, vec![t])),
        }
    }
    out
}

fn synonym_q(ctx: &QueryCtx, f: &FieldInfo, terms: &[&Tok], o: &MatchOpts) -> EsResult<QueryRef> {
    if let Some(fz) = o.fuzzy
        && fz.fuzziness.is_some()
    {
        let mut b = BooleanQuery::new();
        for t in terms {
            b = b.add(Occur::Should, term_q(ctx, f, &t.term, o)?);
        }
        return Ok(arc(b));
    }
    let mut v: Vec<Vec<u8>> = terms.iter().map(|t| t.term.as_bytes().to_vec()).collect();
    v.dedup();
    if v.len() == 1 {
        return Ok(crate::opensearch::keyword_term(f, arc(TermQuery::new(f.number, v.remove(0)))));
    }
    Ok(arc(SynonymQuery::new(f.number, v)))
}

fn is_graph(toks: &[Tok]) -> bool {
    toks.iter().any(|t| t.pos_len > 1)
}

/// `createBooleanQuery`.
fn boolean(ctx: &QueryCtx, f: &FieldInfo, toks: &[Tok], o: &MatchOpts) -> EsResult<QueryRef> {
    if toks.len() == 1 {
        return term_q(ctx, f, &toks[0].term, o);
    }
    if is_graph(toks) {
        return graph_boolean(ctx, f, toks, o);
    }
    let groups = positions(toks);
    if groups.len() == 1 {
        return synonym_q(ctx, f, &groups[0].1, o);
    }
    let occur = if o.operator == Operator::And { Occur::Must } else { Occur::Should };
    let mut b = BooleanQuery::new();
    for (_, g) in &groups {
        let q = if g.len() == 1 { term_q(ctx, f, &g[0].term, o)? } else { synonym_q(ctx, f, g, o)? };
        b = b.add(occur, q);
    }
    Ok(arc(b))
}

/// Token graphs (multi-term synonyms): split at articulation points; each side path becomes a
/// phrase (or a conjunction), alternatives a disjunction (`analyzeGraphBoolean`).
fn graph_boolean(ctx: &QueryCtx, f: &FieldInfo, toks: &[Tok], o: &MatchOpts) -> EsResult<QueryRef> {
    let occur = if o.operator == Operator::And { Occur::Must } else { Occur::Should };
    let mut b = BooleanQuery::new();
    for seg in graph_segments(toks) {
        let paths = graph_paths(&seg);
        let q = if paths.len() == 1 && paths[0].len() == 1 {
            term_q(ctx, f, &paths[0][0], o)?
        } else if paths.iter().all(|p| p.len() == 1) {
            let refs: Vec<Tok> = paths.iter().map(|p| Tok { term: p[0].clone(), pos: 0, pos_len: 1 }).collect();
            let r: Vec<&Tok> = refs.iter().collect();
            synonym_q(ctx, f, &r, o)?
        } else {
            let mut alts = BooleanQuery::new();
            for p in &paths {
                let q: QueryRef = if p.len() == 1 {
                    term_q(ctx, f, &p[0], o)?
                } else if o.auto_generate_synonyms_phrase_query && f.has_positions() {
                    arc(PhraseQuery::new(f.number, p.iter().map(|t| t.as_bytes().to_vec()).collect(), 0))
                } else {
                    let mut c = BooleanQuery::new();
                    for t in p {
                        c = c.add(Occur::Must, term_q(ctx, f, t, o)?);
                    }
                    arc(c)
                };
                alts = alts.add(Occur::Should, q);
            }
            arc(alts)
        };
        b = b.add(occur, q);
    }
    Ok(arc(b))
}

/// Splits a token graph at positions every path crosses.
fn graph_segments(toks: &[Tok]) -> Vec<Vec<Tok>> {
    let mut segs: Vec<Vec<Tok>> = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();
    let mut reach = 0u32;
    for t in toks {
        if !cur.is_empty() && t.pos >= reach {
            segs.push(std::mem::take(&mut cur));
        }
        reach = reach.max(t.pos + t.pos_len);
        cur.push(t.clone());
    }
    if !cur.is_empty() {
        segs.push(cur);
    }
    segs
}

/// Every path through a graph segment, as term lists.
fn graph_paths(seg: &[Tok]) -> Vec<Vec<String>> {
    let start = seg.iter().map(|t| t.pos).min().unwrap_or(0);
    let end = seg.iter().map(|t| t.pos + t.pos_len).max().unwrap_or(0);
    let mut out = Vec::new();
    fn walk(seg: &[Tok], at: u32, end: u32, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        if at >= end {
            out.push(path.clone());
            return;
        }
        if out.len() > 64 {
            return;
        }
        let mut any = false;
        for t in seg.iter().filter(|t| t.pos == at) {
            any = true;
            path.push(t.term.clone());
            walk(seg, at + t.pos_len, end, path, out);
            path.pop();
        }
        if !any {
            walk(seg, at + 1, end, path, out);
        }
    }
    walk(seg, start, end, &mut Vec::new(), &mut out);
    out
}

fn check_positions(f: &FieldInfo, what: &str) -> EsResult<()> {
    if !f.has_positions() && f.kind != Kind::MatchOnlyText {
        return Err(EsError::illegal_state(format!("field:[{}] was indexed without position data; cannot run {what}", f.name)));
    }
    Ok(())
}

/// `createPhraseQuery`.
fn phrase(ctx: &QueryCtx, f: &FieldInfo, toks: &[Tok], slop: i32, auto_syn: bool) -> EsResult<QueryRef> {
    let _ = ctx;
    if toks.len() == 1 {
        return Ok(unscored_if_match_only(f, crate::opensearch::keyword_term(f, arc(TermQuery::new(f.number, toks[0].term.as_bytes().to_vec())))));
    }
    let groups = positions(toks);
    if groups.len() == 1 {
        let o = MatchOpts::default();
        return synonym_q(ctx, f, &groups[0].1, &o);
    }
    if is_graph(toks) {
        let o = MatchOpts { operator: Operator::And, auto_generate_synonyms_phrase_query: auto_syn, ..Default::default() };
        return graph_boolean(ctx, f, toks, &o);
    }
    // `TextFieldType.phraseQuery` / `multiPhraseQuery`.
    check_positions(f, if groups.iter().any(|(_, g)| g.len() > 1) { "MultiPhraseQuery" } else { "PhraseQuery" })?;
    // `index_phrases`: exact two-term shingles.
    if let Some(pf) = f.phrase_field
        && slop == 0
        && groups.iter().all(|(_, g)| g.len() == 1)
        && groups.windows(2).all(|w| w[1].0 == w[0].0 + 1)
    {
        let words: Vec<&str> = groups.iter().map(|(_, g)| g[0].term.as_str()).collect();
        let shingles: Vec<Vec<u8>> = words.windows(2).map(|w| format!("{} {}", w[0], w[1]).into_bytes()).collect();
        if shingles.len() == 1 {
            return Ok(arc(TermQuery::new(pf, shingles[0].clone())));
        }
        let pos: Vec<u32> = (0..shingles.len() as u32).collect();
        return Ok(arc(PhraseQuery::with_positions(pf, shingles, pos, 0).map_err(index_err)?));
    }
    let base = groups[0].0;
    if groups.iter().all(|(_, g)| g.len() == 1) {
        let terms: Vec<Vec<u8>> = groups.iter().map(|(_, g)| g[0].term.as_bytes().to_vec()).collect();
        let pos: Vec<u32> = groups.iter().map(|(p, _)| p - base).collect();
        let q = arc(PhraseQuery::with_positions(f.number, terms, pos, slop.max(0) as u32).map_err(index_err)?);
        return Ok(confirm_match_only(ctx, f, q));
    }
    let terms: Vec<Vec<Vec<u8>>> = groups.iter().map(|(_, g)| g.iter().map(|t| t.term.as_bytes().to_vec()).collect()).collect();
    let pos: Vec<u32> = groups.iter().map(|(p, _)| p - base).collect();
    let q = arc(MultiPhraseQuery::new(f.number, terms, pos, slop.max(0) as u32).map_err(index_err)?);
    Ok(confirm_match_only(ctx, f, q))
}

/// `match_only_text` phrases are checked against the re-analyzed `_source`.
fn confirm_match_only(ctx: &QueryCtx, f: &FieldInfo, q: QueryRef) -> QueryRef {
    if f.kind != Kind::MatchOnlyText {
        return q;
    }
    super::extra::source_confirmed(ctx, f, q)
}

/// `createPhrasePrefixQuery`.
fn phrase_prefix(ctx: &QueryCtx, f: &FieldInfo, toks: &[Tok], slop: i32, max_expansions: i32) -> EsResult<QueryRef> {
    let _ = ctx;
    let groups = positions(toks);
    if groups.len() == 1 && groups[0].1.len() == 1 {
        // One term: a prefix query (`index_prefixes` fields use their prefix field).
        let t = &groups[0].1[0].term;
        if let Some((pf, min, max)) = f.prefix_field {
            let len = t.chars().count();
            if len >= min && len <= max {
                return Ok(arc(search_index::query::ConstantScoreQuery::new(arc(TermQuery::new(pf, t.as_bytes().to_vec())))));
            }
        }
        let q: QueryRef = arc(MultiTermQuery::prefix(f.number, t.as_bytes().to_vec()).with_rewrite(RewriteMethod::TopTermsBlendedFreqs(max_expansions.max(1) as usize)));
        let _ = q;
        let mp = MultiPhrasePrefixQuery::new(f.number, vec![vec![t.as_bytes().to_vec()]], vec![0], slop.max(0) as u32, max_expansions.max(1) as usize).map_err(index_err)?;
        return Ok(arc(mp));
    }
    if is_graph(toks) {
        return graph_phrase_prefix(f, toks, slop, max_expansions);
    }
    // `TextFieldType.phrasePrefixQuery` names the plain phrase query.
    check_positions(f, "PhraseQuery")?;
    let base = groups[0].0;
    let terms: Vec<Vec<Vec<u8>>> = groups.iter().map(|(_, g)| g.iter().map(|t| t.term.as_bytes().to_vec()).collect()).collect();
    let pos: Vec<u32> = groups.iter().map(|(p, _)| p - base).collect();
    let q = arc(MultiPhrasePrefixQuery::new(f.number, terms, pos, slop.max(0) as u32, max_expansions.max(1) as usize).map_err(index_err)?);
    Ok(confirm_match_only(ctx, f, q))
}

/// `analyzeGraphPhrase` for `match_phrase_prefix`: Elasticsearch joins the graph's
/// articulation points with a span near query whose last clause is a span prefix. Every path
/// through the graph as a phrase whose last term is a prefix, any of them matching, finds the
/// same documents.
fn graph_phrase_prefix(f: &FieldInfo, toks: &[Tok], slop: i32, max_expansions: i32) -> EsResult<QueryRef> {
    const MAX_CLAUSES: usize = 4096;
    if slop <= 0 {
        return graph_span_phrase(f, toks, true);
    }
    let mut full: Vec<Vec<String>> = vec![Vec::new()];
    for seg in graph_segments(toks) {
        let paths = graph_paths(&seg);
        let mut next = Vec::with_capacity(full.len() * paths.len());
        for head in &full {
            for p in &paths {
                if next.len() >= MAX_CLAUSES {
                    return Err(EsError::new(400, "too_many_nested_clauses", format!("Query contains too many nested clauses; maxClauseCount is set to {MAX_CLAUSES}")));
                }
                let mut v = head.clone();
                v.extend(p.iter().cloned());
                next.push(v);
            }
        }
        full = next;
    }
    let mut alts: Vec<QueryRef> = Vec::with_capacity(full.len());
    for path in full.into_iter().filter(|p| !p.is_empty()) {
        let terms: Vec<Vec<Vec<u8>>> = path.iter().map(|t| vec![t.as_bytes().to_vec()]).collect();
        let pos: Vec<u32> = (0..terms.len() as u32).collect();
        alts.push(arc(MultiPhrasePrefixQuery::new(f.number, terms, pos, slop.max(0) as u32, max_expansions.max(1) as usize).map_err(index_err)?));
    }
    if alts.len() == 1 {
        return Ok(alts.remove(0));
    }
    let mut b = BooleanQuery::new();
    for q in alts {
        b = b.add(Occur::Should, q);
    }
    Ok(arc(b))
}

/// `analyzeGraphPhrase` without slop: a span near query over the graph's articulation
/// points, each a span or of its paths (`createSpanQuery`) or of the terms at that position
/// (`newSpanQuery`); with `prefix`, the last point's final terms are span prefixes.
fn graph_span_phrase(f: &FieldInfo, toks: &[Tok], prefix: bool) -> EsResult<QueryRef> {
    use search_index::query::{SpanNode, SpanQuery};
    let leaf = |t: &str, pre: bool| -> SpanNode {
        if pre {
            SpanNode::Multi(Box::new(MultiTermQuery::prefix(f.number, t.as_bytes().to_vec())))
        } else {
            SpanNode::Term { field: f.number, term: t.as_bytes().to_vec() }
        }
    };
    let segs = graph_segments(toks);
    let last = segs.len().saturating_sub(1);
    let mut clauses: Vec<SpanNode> = Vec::with_capacity(segs.len());
    for (i, seg) in segs.iter().enumerate() {
        let pre = prefix && i == last;
        let paths = graph_paths(seg);
        let side_path = paths.iter().any(|p| p.len() > 1);
        let node = if !side_path {
            let mut terms: Vec<SpanNode> = paths.iter().filter_map(|p| p.first()).map(|t| leaf(t, pre)).collect();
            if terms.len() == 1 { terms.remove(0) } else { SpanNode::Or(terms) }
        } else {
            let mut alts: Vec<SpanNode> = Vec::with_capacity(paths.len());
            for p in &paths {
                let n = p.len();
                let mut parts: Vec<SpanNode> = p.iter().enumerate().map(|(j, t)| leaf(t, pre && j + 1 == n)).collect();
                alts.push(if parts.len() == 1 { parts.remove(0) } else { SpanNode::Near { clauses: parts, slop: 0, in_order: true } });
            }
            if alts.len() == 1 { alts.remove(0) } else { SpanNode::Or(alts) }
        };
        clauses.push(node);
    }
    let node = if clauses.len() == 1 { clauses.remove(0) } else { SpanNode::Near { clauses, slop: 0, in_order: true } };
    Ok(arc(SpanQuery::new(node).map_err(index_err)?))
}

/// `createBooleanPrefixQuery`: terms, the last one a prefix (when `prefix_last`; synonyms at
/// the last position stay a synonym query, Elasticsearch applies no prefix to them).
fn boolean_prefix(ctx: &QueryCtx, f: &FieldInfo, toks: &[Tok], o: &MatchOpts, prefix_last: bool) -> EsResult<QueryRef> {
    let groups = positions(toks);
    let prefix = |t: &str| -> QueryRef {
        if let Some((pf, min, max)) = f.prefix_field {
            let len = t.chars().count();
            if len >= min && len <= max {
                return arc(search_index::query::ConstantScoreQuery::new(arc(TermQuery::new(pf, t.as_bytes().to_vec()))));
            }
        }
        arc(MultiTermQuery::prefix(f.number, t.as_bytes().to_vec()))
    };
    if groups.len() == 1 && groups[0].1.len() == 1 {
        return if prefix_last { Ok(prefix(&groups[0].1[0].term)) } else { term_q(ctx, f, &groups[0].1[0].term, o) };
    }
    let occur = if o.operator == Operator::And { Occur::Must } else { Occur::Should };
    let mut b = BooleanQuery::new();
    let last = groups.len() - 1;
    for (i, (_, g)) in groups.iter().enumerate() {
        let q = if i == last && prefix_last && g.len() == 1 {
            prefix(&g[0].term)
        } else if g.len() == 1 {
            term_q(ctx, f, &g[0].term, o)?
        } else {
            synonym_q(ctx, f, g, o)?
        };
        b = b.add(occur, q);
    }
    Ok(arc(b))
}

/// `Queries.maybeApplyMinimumShouldMatch`.
pub fn apply_msm(q: QueryRef, msm: Option<&str>) -> EsResult<QueryRef> {
    let Some(msm) = msm else { return Ok(q) };
    let Some(b) = q.as_any().downcast_ref::<BooleanQuery>() else { return Ok(q) };
    // Applied with no optional clauses too: an integer `minimum_should_match` on
    // `operator: and` (all clauses required) makes the query match nothing, as in Elasticsearch.
    let optional = b.clauses.iter().filter(|c| c.occur == Occur::Should).count() as i32;
    let n = calculate_msm(optional, msm)?;
    if n <= 0 {
        return Ok(q);
    }
    let mut nb = b.clone();
    nb.minimum_should_match = n as usize;
    Ok(arc(nb))
}

pub fn match_query(ctx: &QueryCtx, m: &Match) -> EsResult<QueryRef> {
    let o = MatchOpts {
        operator: m.operator,
        analyzer: m.analyzer.as_deref(),
        fuzzy: Some(&m.fuzzy),
        lenient: m.lenient,
        zero_terms: m.zero_terms_query,
        auto_generate_synonyms_phrase_query: m.auto_generate_synonyms_phrase_query,
        ..Default::default()
    };
    let q = match field_query(ctx, &m.field, &m.query, MatchType::Boolean, &o, "match")? {
        Some(q) => apply_msm(q, m.minimum_should_match.as_deref())?,
        None => zero_terms_query(m.zero_terms_query),
    };
    // `MatchQueryBuilder.doIndexMetadataRewrite`: neither fuzzy nor lenient, with a keyword
    // analyzer (a constant field's), the query is rewritten to a `term` query first.
    if m.fuzzy.fuzziness.is_none() && !m.lenient && m.analyzer.as_deref().is_none_or(|a| a == "keyword")
        && let Some(f) = ctx.field(&m.field)
    {
        return Ok(super::query::constant_rewritten(&f, q));
    }
    Ok(q)
}

pub fn match_phrase(ctx: &QueryCtx, m: &MatchPhrase) -> EsResult<QueryRef> {
    let o = MatchOpts { analyzer: m.analyzer.as_deref(), slop: m.slop, zero_terms: m.zero_terms_query, ..Default::default() };
    Ok(field_query(ctx, &m.field, &m.query, MatchType::Phrase, &o, "match_phrase")?.unwrap_or_else(|| zero_terms_query(m.zero_terms_query)))
}

pub fn match_phrase_prefix(ctx: &QueryCtx, m: &MatchPhrasePrefix) -> EsResult<QueryRef> {
    let o = MatchOpts { analyzer: m.analyzer.as_deref(), slop: m.slop, max_expansions: m.max_expansions, zero_terms: m.zero_terms_query, ..Default::default() };
    Ok(field_query(ctx, &m.field, &m.query, MatchType::PhrasePrefix, &o, "match_phrase_prefix")?.unwrap_or_else(|| zero_terms_query(m.zero_terms_query)))
}

pub fn match_bool_prefix(ctx: &QueryCtx, m: &MatchBoolPrefix) -> EsResult<QueryRef> {
    let o = MatchOpts { operator: m.operator, analyzer: m.analyzer.as_deref(), fuzzy: Some(&m.fuzzy), ..Default::default() };
    match field_query(ctx, &m.field, &m.query, MatchType::BooleanPrefix, &o, "match_bool_prefix")? {
        Some(q) => apply_msm(q, m.minimum_should_match.as_deref()),
        None => Ok(zero_terms_query(ZeroTerms::None)),
    }
}

/// `QueryParserHelper.resolveMappingFields`: patterns expand to text-searchable fields.
pub fn resolve_fields(ctx: &QueryCtx, fields: &[(String, f32)]) -> Vec<(String, f32)> {
    let mut out: Vec<(String, f32)> = Vec::new();
    // Strict columnar modes expand `*` to indexed fields only (doc-values-only columns would
    // turn every default-field query into column scans).
    let columnar = fields.iter().any(|(p, _)| p == "*") && crate::meta::index_mode::IndexMode::of(&ctx.idx.settings).is_strict_columnar();
    for (pat, boost) in fields {
        if pat.contains('*') {
            let all = pat == "*";
            for name in ctx.table.matching(pat) {
                let Some(f) = ctx.field(&name) else { continue };
                if !has_text_search(&f) || (all && f.name.starts_with('_')) {
                    continue;
                }
                if (!f.index && !f.doc_values) || (all && columnar && !f.index) {
                    continue;
                }
                if !may_exist_in_index(ctx, &f) {
                    continue;
                }
                match out.iter_mut().find(|(n, _)| *n == name) {
                    Some(e) => e.1 *= boost,
                    None => out.push((name, *boost)),
                }
            }
        } else {
            match out.iter_mut().find(|(n, _)| n == pat) {
                Some(e) => e.1 *= boost,
                None => out.push((pat.clone(), *boost)),
            }
        }
    }
    out
}

/// `MappedFieldType.mayExistInIndex`, which pattern expansion consults: a mapped field exists
/// only once a segment of the shard holds it (`SearchExecutionContext.fieldExistsInIndex`,
/// the segments' field infos). Every indexed field type overrides it so; `constant_keyword`
/// and runtime fields keep the default (always may).
fn may_exist_in_index(ctx: &QueryCtx, f: &FieldInfo) -> bool {
    // Only mapped fields have field infos (runtime fields always may exist).
    if f.kind == Kind::ConstantKeyword || ctx.table.by_number.get(&f.number).is_none_or(|m| m.name != f.name) {
        return true;
    }
    ctx.searcher.segments().iter().any(|s| s.reader.field_info(f.number).is_some())
}

/// Resolved fields in the iteration order of the `HashMap` `QueryParserHelper` collects them
/// into, which orders per-field queries.
pub fn hash_ordered(fields: Vec<(String, f32)>) -> Vec<(String, f32)> {
    let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
    crate::json::java_hash_order(&names, 16).into_iter().map(|i| fields[i].clone()).collect()
}

/// The default fields of an index (`index.query.default_field`, `*`).
pub fn default_fields(ctx: &QueryCtx) -> Vec<(String, f32)> {
    let v = ctx.idx.settings.get("index.query.default_field").unwrap_or("*");
    v.split(',').map(|f| super::query_string::parse_field_boost(f.trim())).collect()
}

fn boosted(q: QueryRef, boost: f32) -> QueryRef {
    if boost == 1.0 || q.as_any().is::<search_index::query::MatchNoneQuery>() { q } else { arc(BoostQuery::new(q, boost)) }
}

pub fn multi_match(ctx: &QueryCtx, m: &MultiMatch) -> EsResult<QueryRef> {
    // `MultiMatchQueryBuilder.doToQuery` checks the analyzer before resolving any field.
    if let Some(n) = &m.analyzer {
        named_analyzer(ctx, n, "multi_match")?;
    }
    let explicit = !m.fields.is_empty();
    let fields = if explicit { resolve_fields(ctx, &m.fields) } else { resolve_fields(ctx, &default_fields(ctx)) };
    // `QueryParserHelper.resolveMappingFields` collects into a `HashMap`: its iteration order
    // orders the per-field queries.
    let fields: Vec<(String, f32)> = {
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        crate::json::java_hash_order(&names, 16).into_iter().map(|i| fields[i].clone()).collect()
    };
    let lenient = m.lenient.unwrap_or(!explicit);
    let ty = m.ty;
    // `MultiMatchQueryBuilder.Type.tieBreaker()`: `bool_prefix` scores like `most_fields`.
    let tie = m.tie_breaker.unwrap_or(if matches!(ty, MultiMatchType::MostFields | MultiMatchType::BoolPrefix) { 1.0 } else { 0.0 });
    if fields.is_empty() {
        return Ok(match_none("No fields to search"));
    }
    if ty == MultiMatchType::CrossFields {
        return cross_fields(ctx, m, &fields, lenient, tie);
    }
    let (mt, slop) = match ty {
        MultiMatchType::Phrase => (MatchType::Phrase, m.slop),
        MultiMatchType::PhrasePrefix => (MatchType::PhrasePrefix, m.slop),
        MultiMatchType::BoolPrefix => (MatchType::BooleanPrefix, 0),
        _ => (MatchType::Boolean, 0),
    };
    let o = MatchOpts {
        operator: m.operator,
        analyzer: m.analyzer.as_deref(),
        fuzzy: Some(&m.fuzzy),
        slop,
        max_expansions: m.fuzzy.max_expansions,
        lenient,
        zero_terms: m.zero_terms_query,
        auto_generate_synonyms_phrase_query: m.auto_generate_synonyms_phrase_query,
    };
    let mut queries = Vec::new();
    for (name, boost) in &fields {
        if let Some(q) = field_query(ctx, name, &m.query, mt, &o, "multi_match")? {
            let q = if matches!(mt, MatchType::Boolean | MatchType::BooleanPrefix) { apply_msm(q, m.minimum_should_match.as_deref())? } else { q };
            queries.push(boosted(q, *boost));
        }
    }
    match queries.len() {
        0 => Ok(zero_terms_query(m.zero_terms_query)),
        1 => Ok(queries.remove(0)),
        _ => Ok(arc(DisMaxQuery::new(queries, tie))),
    }
}

/// `cross_fields`: fields grouped by search analyzer; each term blended across a group's fields
/// (document frequencies taken from the most frequent field, Lucene's `BlendedTermQuery`).
#[allow(clippy::type_complexity)]
fn cross_fields(ctx: &QueryCtx, m: &MultiMatch, fields: &[(String, f32)], lenient: bool, tie: f32) -> EsResult<QueryRef> {
    let text = value_text(&m.query);
    // Groups by analyzer name.
    let mut groups: Vec<(String, Option<Analyzer>, Vec<(Arc<FieldInfo>, f32)>)> = Vec::new();
    let mut others: Vec<QueryRef> = Vec::new();
    for (name, boost) in fields {
        let Some(f) = ctx.field(name) else { continue };
        let a = match &m.analyzer {
            Some(n) => Some(named_analyzer(ctx, n, "multi_match")?),
            None => f.query_analyzer().cloned(),
        };
        let Some(a) = a else {
            let o = MatchOpts { lenient, ..Default::default() };
            if let Some(q) = field_query(ctx, name, &m.query, MatchType::Boolean, &o, "multi_match")? {
                others.push(boosted(q, *boost));
            }
            continue;
        };
        let key = a.name().to_string();
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.2.push((f, *boost)),
            None => groups.push((key, Some(a), vec![(f, *boost)])),
        }
    }
    let occur = if m.operator == Operator::And { Occur::Must } else { Occur::Should };
    for (_, a, fs) in &groups {
        let Some(a) = a else { continue };
        let toks = analyze(a, &text);
        if toks.is_empty() {
            continue;
        }
        let mut b = BooleanQuery::new();
        for (_, g) in positions(&toks) {
            let mut alts = Vec::new();
            for t in g {
                alts.push(blended_term(ctx, fs, &t.term, tie));
            }
            let q = if alts.len() == 1 { alts.remove(0) } else { arc(DisMaxQuery::new(alts, tie)) };
            b = b.add(occur, q);
        }
        let q: QueryRef = if b.clauses.len() == 1 { b.clauses[0].query.clone() } else { arc(b) };
        others.push(apply_msm(q, m.minimum_should_match.as_deref())?);
    }
    match others.len() {
        0 => Ok(zero_terms_query(m.zero_terms_query)),
        1 => Ok(others.remove(0)),
        _ => Ok(arc(DisMaxQuery::new(others, tie))),
    }
}

/// `BlendedTermQuery.dismaxBlendedQuery`: one term over several fields with blended stats.
fn blended_term(ctx: &QueryCtx, fields: &[(Arc<FieldInfo>, f32)], term: &str, tie: f32) -> QueryRef {
    let stats: Vec<TermStats> = fields.iter().map(|(f, _)| ctx.searcher.term_stats(f.number, term.as_bytes())).collect();
    let stats = blend(ctx, fields, stats);
    let mut queries = Vec::new();
    for ((f, boost), st) in fields.iter().zip(&stats) {
        let q: QueryRef = if st.doc_freq == 0 { arc(TermQuery::new(f.number, term.as_bytes().to_vec())) } else { arc(TermQuery::with_stats(f.number, term.as_bytes().to_vec(), *st)) };
        queries.push(boosted(q, *boost));
    }
    if queries.len() == 1 {
        return queries.remove(0);
    }
    arc(DisMaxQuery::new(queries, tie))
}

use search_index::similarity::TermStats;

/// Blends per-field term statistics like Lucene's `BlendedTermQuery`: every field gets (about)
/// the largest document frequency, with a +1 bias per step towards less frequent fields so
/// the more popular field wins ties, and one shared total term frequency.
fn blend(ctx: &QueryCtx, fields: &[(Arc<FieldInfo>, f32)], mut stats: Vec<TermStats>) -> Vec<TermStats> {
    if stats.len() <= 1 {
        return stats;
    }
    let mut max_doc = ctx.searcher.max_doc() as u64;
    let mut max = 0u64;
    let mut min_sum_ttf = u64::MAX;
    for ((f, _), st) in fields.iter().zip(&stats) {
        max = max.max(st.doc_freq);
        if st.total_term_freq > 0 {
            let sum = ctx.searcher.collection_stats(f.number).map(|c| c.sum_total_term_freq).unwrap_or(0);
            min_sum_ttf = min_sum_ttf.min(sum);
        }
    }
    if max_doc > min_sum_ttf {
        max_doc = min_sum_ttf;
    }
    if max == 0 {
        return stats;
    }
    // Stable sort by descending document frequency.
    let mut order: Vec<usize> = (0..stats.len()).collect();
    order.sort_by(|&a, &b| stats[b].doc_freq.cmp(&stats[a].doc_freq));
    let mut prev = stats[order[0]].doc_freq;
    let mut actual = max_doc.min(max);
    let mut sum_ttf = 0u64;
    for &i in &order {
        let current = stats[i].doc_freq;
        if current == 0 {
            break;
        }
        if prev > current {
            actual += 1;
        }
        let doc_count = ctx.searcher.collection_stats(fields[i].0.number).map(|c| c.doc_count).unwrap_or(0);
        let df = actual.min(doc_count).min(max_doc);
        stats[i] = TermStats { doc_freq: df, total_term_freq: stats[i].total_term_freq.max(df) };
        prev = current;
        sum_ttf += stats[i].total_term_freq;
    }
    let sum_ttf = sum_ttf.min(min_sum_ttf);
    for st in stats.iter_mut() {
        if st.doc_freq != 0 {
            st.total_term_freq = sum_ttf;
        }
    }
    stats
}

pub fn combined_fields(ctx: &QueryCtx, c: &crate::query::full_text::CombinedFields) -> EsResult<QueryRef> {
    // `CombinedFieldsQueryBuilder.doToQuery`: no default fields.
    if c.fields.is_empty() {
        return Err(EsError::illegal_argument("In [combined_fields] query, at least one field must be provided"));
    }
    let fields = resolve_fields(ctx, &c.fields);
    let mut infos: Vec<(Arc<FieldInfo>, f32)> = Vec::new();
    let mut analyzer: Option<Analyzer> = None;
    for (name, boost) in &fields {
        let Some(f) = ctx.field(name) else { continue };
        if !f.kind.is_text() {
            return Err(EsError::illegal_argument(format!(
                "Field [{}] of type [{}] does not support [combined_fields] queries",
                f.name, f.ty
            )));
        }
        let a = f.query_analyzer().cloned();
        if let (Some(prev), Some(cur)) = (&analyzer, &a)
            && prev.name() != cur.name()
        {
            return Err(EsError::illegal_argument(
                "All fields in [combined_fields] query must have the same search analyzer".to_string(),
            ));
        }
        analyzer = a.or(analyzer);
        if *boost < 1.0 {
            return Err(EsError::illegal_argument(format!("[combined_fields] requires field boosts to be >= 1.0, but [{}] had boost {}", f.name, crate::json::write::java_float(*boost))));
        }
        infos.push((f, *boost));
    }
    let Some(a) = analyzer else { return Ok(zero_terms_query(c.zero_terms_query)) };
    let toks = analyze(&a, &c.query);
    if toks.is_empty() {
        return Ok(zero_terms_query(c.zero_terms_query));
    }
    let weights: Vec<(u32, f32)> = infos.iter().map(|(f, b)| (f.number, *b)).collect();
    let occur = if c.operator == Operator::And { Occur::Must } else { Occur::Should };
    let mut b = BooleanQuery::new();
    for (_, g) in positions(&toks) {
        let q: QueryRef = if g.len() == 1 {
            arc(CombinedFieldQuery::new(&weights, g[0].term.as_bytes().to_vec()).map_err(index_err)?)
        } else {
            let mut s = BooleanQuery::new();
            for t in g {
                s = s.add(Occur::Should, arc(CombinedFieldQuery::new(&weights, t.term.as_bytes().to_vec()).map_err(index_err)?));
            }
            arc(s)
        };
        b = b.add(occur, q);
    }
    let q: QueryRef = if b.clauses.len() == 1 { b.clauses[0].query.clone() } else { arc(b) };
    apply_msm(q, c.minimum_should_match.as_deref())
}
