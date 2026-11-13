//! Span query matches over one field instance's analyzed tokens, for highlighting.
//!
//! Lucene highlights span queries term by term, but only the term occurrences that take part
//! in a match of the whole span tree: the unified highlighter through the sub-matches of
//! `SpanWeight.matches` (weight matches) or `PhraseHelper` (without them), the plain
//! highlighter through `WeightedSpanTermExtractor`. `span_near [quick, fox]` highlights
//! `quick` in "quick brown fox" and nothing in "quick dog".
//!
//! The doc is tiny (one hit's values), so matches are computed eagerly per node into sorted
//! lists, then combined with the iteration rules of Lucene's span classes, which decide which
//! occurrences a match uses: `NearSpansOrdered` (stretch to order, positions never move back),
//! `NearSpansUnordered` (advance the cell with the lowest start until the window fits),
//! `SpanOr` (union by start, then end), `SpanNot` / `SpanFirst` (filters), and the
//! `ContainSpans` of `span_containing` / `span_within`.

use std::collections::HashSet;

use crate::highlight::HlToken;
use crate::query::SpanNode;

/// One match: `[start, end)` positions and its leaf term occurrences (position, term) — what
/// Lucene's `SpanCollector` gathers for it.
#[derive(Clone, Debug)]
struct M {
    start: i64,
    end: i64,
    leaves: Vec<(u32, Vec<u8>)>,
}

/// The (position, term) occurrences that take part in some match of `node` over `tokens`.
pub fn leaves(node: &SpanNode, tokens: &[HlToken]) -> HashSet<(u32, Vec<u8>)> {
    let mut out = HashSet::new();
    for m in matches(node, tokens) {
        out.extend(m.leaves);
    }
    out
}

/// The `[start, end)` position ranges of the node's matches (what the plain highlighter's
/// `WeightedSpanTermExtractor` records per span query).
pub fn matches_ranges(node: &SpanNode, tokens: &[HlToken]) -> Vec<(i64, i64)> {
    matches(node, tokens).into_iter().map(|m| (m.start, m.end)).collect()
}

/// The plain highlighter's rule: a token of any term the span query reads (excludes
/// included) highlights inside one of its match ranges.
pub fn in_match_range(node: &SpanNode, ranges: &[(i64, i64)], t: &HlToken) -> bool {
    let p = t.position as i64;
    ranges.iter().any(|&(s, e)| s <= p && p < e) && reads(node, &t.term)
}

/// Every term the node reads, `span_not` excludes included (Lucene's term collector).
fn reads(node: &SpanNode, term: &[u8]) -> bool {
    match node {
        SpanNode::Not { include, exclude, .. } => reads(include, term) || reads(exclude, term),
        SpanNode::Mask { inner, .. } | SpanNode::First { inner, .. } => reads(inner, term),
        SpanNode::Near { clauses, .. } | SpanNode::Or(clauses) => clauses.iter().any(|c| reads(c, term)),
        SpanNode::Containing { big, little } | SpanNode::Within { big, little } => reads(big, term) || reads(little, term),
        leaf => extracts(leaf, term),
    }
}

/// Whether the node reads `term` outside a `span_not` exclude: the terms Lucene's query
/// visitor extracts (the unified highlighter's re-analysis keeps only those tokens).
pub fn extracts(node: &SpanNode, term: &[u8]) -> bool {
    match node {
        SpanNode::Term { term: t, .. } => t == term,
        SpanNode::Multi(q) => q.matches_term(term),
        SpanNode::Gap { .. } => false,
        SpanNode::Mask { inner, .. } | SpanNode::First { inner, .. } => extracts(inner, term),
        SpanNode::Not { include, .. } => extracts(include, term),
        SpanNode::Near { clauses, .. } | SpanNode::Or(clauses) => clauses.iter().any(|c| extracts(c, term)),
        SpanNode::Containing { big, little } | SpanNode::Within { big, little } => extracts(big, term) || extracts(little, term),
    }
}

/// The node's matches in Lucene's iteration order (start, then end).
fn matches(node: &SpanNode, tokens: &[HlToken]) -> Vec<M> {
    match node {
        SpanNode::Term { term, .. } => term_matches(tokens, |t| t == term.as_slice()),
        SpanNode::Multi(q) => term_matches(tokens, |t| q.matches_term(t)),
        // A gap only means something inside an ordered near (see `near_ordered`).
        SpanNode::Gap { .. } => Vec::new(),
        SpanNode::Mask { inner, .. } => matches(inner, tokens),
        SpanNode::Or(clauses) => {
            let mut all: Vec<M> = clauses.iter().flat_map(|c| matches(c, tokens)).collect();
            // A stable sort keeps clause order between equal spans (the queue's tie order).
            all.sort_by_key(|m| (m.start, m.end));
            all
        }
        SpanNode::Near { clauses, slop, in_order } => {
            if *in_order {
                near_ordered(clauses, *slop as i64, tokens)
            } else {
                let subs: Vec<Vec<M>> = clauses.iter().map(|c| matches(c, tokens)).collect();
                near_unordered(&subs, *slop as i64)
            }
        }
        SpanNode::Not { include, exclude, pre, post } => {
            let excl = matches(exclude, tokens);
            let (pre, post) = (*pre as i64, *post as i64);
            let mut e = 0usize;
            let mut out = Vec::new();
            for m in matches(include, tokens) {
                // `SpanNotQuery`'s filter: skip excludes that end before the candidate (less
                // `pre`); the candidate stays when the next exclude starts after it (plus
                // `post`), or none is left.
                while e < excl.len() && excl[e].end <= m.start - pre {
                    e += 1;
                }
                if e == excl.len() || excl[e].start - post >= m.end {
                    out.push(m);
                }
            }
            out
        }
        SpanNode::First { inner, end } => matches(inner, tokens).into_iter().filter(|m| m.start >= 0 && m.end <= *end as i64).collect(),
        SpanNode::Containing { big, little } => {
            let (bigs, littles) = (matches(big, tokens), matches(little, tokens));
            let mut l = 0usize;
            let mut out = Vec::new();
            for b in bigs {
                while l < littles.len() && littles[l].start < b.start {
                    l += 1;
                }
                if l == littles.len() {
                    break;
                }
                if b.end >= littles[l].end {
                    let mut m = b;
                    m.leaves.extend(littles[l].leaves.iter().cloned());
                    out.push(m);
                }
            }
            out
        }
        SpanNode::Within { big, little } => {
            let (bigs, littles) = (matches(big, tokens), matches(little, tokens));
            let mut b = 0usize;
            let mut out = Vec::new();
            for lm in littles {
                while b < bigs.len() && bigs[b].end < lm.end {
                    b += 1;
                }
                if b == bigs.len() {
                    break;
                }
                // The little match is the match; its leaves are both sides' (`ConjunctionSpans`
                // collects every sub-span).
                if bigs[b].start <= lm.start {
                    let mut m = lm;
                    m.leaves.extend(bigs[b].leaves.iter().cloned());
                    out.push(m);
                }
            }
            out
        }
    }
}

fn term_matches(tokens: &[HlToken], hit: impl Fn(&[u8]) -> bool) -> Vec<M> {
    let mut out: Vec<M> = tokens
        .iter()
        .filter(|t| hit(&t.term))
        .map(|t| M { start: t.position as i64, end: t.position as i64 + 1, leaves: vec![(t.position, t.term.clone())] })
        .collect();
    out.sort_by_key(|m| (m.start, m.end));
    out
}

/// `NearSpansOrdered`: the first clause steps through its matches; every later clause moves
/// forward (never back) to its first match starting at or after the previous one's end; a
/// match when the gaps add up to at most `slop`. A `span_gap` clause matches right where the
/// previous one ends, `width` positions long.
fn near_ordered(clauses: &[SpanNode], slop: i64, tokens: &[HlToken]) -> Vec<M> {
    let subs: Vec<Option<Vec<M>>> = clauses
        .iter()
        .map(|c| match c {
            SpanNode::Gap { .. } => None,
            c => Some(matches(c, tokens)),
        })
        .collect();
    let mut out = Vec::new();
    let Some(Some(first)) = subs.first() else { return out };
    let mut idx = vec![0usize; subs.len()];
    'outer: for m0 in first {
        let (mut prev_end, start, mut width) = (m0.end, m0.start, 0i64);
        let mut leaves = m0.leaves.clone();
        for i in 1..subs.len() {
            match &subs[i] {
                None => {
                    let SpanNode::Gap { width: w, .. } = &clauses[i] else { unreachable!() };
                    prev_end += *w as i64;
                }
                Some(list) => {
                    while idx[i] < list.len() && list[idx[i]].start < prev_end {
                        idx[i] += 1;
                    }
                    let Some(m) = list.get(idx[i]) else { break 'outer };
                    width += m.start - prev_end;
                    prev_end = m.end;
                    leaves.extend(m.leaves.iter().cloned());
                }
            }
        }
        if width <= slop {
            out.push(M { start, end: prev_end, leaves });
        }
    }
    out
}

/// `NearSpansUnordered`: every clause sits on one of its matches; while the window from the
/// lowest start to the highest end, less the clauses' own lengths, exceeds `slop`, the clause
/// with the lowest (start, end) moves on; each fitting window is a match, after which the
/// lowest clause moves on too.
fn near_unordered(subs: &[Vec<M>], slop: i64) -> Vec<M> {
    let mut out = Vec::new();
    if subs.is_empty() || subs.iter().any(Vec::is_empty) {
        return out;
    }
    let mut idx = vec![0usize; subs.len()];
    loop {
        let cur = |i: usize, idx: &[usize]| &subs[i][idx[i]];
        let min = (0..subs.len()).min_by_key(|&i| (cur(i, &idx).start, cur(i, &idx).end)).unwrap();
        let max_end = (0..subs.len()).map(|i| cur(i, &idx).end).max().unwrap();
        let total: i64 = (0..subs.len()).map(|i| cur(i, &idx).end - cur(i, &idx).start).sum();
        let start = cur(min, &idx).start;
        let width = max_end - start - total;
        if width <= slop {
            let leaves = (0..subs.len()).flat_map(|i| cur(i, &idx).leaves.iter().cloned()).collect();
            out.push(M { start, end: max_end, leaves });
        }
        idx[min] += 1;
        if idx[min] == subs[min].len() {
            return out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(words: &[&str]) -> Vec<HlToken> {
        words
            .iter()
            .enumerate()
            .map(|(i, w)| HlToken { term: w.as_bytes().to_vec(), position: i as u32, start: 0, end: 0, position_increment: 1 })
            .collect()
    }
    fn t(s: &str) -> SpanNode {
        SpanNode::Term { field: 0, term: s.as_bytes().to_vec() }
    }
    fn positions(node: &SpanNode, words: &[&str]) -> Vec<u32> {
        let mut v: Vec<u32> = leaves(node, &toks(words)).into_iter().map(|(p, _)| p).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn ordered_near_highlights_only_matching_occurrences() {
        let q = SpanNode::Near { clauses: vec![t("quick"), t("fox")], slop: 2, in_order: true };
        // Second value (after the position gap): `quick` without a `fox` near it.
        let mut words = vec!["red", "quick", "apple", "fox", "banana"];
        words.extend(std::iter::repeat_n("_", 100));
        words.extend(["of", "quick", "search"]);
        assert_eq!(positions(&q, &words), vec![1, 3]);
        assert!(positions(&q, &["fox", "quick"]).is_empty());
        assert!(positions(&q, &["quick", "a", "b", "c", "fox"]).is_empty());
    }

    #[test]
    fn unordered_near_and_filters() {
        let q = SpanNode::Near { clauses: vec![t("quick"), t("fox")], slop: 0, in_order: false };
        assert_eq!(positions(&q, &["fox", "quick", "x", "quick"]), vec![0, 1]);
        let not = SpanNode::Not { include: Box::new(t("quick")), exclude: Box::new(t("fox")), pre: 1, post: 0 };
        assert_eq!(positions(&not, &["fox", "quick", "x", "quick"]), vec![3]);
        let first = SpanNode::First { inner: Box::new(t("quick")), end: 2 };
        assert_eq!(positions(&first, &["fox", "quick", "x", "quick"]), vec![1]);
        let or = SpanNode::Or(vec![t("fox"), t("x")]);
        assert_eq!(positions(&or, &["fox", "quick", "x"]), vec![0, 2]);
    }

    #[test]
    fn containing_within_and_gaps() {
        let big = SpanNode::Near { clauses: vec![t("a"), t("c")], slop: 3, in_order: true };
        let c = SpanNode::Containing { big: Box::new(big.clone()), little: Box::new(t("b")) };
        assert_eq!(positions(&c, &["a", "b", "c", "a", "x", "c"]), vec![0, 1, 2]);
        let w = SpanNode::Within { big: Box::new(big), little: Box::new(t("b")) };
        assert_eq!(positions(&w, &["b", "a", "b", "c"]), vec![1, 2, 3]);
        // The plain highlighter: query terms inside the little match's range only.
        let toks = toks(&["b", "a", "b", "c"]);
        let w = SpanNode::Within { big: Box::new(SpanNode::Near { clauses: vec![t("a"), t("c")], slop: 3, in_order: true }), little: Box::new(t("b")) };
        let hits: Vec<u32> = toks.iter().filter(|x| in_match_range(&w, &matches_ranges(&w, &toks), x)).map(|x| x.position).collect();
        assert_eq!(hits, vec![2]);
        let gap = SpanNode::Near { clauses: vec![t("a"), SpanNode::Gap { field: 0, width: 1 }, t("c")], slop: 0, in_order: true };
        assert_eq!(positions(&gap, &["a", "b", "c", "a", "c"]), vec![0, 2]);
    }
}
