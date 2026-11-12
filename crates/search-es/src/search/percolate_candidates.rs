//! Candidate pre-selection for the `percolate` query (Elasticsearch's `QueryAnalyzer` plus the
//! `extracted_terms` field of `PercolatorFieldMapper`).
//!
//! Running every stored query against the percolated documents costs a JSON parse, a query
//! conversion and a search per stored query, per request. Elasticsearch avoids most of that by
//! extracting, at index time, terms a stored query needs (a document can only match it when it
//! contains at least one of them) and turning the percolated documents' terms into a query over
//! those extractions; only the candidates it finds are verified against the documents.
//!
//! roll keeps the same idea without extra index fields: the first percolation over a segment
//! builds a per-segment [`StoredQueries`] (cached on the segment core, so it lives exactly as
//! long as the immutable segment and is shared by every searcher holding it). It holds every
//! stored query already parsed, plus an inverted map from `(field, term)` to the stored queries
//! that need that term; queries whose needs can't be expressed as terms (ranges, scripts,
//! wildcards, match_all, …) go on an "always a candidate" list. A request then enumerates the
//! (few) terms of its in-memory document index, looks each one up, and verifies only the union
//! of hits and the always list.
//!
//! Stored queries are deduplicated by their stored bytes: an index often holds the same query
//! for many documents (Rally's `percolator` track: 2M stored queries, most of them one
//! `match`), and a query's verification doesn't depend on the document storing it, so each
//! distinct query is parsed, indexed and verified once and its result applies to all its
//! documents.
//!
//! The extraction is conservative: it may keep a query that can't match (verification drops
//! it), never drop one that can. Terms come from converting the stored query under the index's
//! current mapping, the same analysis the verification uses. When the percolated documents
//! bring fields the index's mapping lacks (dynamic mapping in the memory index), field patterns
//! of stored queries (`query_string` over `*`, `multi_match` with `fields: ["msg*"]`) could
//! reach fields the extraction never saw, so such requests verify every stored query.

use super::query::QueryCtx;
use crate::engine::fields::FieldTable;
use search_index::query::{
    BooleanQuery, BoostQuery, CombinedFieldQuery, ConstantScoreQuery, CoveringQuery, DisMaxQuery, MatchNoneQuery, MultiPhrasePrefixQuery, MultiPhraseQuery,
    MultiTermQuery, Occur, PhraseQuery, SynonymQuery, TermQuery, TermsMatcher,
};
use search_index::{QueryRef, Searcher};
use std::collections::HashMap;
use std::sync::Arc;

/// What a document must contain for a query to match it.
#[derive(Clone, Debug, PartialEq)]
pub enum Needs {
    /// At least one of these `(field number, term)` pairs.
    AnyOf(Vec<(u32, Vec<u8>)>),
    /// Nothing matches.
    Nothing,
    /// Unknown: every document may match.
    Unknown,
}

/// The requirements of a converted query (`QueryAnalyzer.analyze`, reduced to "at least one of
/// these terms").
pub fn needs(q: &QueryRef) -> Needs {
    let any = q.as_any();
    if let Some(t) = any.downcast_ref::<TermQuery>() {
        return Needs::AnyOf(vec![(t.field, t.term.clone())]);
    }
    if let Some(b) = any.downcast_ref::<BoostQuery>() {
        return needs(&b.query);
    }
    if let Some(c) = any.downcast_ref::<ConstantScoreQuery>() {
        return needs(&c.query);
    }
    if any.downcast_ref::<MatchNoneQuery>().is_some() {
        return Needs::Nothing;
    }
    if let Some(s) = any.downcast_ref::<SynonymQuery>() {
        return any_of(s.terms.iter().map(|(t, _)| (s.field, t.clone())).collect());
    }
    if let Some(c) = any.downcast_ref::<CombinedFieldQuery>() {
        return any_of(c.fields.iter().map(|(f, _)| (*f, c.term.clone())).collect());
    }
    if let Some(m) = any.downcast_ref::<MultiTermQuery>() {
        return match &m.matcher {
            TermsMatcher::Set(terms) => any_of(terms.iter().map(|t| (m.field, t.clone())).collect()),
            _ => Needs::Unknown,
        };
    }
    if let Some(p) = any.downcast_ref::<PhraseQuery>() {
        // Every term is required: the longest one is the most selective guess.
        return match p.terms.iter().max_by_key(|t| t.len()) {
            Some(t) => Needs::AnyOf(vec![(p.field, t.clone())]),
            None => Needs::Unknown,
        };
    }
    if let Some(p) = any.downcast_ref::<MultiPhraseQuery>() {
        return best_position(p.field, &p.terms);
    }
    if let Some(p) = any.downcast_ref::<MultiPhrasePrefixQuery>() {
        // The last position is a prefix: only the complete positions name exact terms.
        return match p.terms.split_last() {
            Some((_, full)) if !full.is_empty() => best_position(p.field, full),
            _ => Needs::Unknown,
        };
    }
    if let Some(d) = any.downcast_ref::<DisMaxQuery>() {
        return union(d.disjuncts.iter().map(needs));
    }
    if let Some(c) = any.downcast_ref::<CoveringQuery>() {
        return union(c.queries.iter().map(needs));
    }
    if let Some(b) = any.downcast_ref::<BooleanQuery>() {
        return boolean(b);
    }
    Needs::Unknown
}

fn any_of(terms: Vec<(u32, Vec<u8>)>) -> Needs {
    if terms.is_empty() { Needs::Nothing } else { Needs::AnyOf(terms) }
}

/// A multi-phrase needs one term of every position: the position whose terms are the longest
/// (rarest) is the guess.
fn best_position(field: u32, positions: &[Vec<Vec<u8>>]) -> Needs {
    let best = positions.iter().filter(|ts| !ts.is_empty()).max_by_key(|ts| ts.iter().map(Vec::len).min().unwrap_or(0));
    match best {
        Some(ts) => Needs::AnyOf(ts.iter().map(|t| (field, t.clone())).collect()),
        None => Needs::Unknown,
    }
}

/// Disjunction: the document needs one term of any clause.
fn union(parts: impl Iterator<Item = Needs>) -> Needs {
    let mut out: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut seen = false;
    for n in parts {
        seen = true;
        match n {
            Needs::Unknown => return Needs::Unknown,
            Needs::Nothing => {}
            Needs::AnyOf(ts) => out.extend(ts),
        }
    }
    if !seen {
        return Needs::Unknown;
    }
    out.sort_unstable();
    out.dedup();
    any_of(out)
}

fn boolean(b: &BooleanQuery) -> Needs {
    let mut best: Option<Vec<(u32, Vec<u8>)>> = None;
    let mut required = false;
    for c in b.clauses.iter().filter(|c| matches!(c.occur, Occur::Must | Occur::Filter)) {
        required = true;
        match needs(&c.query) {
            Needs::Nothing => return Needs::Nothing,
            Needs::Unknown => {}
            // Any one required clause's needs are necessary: keep the most selective (fewest
            // terms, then the longest shortest term).
            Needs::AnyOf(ts) => {
                let better = best.as_ref().is_none_or(|cur| selectivity(&ts) > selectivity(cur));
                if better {
                    best = Some(ts);
                }
            }
        }
    }
    if let Some(ts) = best {
        return Needs::AnyOf(ts);
    }
    if required {
        return Needs::Unknown;
    }
    let shoulds: Vec<&QueryRef> = b.clauses.iter().filter(|c| c.occur == Occur::Should).map(|c| &c.query).collect();
    if shoulds.is_empty() {
        // Only prohibited clauses (or none): matches everything else.
        return Needs::Unknown;
    }
    union(shoulds.into_iter().map(needs))
}

fn selectivity(ts: &[(u32, Vec<u8>)]) -> (std::cmp::Reverse<usize>, usize) {
    (std::cmp::Reverse(ts.len()), ts.iter().map(|(_, t)| t.len()).min().unwrap_or(0))
}

/// A segment's distinct stored queries, parsed, with their term requirements inverted.
pub struct StoredQueries {
    /// The distinct stored queries.
    pub queries: Vec<Arc<crate::query::Query>>,
    /// Per distinct query, the docs storing it (ascending).
    pub docs_of: Vec<Vec<u32>>,
    /// Every doc with a parseable stored query and the index of its query, by doc.
    by_doc: Vec<(u32, u32)>,
    /// Field name -> term -> indexes into `queries` needing it.
    by_term: HashMap<String, HashMap<Vec<u8>, Vec<u32>>>,
    /// Indexes into `queries` that are candidates for every request.
    always: Vec<u32>,
}

impl StoredQueries {
    /// Parses and analyses the stored queries of `field` (binary doc values) in `seg`.
    /// Unparseable stored queries are left out, as the verification would skip them anyway.
    pub fn build(ctx: &QueryCtx, seg: &search_index::SegmentContext, field: u32) -> StoredQueries {
        let mut out = StoredQueries { queries: Vec::new(), docs_of: Vec::new(), by_doc: Vec::new(), by_term: HashMap::new(), always: Vec::new() };
        let Some(dv) = seg.reader.binary(field) else { return out };
        let schema = ctx.searcher.schema();
        // Stored bytes -> distinct query; unparseable bytes map to `None`.
        let mut seen: HashMap<Vec<u8>, Option<u32>> = HashMap::new();
        for d in 0..seg.reader.max_doc() {
            let Some(bytes) = dv.get(d) else { continue };
            if let Some(known) = seen.get(bytes) {
                if let Some(i) = *known {
                    out.docs_of[i as usize].push(d);
                    out.by_doc.push((d, i));
                }
                continue;
            }
            let parsed = match crate::json::parse(bytes) {
                Ok(Some(v)) => crate::query::parse_query(&v, None, &[]).ok().map(|(q, _)| q),
                _ => None,
            };
            let Some(parsed) = parsed else {
                seen.insert(bytes.to_vec(), None);
                continue;
            };
            let qctx = QueryCtx::new(ctx.cluster, ctx.st, ctx.idx, ctx.table, ctx.searcher, ctx.now_ms);
            let n = match qctx.to_query(&parsed) {
                Ok(q) => needs(&q),
                Err(_) => Needs::Unknown,
            };
            let i = out.queries.len() as u32;
            seen.insert(bytes.to_vec(), Some(i));
            out.queries.push(Arc::new(parsed));
            out.docs_of.push(vec![d]);
            out.by_doc.push((d, i));
            match n {
                // Stays out of the candidates; but conversions can depend on the request's
                // documents (dynamic fields), so it's verified if fields were added.
                Needs::Nothing => {}
                Needs::Unknown => out.always.push(i),
                Needs::AnyOf(ts) => {
                    for (f, t) in ts {
                        let ids = out.by_term.entry(schema.name(f)).or_default().entry(t).or_default();
                        if ids.last() != Some(&i) {
                            ids.push(i);
                        }
                    }
                }
            }
        }
        out
    }

    /// Indexes into [`queries`](Self::queries) worth verifying against the documents of
    /// `mem` (all of them when `all`), ascending.
    pub fn candidates(&self, mem: &Searcher, all: bool) -> Vec<u32> {
        if all {
            return (0..self.queries.len() as u32).collect();
        }
        let mut hit = vec![false; self.queries.len()];
        for &i in &self.always {
            hit[i as usize] = true;
        }
        let schema = mem.schema();
        for (name, terms) in &self.by_term {
            let Some(fs) = schema.by_name(name) else { continue };
            for seg in mem.segments() {
                let Some(tr) = seg.reader.terms(fs.number) else { continue };
                // Probe from the smaller side.
                if (terms.len() as u64) < tr.stats().num_terms {
                    for (t, ids) in terms {
                        if tr.seek_exact(t).is_some() {
                            ids.iter().for_each(|&i| hit[i as usize] = true);
                        }
                    }
                } else {
                    let mut it = tr.iter();
                    while let Some(t) = it.next() {
                        if let Some(ids) = terms.get(t) {
                            ids.iter().for_each(|&i| hit[i as usize] = true);
                        }
                    }
                }
            }
        }
        hit.iter().enumerate().filter(|(_, h)| **h).map(|(i, _)| i as u32).collect()
    }

    /// The index into [`queries`](Self::queries) of the query `doc` stores.
    pub fn query_of(&self, doc: u32) -> Option<u32> {
        self.by_doc.binary_search_by_key(&doc, |e| e.0).ok().map(|i| self.by_doc[i].1)
    }

    /// Approximate heap bytes (segment cache accounting).
    pub fn heap_bytes(&self) -> usize {
        let terms: usize = self.by_term.values().flat_map(|m| m.iter()).map(|(t, ids)| t.len() + ids.len() * 4 + 48).sum();
        self.queries.len() * 280 + self.by_doc.len() * 12 + terms + self.always.len() * 4
    }
}

/// Whether the memory index maps fields the index doesn't (dynamic fields of the percolated
/// documents): extraction under the index's mapping could then miss terms.
pub fn adds_fields(index: &FieldTable, mem: &FieldTable) -> bool {
    mem.fields.keys().any(|n| !index.fields.contains_key(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_index::query::{BooleanQuery, MatchAllQuery, Occur, PhraseQuery, TermQuery};

    fn term(f: u32, t: &str) -> QueryRef {
        Arc::new(TermQuery::new(f, t.as_bytes().to_vec()))
    }

    fn terms(n: &Needs) -> Vec<(u32, String)> {
        match n {
            Needs::AnyOf(ts) => ts.iter().map(|(f, t)| (*f, String::from_utf8_lossy(t).into_owned())).collect(),
            _ => vec![],
        }
    }

    #[test]
    fn term_and_boolean_requirements() {
        assert_eq!(terms(&needs(&term(1, "a"))), vec![(1, "a".to_string())]);
        // Must clauses: the most selective one (fewest terms) wins.
        let should_ab: QueryRef = Arc::new(BooleanQuery::new().add(Occur::Should, term(1, "a")).add(Occur::Should, term(1, "b")));
        let q: QueryRef = Arc::new(BooleanQuery::new().add(Occur::Must, should_ab.clone()).add(Occur::Filter, term(2, "zz")));
        assert_eq!(terms(&needs(&q)), vec![(2, "zz".to_string())]);
        // Should-only: the union.
        assert_eq!(terms(&needs(&should_ab)), vec![(1, "a".to_string()), (1, "b".to_string())]);
        // A should clause that can match anything makes the disjunction unknown.
        let q: QueryRef = Arc::new(BooleanQuery::new().add(Occur::Should, term(1, "a")).add(Occur::Should, Arc::new(MatchAllQuery)));
        assert_eq!(needs(&q), Needs::Unknown);
        // Must-not alone: unknown; a required unknown with a required term: the term.
        let q: QueryRef = Arc::new(BooleanQuery::new().add(Occur::MustNot, term(1, "a")));
        assert_eq!(needs(&q), Needs::Unknown);
        let q: QueryRef = Arc::new(BooleanQuery::new().add(Occur::Must, Arc::new(MatchAllQuery)).add(Occur::Must, term(3, "x")).add(Occur::MustNot, term(3, "y")));
        assert_eq!(terms(&needs(&q)), vec![(3, "x".to_string())]);
        // A required clause matching nothing: nothing.
        let q: QueryRef = Arc::new(BooleanQuery::new().add(Occur::Must, Arc::new(MatchNoneQuery { reason: String::new() })).add(Occur::Should, term(1, "a")));
        assert_eq!(needs(&q), Needs::Nothing);
    }

    #[test]
    fn phrases_need_their_rarest_guess() {
        let p = PhraseQuery { field: 4, terms: vec![b"to".to_vec(), b"quick".to_vec(), b"fox".to_vec()], positions: vec![0, 1, 2], slop: 0 };
        assert_eq!(terms(&needs(&(Arc::new(p) as QueryRef))), vec![(4, "quick".to_string())]);
        let mp = MultiPhraseQuery { field: 4, terms: vec![vec![b"a".to_vec(), b"an".to_vec()], vec![b"brown".to_vec(), b"browns".to_vec()]], positions: vec![0, 1], slop: 0 };
        assert_eq!(terms(&needs(&(Arc::new(mp) as QueryRef))), vec![(4, "brown".to_string()), (4, "browns".to_string())]);
    }

    #[test]
    fn sets_and_wrappers() {
        let set: QueryRef = Arc::new(MultiTermQuery {
            field: 2,
            matcher: TermsMatcher::Set(Arc::new(vec![b"x".to_vec(), b"y".to_vec()])),
            rewrite: Default::default(),
        });
        let q: QueryRef = Arc::new(BoostQuery { query: Arc::new(ConstantScoreQuery { query: set }), boost: 2.0 });
        assert_eq!(terms(&needs(&q)), vec![(2, "x".to_string()), (2, "y".to_string())]);
        let prefix: QueryRef = Arc::new(MultiTermQuery::prefix(2, "x"));
        assert_eq!(needs(&prefix), Needs::Unknown);
        let dm: QueryRef = Arc::new(DisMaxQuery { disjuncts: vec![term(1, "a"), Arc::new(MatchNoneQuery { reason: String::new() })], tie_breaker: 0.0 });
        assert_eq!(terms(&needs(&dm)), vec![(1, "a".to_string())]);
    }
}
