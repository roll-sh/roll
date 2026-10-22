//! Interval queries (Elasticsearch's `intervals`, Lucene's `IntervalQuery`): minimal
//! intervals of term positions combined by ordered / unordered / phrase / disjunction /
//! at-least operators and filtered by gaps, width and containment relations.
//!
//! [`IntervalsSource`] is built through the same factories as Lucene's `Intervals` (with the
//! same rewrites: disjunctions pulled up through conjunctions, repeated sources folded,
//! nested phrases flattened), so the iterated intervals, hence matches and scores, are
//! Lucene's. Each segment gets a tree of iterators that walk positions lazily, two-phase:
//! the approximation is the docs' conjunction / disjunction, `matches` asks the root for a
//! first interval.
//!
//! Score: `boost · S / (S + pivot)` (saturation, pivot 1) where the sloppy frequency `S` sums
//! `1 / max(width − min_extent + 1, 1)` over the doc's intervals.

use std::any::Any;
use std::collections::{BinaryHeap, HashSet};
use std::cmp::Reverse;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use search_codec::{NO_MORE_DOCS, PostingsCursor, PostingsFlags};

use crate::error::{Error, Result};
use crate::query::multiterm::MultiTermQuery;
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};

/// `IntervalIterator.NO_MORE_INTERVALS`.
pub const NO_MORE_INTERVALS: i32 = i32::MAX;
const NO_MORE: i64 = NO_MORE_DOCS as i64;
/// `IndexSearcher.getMaxClauseCount()` for disjunction pull-up.
pub const MAX_DISJUNCTION_EXPANSION: usize = 4096;
/// `Intervals.DEFAULT_MAX_EXPANSIONS`.
pub const DEFAULT_MAX_EXPANSIONS: usize = 128;

/// A source of intervals. Build it with the associated constructors (Lucene's `Intervals`
/// factory methods), which apply Lucene's rewrites.
#[derive(Clone, Debug)]
pub enum IntervalsSource {
    Term(Vec<u8>),
    /// Terms of a multi-term query (prefix, wildcard, regexp, fuzzy, range), at most
    /// `max_expansions` per segment; `pattern` is Lucene's description.
    MultiTerm { query: Arc<MultiTermQuery>, max_expansions: usize, pattern: String },
    Block(Vec<IntervalsSource>),
    Ordered(Vec<IntervalsSource>),
    Unordered(Vec<IntervalsSource>),
    /// Disjunction; `pull_up`: whether conjunctions above may distribute over it.
    Or(Vec<IntervalsSource>, bool),
    Repeating(Box<IntervalsSource>, usize, Option<&'static str>),
    MaxGaps(Box<IntervalsSource>, i32),
    MaxWidth(Box<IntervalsSource>, i32),
    Extend(Box<IntervalsSource>, i32, i32),
    Containing(Box<IntervalsSource>, Box<IntervalsSource>),
    ContainedBy(Box<IntervalsSource>, Box<IntervalsSource>),
    NotContaining(Box<IntervalsSource>, Box<IntervalsSource>),
    NotContainedBy(Box<IntervalsSource>, Box<IntervalsSource>),
    NonOverlapping(Box<IntervalsSource>, Box<IntervalsSource>),
    Overlapping(Box<IntervalsSource>, Box<IntervalsSource>),
    /// `PRECEDING` (true) / `FOLLOWING` (false): the position before / after each interval.
    Offset(Box<IntervalsSource>, bool),
    AtLeast(Vec<IntervalsSource>, usize),
    /// Reads another field (`use_field`).
    Field(u32, Box<IntervalsSource>),
    /// Elasticsearch's `SourceIntervalsSource` (`match_only_text`): the source runs, per
    /// candidate doc of its approximation, over a one-doc in-memory segment of the doc's
    /// re-analyzed values. Transparent in descriptions.
    Source(Arc<crate::query::source_confirmed::SourceSpec>, Box<IntervalsSource>),
    NoMatch(String),
    /// A construction error reported when the query runs.
    Error(String),
}

impl PartialEq for IntervalsSource {
    fn eq(&self, o: &Self) -> bool {
        self.key() == o.key()
    }
}
impl Eq for IntervalsSource {}
impl Hash for IntervalsSource {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.key().hash(h)
    }
}

type Src = IntervalsSource;

fn bx(s: Src) -> Box<Src> {
    Box::new(s)
}

impl IntervalsSource {
    // ---- factories (Lucene's `Intervals`) ----

    pub fn term(t: impl Into<Vec<u8>>) -> Src {
        Src::Term(t.into())
    }

    /// Terms at consecutive positions.
    pub fn phrase(subs: Vec<Src>) -> Src {
        if subs.len() == 1 {
            return subs.into_iter().next().unwrap();
        }
        Src::or(pull_up_list(&subs, &|v| Src::Block(flatten_blocks(v))))
    }

    pub fn phrase_terms<T: Into<Vec<u8>>>(terms: impl IntoIterator<Item = T>) -> Src {
        Src::phrase(terms.into_iter().map(Src::term).collect())
    }

    pub fn or(subs: Vec<Src>) -> Src {
        Src::or_with(subs, true)
    }

    /// A disjunction; `pull_up`: whether enclosing conjunctions may be distributed over it.
    pub fn or_with(subs: Vec<Src>, pull_up: bool) -> Src {
        let s = simplify_or(subs);
        if s.len() == 1 {
            return s.into_iter().next().unwrap();
        }
        Src::Or(s, pull_up)
    }

    pub fn ordered(subs: Vec<Src>) -> Src {
        if subs.len() == 1 {
            return subs.into_iter().next().unwrap();
        }
        let mut out: Vec<Src> = Vec::new();
        let mut cur: Vec<Src> = Vec::new();
        for s in subs {
            if cur.is_empty() || cur[0] == s {
                cur.push(s);
            } else {
                let n = cur.len();
                out.push(repeating(cur.swap_remove(0), n));
                cur.clear();
                cur.push(s);
            }
        }
        let n = cur.len();
        out.push(repeating(cur.swap_remove(0), n));
        if out.len() == 1 {
            return name_repeating(out.pop().unwrap(), "ORDERED");
        }
        Src::Ordered(out)
    }

    pub fn unordered(subs: Vec<Src>) -> Src {
        if subs.len() == 1 {
            return subs.into_iter().next().unwrap();
        }
        let mut counts: Vec<(Src, usize)> = Vec::new();
        for s in subs {
            match counts.iter_mut().find(|(x, _)| *x == s) {
                Some(e) => e.1 += 1,
                None => counts.push((s, 1)),
            }
        }
        let mut out: Vec<Src> = counts.into_iter().map(|(s, n)| repeating(s, n)).collect();
        if out.len() == 1 {
            return name_repeating(out.pop().unwrap(), "UNORDERED");
        }
        Src::Unordered(out)
    }

    pub fn unordered_no_overlaps(a: Src, b: Src) -> Src {
        Src::or(vec![Src::ordered(vec![a.clone(), b.clone()]), Src::ordered(vec![b, a])])
    }

    pub fn max_gaps(gaps: i32, s: Src) -> Src {
        Src::or(s.pull_up_disjunctions().into_iter().map(|x| Src::MaxGaps(bx(x), gaps)).collect())
    }

    pub fn max_width(width: i32, s: Src) -> Src {
        Src::MaxWidth(bx(s), width)
    }

    pub fn extend(s: Src, before: i32, after: i32) -> Src {
        Src::Extend(bx(s), before, after)
    }

    pub fn containing(big: Src, small: Src) -> Src {
        Src::or(pull_up_one(&big, &|s| Src::Containing(bx(s), bx(small.clone()))))
    }

    pub fn contained_by(small: Src, big: Src) -> Src {
        Src::or(pull_up_one(&big, &|s| Src::ContainedBy(bx(small.clone()), bx(s))))
    }

    pub fn not_containing(minuend: Src, subtrahend: Src) -> Src {
        Src::or(pull_up_one(&minuend, &|s| Src::NotContaining(bx(s), bx(subtrahend.clone()))))
    }

    pub fn not_contained_by(minuend: Src, subtrahend: Src) -> Src {
        Src::or(pull_up_one(&subtrahend, &|s| Src::NotContainedBy(bx(minuend.clone()), bx(s))))
    }

    pub fn non_overlapping(minuend: Src, subtrahend: Src) -> Src {
        Src::NonOverlapping(bx(minuend), bx(subtrahend))
    }

    pub fn overlapping(source: Src, reference: Src) -> Src {
        Src::Overlapping(bx(source), bx(reference))
    }

    pub fn not_within(minuend: Src, positions: i32, subtrahend: Src) -> Src {
        Src::NonOverlapping(bx(minuend), bx(Src::extend(subtrahend, positions, positions)))
    }

    pub fn within(source: Src, positions: i32, reference: Src) -> Src {
        Src::contained_by(source, Src::extend(reference, positions, positions))
    }

    pub fn before(source: Src, reference: Src) -> Src {
        Src::contained_by(source, Src::extend(Src::Offset(bx(reference), true), i32::MAX, 0))
    }

    pub fn after(source: Src, reference: Src) -> Src {
        Src::contained_by(source, Src::extend(Src::Offset(bx(reference), false), 0, i32::MAX))
    }

    pub fn at_least(min: usize, subs: Vec<Src>) -> Src {
        if min == subs.len() {
            return Src::unordered(subs);
        }
        if min > subs.len() {
            let names: Vec<String> = subs.iter().map(|s| s.to_lucene(None)).collect();
            return Src::NoMatch(format!("Too few sources to match minimum of [{min}]: [{}]", names.join(", ")));
        }
        Src::AtLeast(subs, min)
    }

    pub fn fix_field(field: u32, s: Src) -> Src {
        Src::Field(field, bx(s))
    }

    /// Elasticsearch's `SourceIntervalsSource`: `s` (on `spec.field`) confirmed against each
    /// candidate doc's values re-analyzed in memory, for fields without positions
    /// (`match_only_text`). Elasticsearch wraps every leaf the field builds; wrapping a whole
    /// subtree of the field matches the same intervals with one in-memory segment per doc.
    pub fn source_confirmed(spec: Arc<crate::query::source_confirmed::SourceSpec>, s: Src) -> Src {
        Src::Source(spec, bx(s))
    }

    pub fn no_intervals(reason: impl Into<String>) -> Src {
        Src::NoMatch(reason.into())
    }

    /// Terms of `query` (its matcher; the rewrite method is ignored).
    pub fn multi_term(query: MultiTermQuery, max_expansions: usize, pattern: impl Into<String>) -> Src {
        Src::MultiTerm { query: Arc::new(query), max_expansions, pattern: pattern.into() }
    }

    /// Lucene's `IntervalBuilder.analyzeText` (Elasticsearch's `match` rule) over analyzed
    /// tokens `(term, position_increment, position_length)`: one term; terms combined as a
    /// phrase (`max_gaps == 0 && ordered`), ordered or unordered, filtered by `max_gaps` (`-1`:
    /// none), with position gaps as `extend` and stacked synonyms as disjunctions; token graphs
    /// (`synonym_graph`, `word_delimiter_graph`: tokens spanning several positions) split at
    /// their articulation points, each side-path region becoming a disjunction of its paths as
    /// phrases. Fails with `too_many_clauses` past [`MAX_DISJUNCTION_EXPANSION`] paths.
    pub fn analyzed(tokens: &[(Vec<u8>, u32, u32)], max_gaps: i32, ordered: bool) -> Result<Src> {
        Src::analyzed_with_limit(tokens, max_gaps, ordered, MAX_DISJUNCTION_EXPANSION)
    }

    /// [`analyzed`](Self::analyzed) with `indices.query.bool.max_clause_count`.
    pub fn analyzed_with_limit(tokens: &[(Vec<u8>, u32, u32)], max_gaps: i32, ordered: bool, max_clause_count: usize) -> Result<Src> {
        if tokens.is_empty() {
            return Ok(Src::NoMatch("No terms in analyzed text".into()));
        }
        if tokens.len() == 1 {
            return Ok(Src::term(tokens[0].0.clone()));
        }
        if tokens.iter().any(|t| t.2 > 1) {
            let clauses = analyze_graph(tokens, max_clause_count)?;
            return Ok(combine_sources(clauses, max_gaps, ordered));
        }
        let mut terms: Vec<Src> = Vec::new();
        if !tokens.iter().any(|t| t.1 == 0) {
            for (t, inc, _) in tokens {
                terms.push(extend_before(Src::term(t.clone()), *inc as i32 - 1));
            }
        } else {
            let mut syn: Vec<Src> = Vec::new();
            let mut spaces = 0i32;
            for (t, inc, _) in tokens {
                if *inc > 0 {
                    match syn.len() {
                        0 => {}
                        1 => terms.push(extend_before(syn.pop().unwrap(), spaces)),
                        _ => terms.push(extend_before(Src::or(std::mem::take(&mut syn)), spaces)),
                    }
                    syn.clear();
                    spaces = *inc as i32 - 1;
                }
                syn.push(Src::term(t.clone()));
            }
            if syn.len() == 1 {
                terms.push(extend_before(syn.pop().unwrap(), spaces));
            } else {
                terms.push(extend_before(Src::or(syn), spaces));
            }
        }
        Ok(combine_sources(terms, max_gaps, ordered))
    }

    // ---- structure ----

    /// The smallest number of positions an interval spans.
    pub fn min_extent(&self) -> i32 {
        match self {
            Src::Term(_) | Src::MultiTerm { .. } | Src::Offset(..) => 1,
            Src::Block(v) | Src::Ordered(v) | Src::Unordered(v) => v.iter().map(|s| s.min_extent()).fold(0i32, |a, b| a.wrapping_add(b)),
            Src::Or(v, _) => v.iter().map(|s| s.min_extent()).min().unwrap_or(i32::MAX),
            Src::Repeating(s, ..) | Src::MaxGaps(s, _) | Src::MaxWidth(s, _) | Src::Field(_, s) | Src::Source(_, s) => s.min_extent(),
            Src::Extend(s, b, a) => {
                let m = b.wrapping_add(s.min_extent()).wrapping_add(*a);
                if m < 0 { i32::MAX } else { m }
            }
            Src::Containing(big, _) => big.min_extent(),
            Src::ContainedBy(small, _) => small.min_extent(),
            Src::NotContaining(m, _) | Src::NotContainedBy(m, _) | Src::NonOverlapping(m, _) | Src::Overlapping(m, _) => m.min_extent(),
            Src::AtLeast(v, n) => {
                let mut e: Vec<i32> = v.iter().map(|s| s.min_extent()).collect();
                e.sort_unstable();
                e.iter().take(*n).fold(0i32, |a, b| a.wrapping_add(*b))
            }
            Src::NoMatch(_) | Src::Error(_) => 0,
        }
    }

    /// Lucene's `pullUpDisjunctions`.
    pub fn pull_up_disjunctions(&self) -> Vec<Src> {
        match self {
            Src::Or(v, true) => v.clone(),
            Src::Ordered(v) => pull_up_list(v, &|v| Src::Ordered(v)),
            Src::Unordered(v) => pull_up_list(v, &|v| Src::Unordered(v)),
            Src::MaxWidth(s, w) => pull_up_one(s, &|x| Src::MaxWidth(bx(x), *w)),
            Src::Extend(s, b, a) => {
                let inner = s.pull_up_disjunctions();
                if inner.is_empty() {
                    return vec![self.clone()];
                }
                dedup(inner.into_iter().map(|x| Src::Extend(bx(x), *b, *a)).collect())
            }
            Src::Containing(big, small) => pull_up_one(big, &|x| Src::Containing(bx(x), small.clone())),
            Src::ContainedBy(small, big) => pull_up_one(big, &|x| Src::ContainedBy(small.clone(), bx(x))),
            Src::Overlapping(s, r) => {
                pull_up_list(&[(**s).clone(), (**r).clone()], &|v| Src::Overlapping(bx(v[0].clone()), bx(v[1].clone())))
            }
            Src::Field(f, s) => {
                let inner = s.pull_up_disjunctions();
                if inner.len() == 1 {
                    return vec![self.clone()];
                }
                dedup(inner.into_iter().map(|x| Src::Field(*f, bx(x))).collect())
            }
            _ => vec![self.clone()],
        }
    }

    /// Lucene's `toString()`; `schema` resolves `Field` names.
    pub fn to_lucene(&self, schema: Option<&Schema>) -> String {
        let list = |v: &[Src]| v.iter().map(|s| s.to_lucene(schema)).collect::<Vec<_>>().join(",");
        match self {
            Src::Term(t) => String::from_utf8_lossy(t).into_owned(),
            Src::MultiTerm { pattern, .. } => format!("MultiTerm({pattern})"),
            Src::Block(v) => format!("BLOCK({})", list(v)),
            Src::Ordered(v) => format!("ORDERED({})", list(v)),
            Src::Unordered(v) => format!("UNORDERED({})", list(v)),
            Src::Or(v, _) => {
                let mut s: Vec<String> = v.iter().map(|x| x.to_lucene(schema)).collect();
                s.sort();
                format!("or({})", s.join(","))
            }
            Src::Repeating(s, n, name) => {
                let one = s.to_lucene(schema);
                let all = vec![one; *n].join(",");
                match name {
                    Some(nm) => format!("{nm}({all})"),
                    None => all,
                }
            }
            Src::MaxGaps(s, n) => format!("MAXGAPS/{n}({})", s.to_lucene(schema)),
            Src::MaxWidth(s, n) => format!("MAXWIDTH/{n}({})", s.to_lucene(schema)),
            Src::Extend(s, b, a) => format!("EXTEND({},{b},{a})", s.to_lucene(schema)),
            Src::Containing(a, b) => format!("CONTAINING({},{})", a.to_lucene(schema), b.to_lucene(schema)),
            Src::ContainedBy(a, b) => format!("CONTAINED_BY({},{})", a.to_lucene(schema), b.to_lucene(schema)),
            Src::NotContaining(a, b) => format!("NOT_CONTAINING({},{})", a.to_lucene(schema), b.to_lucene(schema)),
            Src::NotContainedBy(a, b) => format!("NOT_CONTAINED_BY({},{})", a.to_lucene(schema), b.to_lucene(schema)),
            Src::NonOverlapping(a, b) => format!("NON_OVERLAPPING({},{})", a.to_lucene(schema), b.to_lucene(schema)),
            Src::Overlapping(a, b) => format!("OVERLAPPING({},{})", a.to_lucene(schema), b.to_lucene(schema)),
            Src::Offset(s, true) => format!("PRECEDING({})", s.to_lucene(schema)),
            Src::Offset(s, false) => format!("FOLLOWING({})", s.to_lucene(schema)),
            Src::AtLeast(v, n) => format!("AtLeast({}~{n})", list(v)),
            Src::Source(_, s) => s.to_lucene(schema),
            Src::Field(f, s) => {
                let name = schema.map_or_else(|| f.to_string(), |sc| sc.name(*f));
                format!("FIELD({name},{})", s.to_lucene(schema))
            }
            Src::NoMatch(r) => format!("NOMATCH({r})"),
            Src::Error(e) => format!("ERROR({e})"),
        }
    }

    /// An unambiguous structural key (equality and hashing).
    fn key(&self) -> String {
        let list = |v: &[Src]| v.iter().map(|s| s.key()).collect::<Vec<_>>().join(",");
        match self {
            Src::Term(t) => format!("T{t:?}"),
            Src::MultiTerm { pattern, query, max_expansions } => format!("M{}:{pattern:?}:{max_expansions}:{:?}", query.field, mt_kind(query)),
            Src::Block(v) => format!("B({})", list(v)),
            Src::Ordered(v) => format!("O({})", list(v)),
            Src::Unordered(v) => format!("U({})", list(v)),
            Src::Or(v, p) => {
                let mut k: Vec<String> = v.iter().map(|x| x.key()).collect();
                k.sort();
                format!("D{p}({})", k.join(","))
            }
            Src::Repeating(s, n, _) => format!("R{n}({})", s.key()),
            Src::MaxGaps(s, n) => format!("G{n}({})", s.key()),
            Src::MaxWidth(s, n) => format!("W{n}({})", s.key()),
            Src::Extend(s, b, a) => format!("E{b},{a}({})", s.key()),
            Src::Containing(a, b) => format!("C({},{})", a.key(), b.key()),
            Src::ContainedBy(a, b) => format!("CB({},{})", a.key(), b.key()),
            Src::NotContaining(a, b) => format!("NC({},{})", a.key(), b.key()),
            Src::NotContainedBy(a, b) => format!("NCB({},{})", a.key(), b.key()),
            Src::NonOverlapping(a, b) => format!("NO({},{})", a.key(), b.key()),
            Src::Overlapping(a, b) => format!("OV({},{})", a.key(), b.key()),
            Src::Offset(s, p) => format!("P{p}({})", s.key()),
            Src::AtLeast(v, n) => format!("A{n}({})", list(v)),
            Src::Field(f, s) => format!("F{f}({})", s.key()),
            Src::Source(spec, s) => format!("S{}({})", spec.field, s.key()),
            Src::NoMatch(r) => format!("N({r:?})"),
            Src::Error(e) => format!("X({e:?})"),
        }
    }

    fn first_error(&self) -> Option<&str> {
        match self {
            Src::Error(e) => Some(e),
            Src::Block(v) | Src::Ordered(v) | Src::Unordered(v) | Src::Or(v, _) | Src::AtLeast(v, _) => v.iter().find_map(|s| s.first_error()),
            Src::Repeating(s, ..) | Src::MaxGaps(s, _) | Src::MaxWidth(s, _) | Src::Extend(s, ..) | Src::Offset(s, _) | Src::Field(_, s) | Src::Source(_, s) => s.first_error(),
            Src::Containing(a, b)
            | Src::ContainedBy(a, b)
            | Src::NotContaining(a, b)
            | Src::NotContainedBy(a, b)
            | Src::NonOverlapping(a, b)
            | Src::Overlapping(a, b) => a.first_error().or_else(|| b.first_error()),
            _ => None,
        }
    }

    fn visit(&self, field: u32, v: &mut dyn QueryVisitor) {
        match self {
            Src::Term(t) => v.term(field, t),
            Src::MultiTerm { query, .. } => {
                let q = query.clone();
                v.multi_term(field, Arc::new(move |t: &[u8]| q.matches_term(t)));
            }
            Src::Block(xs) | Src::Ordered(xs) | Src::Unordered(xs) | Src::Or(xs, _) | Src::AtLeast(xs, _) => xs.iter().for_each(|s| s.visit(field, v)),
            Src::Repeating(s, ..) | Src::MaxGaps(s, _) | Src::MaxWidth(s, _) | Src::Extend(s, ..) | Src::Offset(s, _) => s.visit(field, v),
            Src::Field(f, s) => s.visit(*f, v),
            Src::Source(_, s) => s.visit(field, v),
            // Subtrahends are must-not.
            Src::NotContaining(a, _) | Src::NotContainedBy(a, _) | Src::NonOverlapping(a, _) => a.visit(field, v),
            Src::Containing(a, b) | Src::ContainedBy(a, b) | Src::Overlapping(a, b) => {
                a.visit(field, v);
                b.visit(field, v);
            }
            Src::NoMatch(_) | Src::Error(_) => {}
        }
    }

    /// The iterator of this source in a segment (`None`: nothing can match there).
    pub(crate) fn iter(&self, field: u32, seg: &SegmentContext, s: &Searcher) -> Result<Option<BI>> {
        Ok(match self {
            Src::Term(t) => term_iter(field, t, seg, s)?,
            Src::MultiTerm { query, max_expansions, pattern } => {
                let Some(tr) = seg.reader.terms(field) else { return Ok(None) };
                check_positions(tr.has_positions(), field, s)?;
                seg.ensure_positions(field)?;
                let mut subs: Vec<BI> = Vec::new();
                let mut err = None;
                query.enumerate_terms(tr, &mut |_t, st| {
                    if subs.len() >= *max_expansions {
                        err = Some(Error::illegal_state(format!(
                            "Automaton [{pattern}] expanded to too many terms (limit {max_expansions})"
                        )));
                        return false;
                    }
                    subs.push(Box::new(TermII::new(tr.postings(&st, PostingsFlags::POSITIONS), st.doc_freq, st.total_term_freq)));
                    true
                });
                if let Some(e) = err {
                    return Err(e);
                }
                if subs.is_empty() { None } else { Some(Box::new(DisjII::new(subs))) }
            }
            Src::Block(v) => conj_subs(v, field, seg, s)?.map(|subs| Box::new(BlockII::new(subs)) as BI),
            Src::Ordered(v) => conj_subs(v, field, seg, s)?.map(|subs| Box::new(OrderedII::new(subs)) as BI),
            Src::Unordered(v) => conj_subs(v, field, seg, s)?.map(|subs| Box::new(UnorderedII::new(subs)) as BI),
            Src::Or(v, _) => {
                let mut subs = Vec::new();
                for x in v {
                    if let Some(it) = x.iter(field, seg, s)? {
                        subs.push(it);
                    }
                }
                if subs.is_empty() { None } else { Some(Box::new(DisjII::new(subs))) }
            }
            Src::Repeating(x, n, _) => x.iter(field, seg, s)?.map(|it| Box::new(DupII::new(it, *n)) as BI),
            Src::MaxGaps(x, n) => x.iter(field, seg, s)?.map(|it| Box::new(FilterII { inp: it, gaps: Some(*n), width: None }) as BI),
            Src::MaxWidth(x, n) => x.iter(field, seg, s)?.map(|it| Box::new(FilterII { inp: it, gaps: None, width: Some(*n) }) as BI),
            Src::Extend(x, b, a) => x.iter(field, seg, s)?.map(|it| Box::new(ExtendII { inp: it, before: *b, after: *a, positioned: false }) as BI),
            Src::Offset(x, before) => x.iter(field, seg, s)?.map(|it| Box::new(OffsetII { inp: it, before: *before }) as BI),
            Src::Containing(a, b) => pair(a, b, field, seg, s)?.map(|(a, b)| Box::new(FilteringII::new(a, b, Rel::Containing)) as BI),
            Src::ContainedBy(a, b) => pair(a, b, field, seg, s)?.map(|(a, b)| Box::new(FilteringII::new(a, b, Rel::ContainedBy)) as BI),
            Src::Overlapping(a, b) => pair(a, b, field, seg, s)?.map(|(a, b)| Box::new(FilteringII::new(a, b, Rel::Overlapping)) as BI),
            Src::NotContaining(a, b) => difference(a, b, field, seg, s, Diff::NotContaining)?,
            Src::NotContainedBy(a, b) => difference(a, b, field, seg, s, Diff::NotContainedBy)?,
            Src::NonOverlapping(a, b) => difference(a, b, field, seg, s, Diff::NonOverlapping)?,
            Src::AtLeast(v, n) => {
                let mut subs = Vec::new();
                for x in v {
                    if let Some(it) = x.iter(field, seg, s)? {
                        subs.push(it);
                    }
                }
                if subs.len() < *n { None } else { Some(Box::new(AtLeastII::new(subs, *n))) }
            }
            Src::Field(f, x) => x.iter(*f, seg, s)?,
            Src::Source(spec, x) => crate::query::source_confirmed::source_iter(spec, x, seg, s)?,
            Src::NoMatch(_) => None,
            Src::Error(e) => return Err(Error::illegal(e.clone())),
        })
    }
}

fn mt_kind(q: &MultiTermQuery) -> String {
    q.lucene_string(&Schema::new())
}

fn repeating(s: Src, n: usize) -> Src {
    if n == 1 { s } else { Src::Repeating(bx(s), n, None) }
}

fn name_repeating(s: Src, name: &'static str) -> Src {
    match s {
        Src::Repeating(x, n, _) => Src::Repeating(x, n, Some(name)),
        other => other,
    }
}

/// Lucene's `GraphTokenStreamFiniteStrings` + `IntervalBuilder.analyzeGraph`. States are
/// positions (a token spanning `n` positions goes from its position to `n` further; gaps add
/// states), renumbered densely, so every transition goes forward and a live state is an
/// articulation point exactly when no transition jumps over it. Paths are enumerated depth-first in token order,
/// as Lucene's `FiniteStringsIterator` walks transitions sorted by label (the token's index).
fn analyze_graph(tokens: &[(Vec<u8>, u32, u32)], max_clause_count: usize) -> Result<Vec<Src>> {
    // Build: transitions (from, to, token index) and each token's increment within a path.
    let mut trans: Vec<(u32, u32, usize)> = Vec::with_capacity(tokens.len());
    let mut incs: Vec<u32> = Vec::with_capacity(tokens.len());
    let (mut pos, mut prev_incr, mut gap, mut last_state) = (-1i64, 1u32, 0i64, -1i64);
    for (id, (_, incr, len)) in tokens.iter().enumerate() {
        if pos == -1 && *incr < 1 {
            return Err(Error::illegal("Malformed TokenStream, start token can't have increment less than 1"));
        }
        if *incr == 0 {
            if gap > 0 {
                pos -= gap;
            }
        } else {
            pos += 1;
            gap = *incr as i64 - 1;
        }
        let end = pos + (*len).max(1) as i64 + gap;
        last_state = last_state.max(end);
        trans.push((pos as u32, end.min(u32::MAX as i64) as u32, id));
        pos += gap;
        incs.push(if *incr == 0 { prev_incr } else { *incr });
        if *incr > 0 {
            prev_incr = *incr;
        }
    }
    // Renumber states densely (gaps may skip many positions), keeping their order.
    let mut states: Vec<u32> = trans.iter().flat_map(|&(f, t, _)| [f, t]).chain([0, last_state.min(u32::MAX as i64) as u32]).collect();
    states.sort_unstable();
    states.dedup();
    let idx = |s: u32| states.binary_search(&s).unwrap() as u32;
    for t in trans.iter_mut() {
        *t = (idx(t.0), idx(t.1), t.2);
    }
    let fin = states.len() as u32 - 1;
    // Live states: reachable from 0 and reaching the final state.
    let n = fin as usize + 1;
    let mut fwd = vec![false; n];
    fwd[0] = true;
    let mut order: Vec<usize> = (0..trans.len()).collect();
    order.sort_by_key(|&i| (trans[i].0, trans[i].2));
    for &i in &order {
        let (f, t, _) = trans[i];
        if fwd[f as usize] {
            fwd[t as usize] = true;
        }
    }
    let mut back = vec![false; n];
    back[fin as usize] = true;
    for &i in order.iter().rev() {
        let (f, t, _) = trans[i];
        if back[t as usize] {
            back[f as usize] = true;
        }
    }
    let live: Vec<(u32, u32, usize)> = order.iter().map(|&i| trans[i]).filter(|&(f, t, _)| fwd[f as usize] && back[t as usize]).collect();
    if live.is_empty() {
        return Ok(Vec::new());
    }
    // Articulation points: live states no transition jumps over.
    let mut jumped = vec![false; n + 1];
    let mut diff = vec![0i32; n + 1];
    for &(f, t, _) in &live {
        if t > f + 1 {
            diff[f as usize + 1] += 1;
            diff[t as usize] -= 1;
        }
    }
    let mut run = 0;
    for (v, j) in jumped.iter_mut().enumerate().take(n) {
        run += diff[v];
        *j = run > 0;
    }
    let mut is_state = vec![false; n];
    for &(f, t, _) in &live {
        is_state[f as usize] = true;
        is_state[t as usize] = true;
    }
    let points: Vec<u32> = (1..fin).filter(|&v| is_state[v as usize] && !jumped[v as usize]).collect();
    // Transitions by source state, in token order.
    let mut out: Vec<Vec<(u32, usize)>> = vec![Vec::new(); n];
    for &(f, t, id) in &live {
        out[f as usize].push((t, id));
    }
    for o in out.iter_mut() {
        o.sort_by_key(|&(_, id)| id);
    }
    let path_terms = |path: &[usize]| -> Vec<Src> {
        path.iter().map(|&id| extend_before(Src::term(tokens[id].0.clone()), incs[id] as i32 - 1)).collect()
    };
    let mut clauses = Vec::new();
    let mut start = 0u32;
    for i in 0..=points.len() {
        let end = if i < points.len() { points[i] } else { fin };
        let side = {
            let o = &out[start as usize];
            o.iter().any(|&(t, _)| t != o[0].0)
        };
        let mut paths: Vec<Src> = Vec::new();
        let mut too_many = false;
        finite_strings(&out, start, end, &mut |path| {
            if !side {
                // One region path: Lucene takes the first (stacked one-position synonyms on
                // it are dropped).
                clauses.extend(path_terms(path));
                return false;
            }
            if paths.len() >= max_clause_count {
                too_many = true;
                return false;
            }
            paths.push(combine_sources(path_terms(path), 0, true));
            true
        });
        if too_many {
            return Err(Error::too_many_clauses(max_clause_count));
        }
        if side && !paths.is_empty() {
            clauses.push(Src::or(paths));
        }
        start = end;
    }
    Ok(clauses)
}

/// Every path of token indices from `start` to `end`, depth first in transition order;
/// `emit` returns false to stop.
fn finite_strings(out: &[Vec<(u32, usize)>], start: u32, end: u32, emit: &mut dyn FnMut(&[usize]) -> bool) {
    let mut path: Vec<usize> = Vec::new();
    // (state, next transition index)
    let mut stack: Vec<(u32, usize)> = vec![(start, 0)];
    while let Some(top) = stack.last_mut() {
        let (state, k) = *top;
        let Some(&(to, id)) = out[state as usize].get(k) else {
            stack.pop();
            path.pop();
            continue;
        };
        top.1 += 1;
        path.push(id);
        if to == end || out[to as usize].is_empty() {
            if !emit(&path) {
                return;
            }
            path.pop();
        } else {
            stack.push((to, 0));
        }
    }
}

fn extend_before(s: Src, spaces: i32) -> Src {
    if spaces == 0 { s } else { Src::extend(s, spaces, 0) }
}

fn combine_sources(sources: Vec<Src>, max_gaps: i32, ordered: bool) -> Src {
    if sources.is_empty() {
        return Src::NoMatch("No terms in analyzed text".into());
    }
    if sources.len() == 1 {
        return sources.into_iter().next().unwrap();
    }
    if max_gaps == 0 && ordered {
        return Src::phrase(sources);
    }
    let inner = if ordered { Src::ordered(sources) } else { Src::unordered(sources) };
    if max_gaps == -1 {
        return inner;
    }
    Src::max_gaps(max_gaps, inner)
}

fn dedup(v: Vec<Src>) -> Vec<Src> {
    let mut seen = HashSet::new();
    v.into_iter().filter(|s| seen.insert(s.key())).collect()
}

fn simplify_or(subs: Vec<Src>) -> Vec<Src> {
    let mut out = Vec::new();
    for s in subs {
        if matches!(s, Src::Or(..)) {
            out.extend(s.pull_up_disjunctions());
        } else {
            out.push(s);
        }
    }
    dedup(out)
}

fn flatten_blocks(v: Vec<Src>) -> Vec<Src> {
    let mut out = Vec::new();
    for s in v {
        match s {
            Src::Block(inner) => out.extend(inner),
            other => out.push(other),
        }
    }
    out
}

/// Lucene's `Disjunctions.splitDisjunctions`: singleton-width disjuncts stay together.
fn split_disjunctions(s: &Src) -> Vec<Src> {
    let mut singles = Vec::new();
    let mut others = Vec::new();
    for d in s.pull_up_disjunctions() {
        if d.min_extent() == 1 {
            singles.push(d);
        } else {
            others.push(d);
        }
    }
    let mut out = Vec::new();
    if !singles.is_empty() {
        out.push(Src::or(singles));
    }
    out.extend(others);
    out
}

/// Lucene's `Disjunctions.pullUp(List, function)`.
fn pull_up_list(sources: &[Src], f: &dyn Fn(Vec<Src>) -> Src) -> Vec<Src> {
    let mut rewritten: Vec<Vec<Src>> = vec![Vec::new()];
    for s in sources {
        let d = split_disjunctions(s);
        if d.len() == 1 {
            for l in &mut rewritten {
                l.push(d[0].clone());
            }
        } else {
            if rewritten.len() * d.len() > MAX_DISJUNCTION_EXPANSION {
                return vec![Src::Error("Too many disjunctions to expand".into())];
            }
            let mut next = Vec::with_capacity(rewritten.len() * d.len());
            for disj in &d {
                for l in &rewritten {
                    let mut l = l.clone();
                    l.push(disj.clone());
                    next.push(l);
                }
            }
            rewritten = next;
        }
    }
    rewritten.into_iter().map(f).collect()
}

/// Lucene's `Disjunctions.pullUp(source, function)`.
fn pull_up_one(s: &Src, f: &dyn Fn(Src) -> Src) -> Vec<Src> {
    split_disjunctions(s).into_iter().map(f).collect()
}

fn check_positions(has: bool, field: u32, s: &Searcher) -> Result<()> {
    if !has {
        return Err(Error::illegal(format!(
            "Cannot create an IntervalIterator over field {} because it has no indexed positions",
            s.schema().name(field)
        )));
    }
    Ok(())
}

fn term_iter(field: u32, t: &[u8], seg: &SegmentContext, s: &Searcher) -> Result<Option<BI>> {
    let Some(tr) = seg.reader.terms(field) else { return Ok(None) };
    check_positions(tr.has_positions(), field, s)?;
    let Some(st) = tr.seek_exact(t) else { return Ok(None) };
    seg.ensure_positions(field)?;
    Ok(Some(Box::new(TermII::new(tr.postings(&st, PostingsFlags::POSITIONS), st.doc_freq, st.total_term_freq))))
}

fn conj_subs(v: &[Src], field: u32, seg: &SegmentContext, s: &Searcher) -> Result<Option<Vec<BI>>> {
    let mut out = Vec::with_capacity(v.len());
    for x in v {
        match x.iter(field, seg, s)? {
            Some(it) => out.push(it),
            None => return Ok(None),
        }
    }
    Ok(Some(out))
}

fn pair(a: &Src, b: &Src, field: u32, seg: &SegmentContext, s: &Searcher) -> Result<Option<(BI, BI)>> {
    let Some(x) = a.iter(field, seg, s)? else { return Ok(None) };
    let Some(y) = b.iter(field, seg, s)? else { return Ok(None) };
    Ok(Some((x, y)))
}

fn difference(a: &Src, b: &Src, field: u32, seg: &SegmentContext, s: &Searcher, kind: Diff) -> Result<Option<BI>> {
    let Some(x) = a.iter(field, seg, s)? else { return Ok(None) };
    Ok(Some(match b.iter(field, seg, s)? {
        None => x,
        Some(y) => Box::new(RelII { a: x, b: y, bpos: false, kind }),
    }))
}

// ---- iterators (Lucene's `IntervalIterator`s; docs are `-1` before the first move) ----

pub(crate) trait II: Send {
    fn doc(&self) -> i64;
    fn next_doc(&mut self) -> i64;
    fn advance(&mut self, target: i64) -> i64;
    fn cost(&self) -> u64;
    fn start(&self) -> i32;
    fn end(&self) -> i32;
    fn gaps(&self) -> i32;
    fn width(&self) -> i32 {
        self.end().wrapping_sub(self.start()).wrapping_add(1)
    }
    fn next_interval(&mut self) -> i32;
    fn match_cost(&self) -> f32;
}

pub(crate) type BI = Box<dyn II>;

pub(crate) fn jdoc(d: u32) -> i64 {
    if d == u32::MAX { -1 } else { d as i64 }
}

struct TermII {
    c: PostingsCursor,
    pos: i32,
    upto: i32,
    cost: f32,
}

impl TermII {
    fn new(c: PostingsCursor, df: u32, ttf: u64) -> TermII {
        let exp = ttf as f32 / df.max(1) as f32;
        TermII { c, pos: -1, upto: 0, cost: 128.0 + exp * 7.0 }
    }
    fn reset(&mut self) {
        if self.c.doc() == NO_MORE_DOCS {
            self.upto = -1;
            self.pos = NO_MORE_INTERVALS;
        } else {
            self.upto = self.c.freq() as i32;
            self.pos = -1;
        }
    }
}

impl II for TermII {
    fn doc(&self) -> i64 {
        jdoc(self.c.doc())
    }
    fn next_doc(&mut self) -> i64 {
        self.c.next();
        self.reset();
        self.doc()
    }
    fn advance(&mut self, t: i64) -> i64 {
        self.c.advance(t.min(NO_MORE) as u32);
        self.reset();
        self.doc()
    }
    fn cost(&self) -> u64 {
        self.c.cost()
    }
    fn start(&self) -> i32 {
        self.pos
    }
    fn end(&self) -> i32 {
        self.pos
    }
    fn gaps(&self) -> i32 {
        0
    }
    fn next_interval(&mut self) -> i32 {
        if self.upto <= 0 {
            self.pos = NO_MORE_INTERVALS;
            return self.pos;
        }
        self.upto -= 1;
        self.pos = self.c.next_position() as i32;
        self.pos
    }
    fn match_cost(&self) -> f32 {
        self.cost
    }
}

/// Lucene's `ConjunctionDISI` over the subs' docs (lead: cheapest).
struct ConjDocs {
    lead: usize,
}

impl ConjDocs {
    fn new(subs: &[BI]) -> ConjDocs {
        let lead = (0..subs.len()).min_by_key(|&i| subs[i].cost()).unwrap_or(0);
        ConjDocs { lead }
    }
    fn do_next(&self, subs: &mut [BI], mut doc: i64) -> i64 {
        'outer: loop {
            if doc >= NO_MORE {
                return NO_MORE;
            }
            for i in 0..subs.len() {
                if i == self.lead {
                    continue;
                }
                if subs[i].doc() < doc {
                    let n = subs[i].advance(doc);
                    if n > doc {
                        doc = subs[self.lead].advance(n);
                        continue 'outer;
                    }
                }
            }
            return doc;
        }
    }
    fn next_doc(&self, subs: &mut [BI]) -> i64 {
        let d = subs[self.lead].next_doc();
        self.do_next(subs, d)
    }
    fn advance(&self, subs: &mut [BI], t: i64) -> i64 {
        let d = subs[self.lead].advance(t);
        self.do_next(subs, d)
    }
    fn doc(&self, subs: &[BI]) -> i64 {
        subs[self.lead].doc()
    }
}

/// Lucene's `DisjunctionDISIApproximation` over the subs' docs.
struct DisjDocs {
    doc: i64,
}

impl DisjDocs {
    fn next_doc(&mut self, subs: &mut [BI]) -> i64 {
        let cur = self.doc;
        for s in subs.iter_mut() {
            if s.doc() == cur {
                s.next_doc();
            }
        }
        self.doc = subs.iter().map(|s| s.doc()).min().unwrap_or(NO_MORE);
        self.doc
    }
    fn advance(&mut self, subs: &mut [BI], t: i64) -> i64 {
        for s in subs.iter_mut() {
            if s.doc() < t {
                s.advance(t);
            }
        }
        self.doc = subs.iter().map(|s| s.doc()).min().unwrap_or(NO_MORE);
        self.doc
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cur {
    Empty,
    Exhausted,
    Sub(usize),
}

struct DisjII {
    subs: Vec<BI>,
    docs: DisjDocs,
    /// Interval queue: (end, Reverse(start), sub) ordered smallest first.
    queue: BinaryHeap<Reverse<(i32, Reverse<i32>, usize)>>,
    current: Cur,
    cost: u64,
    match_cost: f32,
}

impl DisjII {
    fn new(subs: Vec<BI>) -> DisjII {
        let cost = subs.iter().map(|s| s.cost()).sum();
        let match_cost = subs.iter().map(|s| s.cost() as f32).sum();
        DisjII { subs, docs: DisjDocs { doc: -1 }, queue: BinaryHeap::new(), current: Cur::Empty, cost, match_cost }
    }
    fn push(&mut self, i: usize) {
        let s = &self.subs[i];
        self.queue.push(Reverse((s.end(), Reverse(s.start()), i)));
    }
    fn reset(&mut self) {
        self.queue.clear();
        let doc = self.docs.doc;
        for i in 0..self.subs.len() {
            if self.subs[i].doc() == doc {
                self.subs[i].next_interval();
                self.push(i);
            }
        }
        self.current = Cur::Empty;
    }
    fn top(&self) -> Option<usize> {
        self.queue.peek().map(|r| r.0.2)
    }
}

impl II for DisjII {
    fn doc(&self) -> i64 {
        self.docs.doc
    }
    fn next_doc(&mut self) -> i64 {
        let d = self.docs.next_doc(&mut self.subs);
        self.reset();
        d
    }
    fn advance(&mut self, t: i64) -> i64 {
        let d = self.docs.advance(&mut self.subs, t);
        self.reset();
        d
    }
    fn cost(&self) -> u64 {
        self.cost
    }
    fn start(&self) -> i32 {
        match self.current {
            Cur::Empty => -1,
            Cur::Exhausted => NO_MORE_INTERVALS,
            Cur::Sub(i) => self.subs[i].start(),
        }
    }
    fn end(&self) -> i32 {
        match self.current {
            Cur::Empty => -1,
            Cur::Exhausted => NO_MORE_INTERVALS,
            Cur::Sub(i) => self.subs[i].end(),
        }
    }
    fn gaps(&self) -> i32 {
        match self.current {
            Cur::Sub(i) => self.subs[i].gaps(),
            _ => 0,
        }
    }
    fn next_interval(&mut self) -> i32 {
        if matches!(self.current, Cur::Empty | Cur::Exhausted) {
            if let Some(t) = self.top() {
                self.current = Cur::Sub(t);
            }
            return self.start();
        }
        let (start, end) = (self.start(), self.end());
        while let Some(t) = self.top() {
            let it = &self.subs[t];
            let contains = start >= it.start() && start <= it.end() && end >= it.start() && end <= it.end();
            if !contains {
                break;
            }
            self.queue.pop();
            if self.subs[t].next_interval() != NO_MORE_INTERVALS {
                self.push(t);
            }
        }
        match self.top() {
            None => {
                self.current = Cur::Exhausted;
                NO_MORE_INTERVALS
            }
            Some(t) => {
                self.current = Cur::Sub(t);
                self.subs[t].start()
            }
        }
    }
    fn match_cost(&self) -> f32 {
        self.match_cost
    }
}

struct BlockII {
    subs: Vec<BI>,
    conj: ConjDocs,
    start: i32,
    end: i32,
    cost: f32,
}

impl BlockII {
    fn new(subs: Vec<BI>) -> BlockII {
        let conj = ConjDocs::new(&subs);
        let cost = subs.iter().map(|s| s.match_cost()).sum();
        BlockII { subs, conj, start: -1, end: -1, cost }
    }
}

impl II for BlockII {
    fn doc(&self) -> i64 {
        self.conj.doc(&self.subs)
    }
    fn next_doc(&mut self) -> i64 {
        let d = self.conj.next_doc(&mut self.subs);
        if d != NO_MORE {
            self.start = -1;
            self.end = -1;
        }
        d
    }
    fn advance(&mut self, t: i64) -> i64 {
        let d = self.conj.advance(&mut self.subs, t);
        if d != NO_MORE {
            self.start = -1;
            self.end = -1;
        }
        d
    }
    fn cost(&self) -> u64 {
        self.subs[self.conj.lead].cost()
    }
    fn start(&self) -> i32 {
        self.start
    }
    fn end(&self) -> i32 {
        self.end
    }
    fn gaps(&self) -> i32 {
        0
    }
    fn next_interval(&mut self) -> i32 {
        let subs = &mut self.subs;
        if subs[0].next_interval() == NO_MORE_INTERVALS {
            self.start = NO_MORE_INTERVALS;
            self.end = NO_MORE_INTERVALS;
            return NO_MORE_INTERVALS;
        }
        let mut i = 1;
        while i < subs.len() {
            while subs[i].start() <= subs[i - 1].end() {
                if subs[i].next_interval() == NO_MORE_INTERVALS {
                    self.start = NO_MORE_INTERVALS;
                    self.end = NO_MORE_INTERVALS;
                    return NO_MORE_INTERVALS;
                }
            }
            if subs[i].start() == subs[i - 1].end().wrapping_add(1) {
                i += 1;
            } else {
                if subs[0].next_interval() == NO_MORE_INTERVALS {
                    self.start = NO_MORE_INTERVALS;
                    self.end = NO_MORE_INTERVALS;
                    return NO_MORE_INTERVALS;
                }
                i = 1;
            }
        }
        self.start = subs[0].start();
        self.end = subs[subs.len() - 1].end();
        self.start
    }
    fn match_cost(&self) -> f32 {
        self.cost
    }
}

struct OrderedII {
    subs: Vec<BI>,
    conj: ConjDocs,
    start: i32,
    end: i32,
    slop: i32,
    i: usize,
    cost: f32,
}

impl OrderedII {
    fn new(subs: Vec<BI>) -> OrderedII {
        let conj = ConjDocs::new(&subs);
        let cost = subs.iter().map(|s| s.match_cost()).sum();
        OrderedII { subs, conj, start: -1, end: -1, slop: -1, i: 1, cost }
    }
    fn reset(&mut self) {
        self.subs[0].next_interval();
        self.i = 1;
        self.start = -1;
        self.end = -1;
        self.slop = -1;
    }
}

impl II for OrderedII {
    fn doc(&self) -> i64 {
        self.conj.doc(&self.subs)
    }
    fn next_doc(&mut self) -> i64 {
        let d = self.conj.next_doc(&mut self.subs);
        if d != NO_MORE {
            self.reset();
        }
        d
    }
    fn advance(&mut self, t: i64) -> i64 {
        let d = self.conj.advance(&mut self.subs, t);
        if d != NO_MORE {
            self.reset();
        }
        d
    }
    fn cost(&self) -> u64 {
        self.subs[self.conj.lead].cost()
    }
    fn start(&self) -> i32 {
        self.start
    }
    fn end(&self) -> i32 {
        self.end
    }
    fn gaps(&self) -> i32 {
        self.slop
    }
    fn next_interval(&mut self) -> i32 {
        self.start = NO_MORE_INTERVALS;
        self.end = NO_MORE_INTERVALS;
        self.slop = NO_MORE_INTERVALS;
        let mut last_start = i32::MAX;
        let mut minimizing = false;
        let n = self.subs.len();
        let mut ci = self.i;
        loop {
            let mut prev_end = self.subs[ci - 1].end();
            loop {
                if prev_end >= last_start {
                    self.i = ci;
                    return self.start;
                }
                if ci == n {
                    break;
                }
                let cur = &mut self.subs[ci];
                if minimizing && cur.start() > prev_end {
                    break;
                }
                loop {
                    if cur.end() >= last_start {
                        self.i = ci;
                        return self.start;
                    }
                    let cs = cur.next_interval();
                    if cs == NO_MORE_INTERVALS {
                        self.i = ci;
                        return self.start;
                    }
                    if cs > prev_end {
                        break;
                    }
                }
                ci += 1;
                prev_end = self.subs[ci - 1].end();
            }
            let start = self.subs[0].start();
            self.start = start;
            if start == NO_MORE_INTERVALS {
                self.i = ci;
                self.end = NO_MORE_INTERVALS;
                return NO_MORE_INTERVALS;
            }
            let end = self.subs[n - 1].end();
            self.end = end;
            let mut slop = end.wrapping_sub(start).wrapping_add(1);
            for s in &self.subs {
                slop = slop.wrapping_sub(s.width());
            }
            self.slop = slop;
            ci = 1;
            if self.subs[0].next_interval() == NO_MORE_INTERVALS {
                self.i = ci;
                return self.start;
            }
            last_start = self.subs[n - 1].start();
            minimizing = true;
        }
    }
    fn match_cost(&self) -> f32 {
        self.cost
    }
}

/// A queue ordered by (start asc, end desc): Lucene's proximity queues.
type StartQueue = BinaryHeap<Reverse<(i32, Reverse<i32>, usize)>>;

fn start_key(it: &dyn II, i: usize) -> Reverse<(i32, Reverse<i32>, usize)> {
    Reverse((it.start(), Reverse(it.end()), i))
}

struct UnorderedII {
    subs: Vec<BI>,
    conj: ConjDocs,
    queue: StartQueue,
    start: i32,
    end: i32,
    slop: i32,
    queue_end: i32,
    cost: f32,
}

impl UnorderedII {
    fn new(subs: Vec<BI>) -> UnorderedII {
        let conj = ConjDocs::new(&subs);
        let cost = subs.iter().map(|s| s.match_cost()).sum();
        UnorderedII { subs, conj, queue: BinaryHeap::new(), start: -1, end: -1, slop: 0, queue_end: -1, cost }
    }
    fn reset(&mut self) {
        self.queue_end = -1;
        self.start = -1;
        self.end = -1;
        self.queue.clear();
        for i in 0..self.subs.len() {
            if self.subs[i].next_interval() == NO_MORE_INTERVALS {
                break;
            }
            self.queue.push(start_key(self.subs[i].as_ref(), i));
            self.update_right(i);
        }
    }
    fn update_right(&mut self, i: usize) {
        let e = self.subs[i].end();
        if e > self.queue_end {
            self.queue_end = e;
        }
    }
    fn top(&self) -> usize {
        self.queue.peek().unwrap().0.2
    }
}

impl II for UnorderedII {
    fn doc(&self) -> i64 {
        self.conj.doc(&self.subs)
    }
    fn next_doc(&mut self) -> i64 {
        let d = self.conj.next_doc(&mut self.subs);
        if d != NO_MORE {
            self.reset();
        }
        d
    }
    fn advance(&mut self, t: i64) -> i64 {
        let d = self.conj.advance(&mut self.subs, t);
        if d != NO_MORE {
            self.reset();
        }
        d
    }
    fn cost(&self) -> u64 {
        self.subs[self.conj.lead].cost()
    }
    fn start(&self) -> i32 {
        self.start
    }
    fn end(&self) -> i32 {
        self.end
    }
    fn gaps(&self) -> i32 {
        self.slop
    }
    fn next_interval(&mut self) -> i32 {
        let n = self.subs.len();
        while self.queue.len() == n && self.subs[self.top()].start() == self.start {
            let i = self.queue.pop().unwrap().0.2;
            if self.subs[i].next_interval() != NO_MORE_INTERVALS {
                self.queue.push(start_key(self.subs[i].as_ref(), i));
                self.update_right(i);
            }
        }
        if self.queue.len() < n {
            self.start = NO_MORE_INTERVALS;
            self.end = NO_MORE_INTERVALS;
            return NO_MORE_INTERVALS;
        }
        loop {
            self.start = self.subs[self.top()].start();
            self.end = self.queue_end;
            let mut slop = self.width();
            for s in &self.subs {
                slop = slop.wrapping_sub(s.width());
            }
            self.slop = slop;
            if self.subs[self.top()].end() == self.end {
                return self.start;
            }
            let i = self.queue.pop().unwrap().0.2;
            if self.subs[i].next_interval() != NO_MORE_INTERVALS {
                self.queue.push(start_key(self.subs[i].as_ref(), i));
                self.update_right(i);
            }
            if !(self.queue.len() == n && self.end == self.queue_end) {
                break;
            }
        }
        self.start
    }
    fn match_cost(&self) -> f32 {
        self.cost
    }
}

struct AtLeastII {
    subs: Vec<BI>,
    docs: DisjDocs,
    min: usize,
    proximity: StartQueue,
    /// (end asc, start desc).
    background: BinaryHeap<Reverse<(i32, Reverse<i32>, usize)>>,
    start: i32,
    end: i32,
    queue_end: i32,
    slop: i32,
    cost: f32,
}

impl AtLeastII {
    fn new(subs: Vec<BI>, min: usize) -> AtLeastII {
        let cost = subs.iter().map(|s| s.match_cost()).sum();
        AtLeastII {
            subs,
            docs: DisjDocs { doc: -1 },
            min,
            proximity: BinaryHeap::new(),
            background: BinaryHeap::new(),
            start: -1,
            end: -1,
            queue_end: -1,
            slop: 0,
            cost,
        }
    }
    fn bg_key(&self, i: usize) -> Reverse<(i32, Reverse<i32>, usize)> {
        let s = &self.subs[i];
        Reverse((s.end(), Reverse(s.start()), i))
    }
    fn update_right(&mut self, i: usize) {
        let e = self.subs[i].end();
        if e > self.queue_end {
            self.queue_end = e;
        }
    }
    fn reset(&mut self) {
        self.proximity.clear();
        self.background.clear();
        let doc = self.docs.doc;
        for i in 0..self.subs.len() {
            if self.subs[i].doc() == doc && self.subs[i].next_interval() != NO_MORE_INTERVALS {
                let k = self.bg_key(i);
                self.background.push(k);
            }
        }
        self.queue_end = -1;
        for _ in 0..self.min {
            let Some(Reverse((_, _, i))) = self.background.pop() else { break };
            self.proximity.push(start_key(self.subs[i].as_ref(), i));
            self.update_right(i);
        }
        self.start = -1;
        self.end = -1;
    }
    fn ptop(&self) -> usize {
        self.proximity.peek().unwrap().0.2
    }
}

impl II for AtLeastII {
    fn doc(&self) -> i64 {
        self.docs.doc
    }
    fn next_doc(&mut self) -> i64 {
        let d = self.docs.next_doc(&mut self.subs);
        self.reset();
        d
    }
    fn advance(&mut self, t: i64) -> i64 {
        let d = self.docs.advance(&mut self.subs, t);
        self.reset();
        d
    }
    fn cost(&self) -> u64 {
        self.subs.iter().map(|s| s.cost()).sum()
    }
    fn start(&self) -> i32 {
        self.start
    }
    fn end(&self) -> i32 {
        self.end
    }
    fn gaps(&self) -> i32 {
        self.slop
    }
    fn next_interval(&mut self) -> i32 {
        while self.proximity.len() == self.min && self.subs[self.ptop()].start() == self.start {
            let i = self.proximity.pop().unwrap().0.2;
            if self.subs[i].next_interval() != NO_MORE_INTERVALS {
                let k = self.bg_key(i);
                self.background.push(k);
                let Reverse((_, _, next)) = self.background.pop().unwrap();
                self.proximity.push(start_key(self.subs[next].as_ref(), next));
                self.update_right(next);
            }
        }
        if self.proximity.len() < self.min {
            self.start = NO_MORE_INTERVALS;
            self.end = NO_MORE_INTERVALS;
            return NO_MORE_INTERVALS;
        }
        loop {
            self.start = self.subs[self.ptop()].start();
            self.end = self.queue_end;
            let mut slop = self.width();
            for r in self.proximity.iter() {
                slop = slop.wrapping_sub(self.subs[r.0.2].width());
            }
            self.slop = slop;
            if self.subs[self.ptop()].end() == self.end {
                return self.start;
            }
            let lead = self.proximity.pop().unwrap().0.2;
            if self.subs[lead].next_interval() != NO_MORE_INTERVALS {
                let k = self.bg_key(lead);
                self.background.push(k);
            }
            if let Some(Reverse((_, _, next))) = self.background.pop() {
                self.proximity.push(start_key(self.subs[next].as_ref(), next));
                self.update_right(next);
            }
            if !(self.proximity.len() == self.min && self.end == self.queue_end) {
                break;
            }
        }
        self.start
    }
    fn match_cost(&self) -> f32 {
        self.cost
    }
}

#[derive(Clone, Copy)]
enum Rel {
    Containing,
    ContainedBy,
    Overlapping,
}

/// Containing / contained-by / overlapping: `a`'s intervals that relate to some of `b`'s.
struct FilteringII {
    subs: Vec<BI>,
    conj: ConjDocs,
    bpos: bool,
    rel: Rel,
    cost: f32,
}

impl FilteringII {
    fn new(a: BI, b: BI, rel: Rel) -> FilteringII {
        let subs = vec![a, b];
        let conj = ConjDocs::new(&subs);
        let cost = subs.iter().map(|s| s.match_cost()).sum();
        FilteringII { subs, conj, bpos: false, rel, cost }
    }
    fn reset(&mut self) {
        self.bpos = self.subs[1].next_interval() != NO_MORE_INTERVALS;
    }
}

impl II for FilteringII {
    fn doc(&self) -> i64 {
        self.conj.doc(&self.subs)
    }
    fn next_doc(&mut self) -> i64 {
        let d = self.conj.next_doc(&mut self.subs);
        if d != NO_MORE {
            self.reset();
        }
        d
    }
    fn advance(&mut self, t: i64) -> i64 {
        let d = self.conj.advance(&mut self.subs, t);
        if d != NO_MORE {
            self.reset();
        }
        d
    }
    fn cost(&self) -> u64 {
        self.subs[self.conj.lead].cost()
    }
    fn start(&self) -> i32 {
        if self.bpos { self.subs[0].start() } else { NO_MORE_INTERVALS }
    }
    fn end(&self) -> i32 {
        if self.bpos { self.subs[0].end() } else { NO_MORE_INTERVALS }
    }
    fn gaps(&self) -> i32 {
        self.subs[0].gaps()
    }
    fn next_interval(&mut self) -> i32 {
        if !self.bpos {
            return NO_MORE_INTERVALS;
        }
        let (x, y) = self.subs.split_at_mut(1);
        let (a, b) = (&mut x[0], &mut y[0]);
        while a.next_interval() != NO_MORE_INTERVALS {
            match self.rel {
                Rel::Containing => {
                    while b.start() < a.start() && b.end() < a.end() {
                        if b.next_interval() == NO_MORE_INTERVALS {
                            self.bpos = false;
                            return NO_MORE_INTERVALS;
                        }
                    }
                    if a.start() <= b.start() && a.end() >= b.end() {
                        return a.start();
                    }
                }
                Rel::ContainedBy => {
                    while b.end() < a.end() {
                        if b.next_interval() == NO_MORE_INTERVALS {
                            self.bpos = false;
                            return NO_MORE_INTERVALS;
                        }
                    }
                    if b.start() <= a.start() {
                        return a.start();
                    }
                }
                Rel::Overlapping => {
                    while b.end() < a.start() {
                        if b.next_interval() == NO_MORE_INTERVALS {
                            self.bpos = false;
                            return NO_MORE_INTERVALS;
                        }
                    }
                    if b.start() <= a.end() {
                        return a.start();
                    }
                }
            }
        }
        if !matches!(self.rel, Rel::Containing) {
            self.bpos = false;
        }
        NO_MORE_INTERVALS
    }
    fn match_cost(&self) -> f32 {
        self.cost
    }
}

#[derive(Clone, Copy)]
enum Diff {
    NotContaining,
    NotContainedBy,
    NonOverlapping,
}

/// Minuend intervals without a relation to the subtrahend's (docs come from the minuend).
struct RelII {
    a: BI,
    b: BI,
    bpos: bool,
    kind: Diff,
}

impl RelII {
    fn reset(&mut self) {
        let doc = self.a.doc();
        self.bpos = self.b.doc() == doc || (self.b.doc() < doc && self.b.advance(doc) == doc);
    }
}

impl II for RelII {
    fn doc(&self) -> i64 {
        self.a.doc()
    }
    fn next_doc(&mut self) -> i64 {
        let d = self.a.next_doc();
        self.reset();
        d
    }
    fn advance(&mut self, t: i64) -> i64 {
        let d = self.a.advance(t);
        self.reset();
        d
    }
    fn cost(&self) -> u64 {
        self.a.cost()
    }
    fn start(&self) -> i32 {
        self.a.start()
    }
    fn end(&self) -> i32 {
        self.a.end()
    }
    fn gaps(&self) -> i32 {
        self.a.gaps()
    }
    fn next_interval(&mut self) -> i32 {
        if !self.bpos {
            return self.a.next_interval();
        }
        let (a, b) = (&mut self.a, &mut self.b);
        while a.next_interval() != NO_MORE_INTERVALS {
            match self.kind {
                Diff::NotContaining => {
                    while b.start() < a.start() && b.end() < a.end() {
                        if b.next_interval() == NO_MORE_INTERVALS {
                            self.bpos = false;
                            return a.start();
                        }
                    }
                    if b.start() > a.end() {
                        return a.start();
                    }
                }
                Diff::NotContainedBy => {
                    while b.end() < a.end() {
                        if b.next_interval() == NO_MORE_INTERVALS {
                            return a.start();
                        }
                    }
                    if a.start() < b.start() {
                        return a.start();
                    }
                }
                Diff::NonOverlapping => {
                    while b.end() < a.start() {
                        if b.next_interval() == NO_MORE_INTERVALS {
                            self.bpos = false;
                            return a.start();
                        }
                    }
                    if b.start() > a.end() {
                        return a.start();
                    }
                }
            }
        }
        NO_MORE_INTERVALS
    }
    fn match_cost(&self) -> f32 {
        self.a.match_cost() + self.b.match_cost()
    }
}

/// `max_gaps` / `max_width` filters.
struct FilterII {
    inp: BI,
    gaps: Option<i32>,
    width: Option<i32>,
}

impl II for FilterII {
    fn doc(&self) -> i64 {
        self.inp.doc()
    }
    fn next_doc(&mut self) -> i64 {
        self.inp.next_doc()
    }
    fn advance(&mut self, t: i64) -> i64 {
        self.inp.advance(t)
    }
    fn cost(&self) -> u64 {
        self.inp.cost()
    }
    fn start(&self) -> i32 {
        self.inp.start()
    }
    fn end(&self) -> i32 {
        self.inp.end()
    }
    fn gaps(&self) -> i32 {
        self.inp.gaps()
    }
    fn next_interval(&mut self) -> i32 {
        loop {
            let n = self.inp.next_interval();
            if n == NO_MORE_INTERVALS {
                return n;
            }
            let ok = match (self.gaps, self.width) {
                (Some(g), _) => self.inp.gaps() <= g,
                (_, Some(w)) => self.inp.end().wrapping_sub(self.inp.start()).wrapping_add(1) <= w,
                _ => true,
            };
            if ok {
                return n;
            }
        }
    }
    fn match_cost(&self) -> f32 {
        self.inp.match_cost()
    }
}

struct ExtendII {
    inp: BI,
    before: i32,
    after: i32,
    positioned: bool,
}

impl II for ExtendII {
    fn doc(&self) -> i64 {
        self.inp.doc()
    }
    fn next_doc(&mut self) -> i64 {
        self.positioned = false;
        self.inp.next_doc()
    }
    fn advance(&mut self, t: i64) -> i64 {
        self.positioned = false;
        self.inp.advance(t)
    }
    fn cost(&self) -> u64 {
        self.inp.cost()
    }
    fn start(&self) -> i32 {
        if !self.positioned {
            return -1;
        }
        let s = self.inp.start();
        if s == NO_MORE_INTERVALS {
            return NO_MORE_INTERVALS;
        }
        0.max(s.wrapping_sub(self.before))
    }
    fn end(&self) -> i32 {
        if !self.positioned {
            return -1;
        }
        let e = self.inp.end();
        if e == NO_MORE_INTERVALS {
            return NO_MORE_INTERVALS;
        }
        let e = e.wrapping_add(self.after);
        if e < 0 || e == NO_MORE_INTERVALS { NO_MORE_INTERVALS - 1 } else { e }
    }
    fn gaps(&self) -> i32 {
        self.inp.gaps()
    }
    fn next_interval(&mut self) -> i32 {
        self.positioned = true;
        self.inp.next_interval();
        self.start()
    }
    fn match_cost(&self) -> f32 {
        self.inp.match_cost()
    }
}

struct OffsetII {
    inp: BI,
    before: bool,
}

impl II for OffsetII {
    fn doc(&self) -> i64 {
        self.inp.doc()
    }
    fn next_doc(&mut self) -> i64 {
        self.inp.next_doc()
    }
    fn advance(&mut self, t: i64) -> i64 {
        self.inp.advance(t)
    }
    fn cost(&self) -> u64 {
        self.inp.cost()
    }
    fn start(&self) -> i32 {
        if self.before {
            let p = self.inp.start();
            if p == -1 {
                return -1;
            }
            if p == NO_MORE_INTERVALS {
                return NO_MORE_INTERVALS;
            }
            0.max(p - 1)
        } else {
            let p = self.inp.end().wrapping_add(1);
            if p == 0 {
                return -1;
            }
            if p < 0 {
                return i32::MAX;
            }
            if p == i32::MAX {
                return i32::MAX - 1;
            }
            p
        }
    }
    fn end(&self) -> i32 {
        self.start()
    }
    fn gaps(&self) -> i32 {
        0
    }
    fn next_interval(&mut self) -> i32 {
        self.inp.next_interval();
        self.start()
    }
    fn match_cost(&self) -> f32 {
        self.inp.match_cost()
    }
}

/// A source repeated `n` times in an ordered / unordered conjunction: windows of `n`
/// consecutive intervals of one iterator (Lucene's `DuplicateIntervalIterator`).
struct DupII {
    inp: BI,
    cache: Vec<i32>,
    n: usize,
    base: usize,
    started: bool,
    exhausted: bool,
}

impl DupII {
    fn new(inp: BI, n: usize) -> DupII {
        DupII { inp, cache: vec![-1; n * 2], n, base: 0, started: false, exhausted: false }
    }
    fn cache_next(&mut self, line: usize) -> i32 {
        if self.inp.next_interval() == NO_MORE_INTERVALS {
            self.exhausted = true;
            return NO_MORE_INTERVALS;
        }
        self.cache[line * 2] = self.inp.start();
        self.cache[line * 2 + 1] = self.inp.end();
        self.start()
    }
    fn restart(&mut self) {
        self.started = false;
        self.exhausted = false;
        self.cache.iter_mut().for_each(|c| *c = -1);
    }
}

impl II for DupII {
    fn doc(&self) -> i64 {
        self.inp.doc()
    }
    fn next_doc(&mut self) -> i64 {
        self.restart();
        self.inp.next_doc()
    }
    fn advance(&mut self, t: i64) -> i64 {
        self.restart();
        self.inp.advance(t)
    }
    fn cost(&self) -> u64 {
        self.inp.cost()
    }
    fn start(&self) -> i32 {
        if self.exhausted { NO_MORE_INTERVALS } else { self.cache[(self.base % self.n) * 2] }
    }
    fn end(&self) -> i32 {
        if self.exhausted { NO_MORE_INTERVALS } else { self.cache[((self.base + self.n - 1) % self.n) * 2 + 1] }
    }
    fn width(&self) -> i32 {
        // Lucene sums `start - end + 1` of the cached intervals (kept as is: it drives gaps).
        let mut w = 0i32;
        for i in 0..self.n {
            let p = (self.base + i) % self.n;
            w = w.wrapping_add(self.cache[p * 2].wrapping_sub(self.cache[p * 2 + 1]).wrapping_add(1));
        }
        w
    }
    fn gaps(&self) -> i32 {
        self.end().wrapping_sub(self.start()).wrapping_add(1).wrapping_sub(self.width())
    }
    fn next_interval(&mut self) -> i32 {
        if self.exhausted {
            return NO_MORE_INTERVALS;
        }
        if !self.started {
            for i in 0..self.n {
                if self.cache_next(i) == NO_MORE_INTERVALS {
                    return NO_MORE_INTERVALS;
                }
            }
            self.base = 0;
            self.started = true;
            return self.start();
        }
        let insert = (self.base + self.n) % self.n;
        self.base = (self.base + 1) % self.n;
        self.cache_next(insert)
    }
    fn match_cost(&self) -> f32 {
        self.inp.match_cost()
    }
}

// ---- query ----

/// An interval query on one field.
#[derive(Clone, Debug)]
pub struct IntervalQuery {
    pub field: u32,
    pub source: IntervalsSource,
    /// Saturation pivot (1).
    pub pivot: f32,
}

impl IntervalQuery {
    pub fn new(field: u32, source: IntervalsSource) -> IntervalQuery {
        IntervalQuery { field, source, pivot: 1.0 }
    }
}

impl Query for IntervalQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        if let Some(e) = self.source.first_error() {
            return Err(Error::illegal(e.to_string()));
        }
        Ok(Box::new(IntervalWeight {
            q: self.clone(),
            boost,
            desc: self.lucene_string(s.schema()),
            min_extent: self.source.min_extent(),
            searcher: s.clone(),
        }))
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        format!("{}:{}", schema.name(self.field), self.source.to_lucene(Some(schema)))
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.source.visit(self.field, v);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct IntervalWeight {
    q: IntervalQuery,
    boost: f32,
    desc: String,
    min_extent: i32,
    searcher: Searcher,
}

impl IntervalWeight {
    fn make(&self, seg: &SegmentContext) -> Result<Option<IntervalScorer>> {
        let Some(it) = self.q.source.iter(self.q.field, seg, &self.searcher)? else { return Ok(None) };
        Ok(Some(IntervalScorer { it, min_extent: self.min_extent, boost: self.boost, pivot: self.q.pivot, freq: 0.0, scored: -2, checked: (-2, false) }))
    }
}

impl Weight for IntervalWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.make(seg)?.map(|s| Box::new(s) as Box<dyn Scorer>))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        if let Some(mut s) = self.make(seg)?
            && s.advance(doc) == doc
        {
            let freq = s.freq_now();
            let score = s.score();
            return Ok(Explanation::matched(
                score,
                "Saturation function on interval frequency, computed as w * S / (S + k) from:",
                vec![
                    Explanation::leaf(self.boost, "w, weight of this function"),
                    Explanation::leaf(self.q.pivot, "k, pivot feature value that would give a score contribution equal to w/2"),
                    Explanation::leaf(freq, format!("S, the sloppy frequency of the interval query {}", self.desc)),
                ],
            ));
        }
        Ok(Explanation::no_match("no matching intervals", vec![]))
    }
}

struct IntervalScorer {
    it: BI,
    min_extent: i32,
    boost: f32,
    pivot: f32,
    freq: f32,
    scored: i64,
    /// The doc `matches` last answered for, and the answer (it moves the intervals, so a
    /// second call must not ask again).
    checked: (i64, bool),
}

impl IntervalScorer {
    fn freq_now(&mut self) -> f32 {
        let d = self.it.doc();
        if self.scored != d {
            self.scored = d;
            let mut f = 0f32;
            loop {
                let len = self.it.end().wrapping_sub(self.it.start()).wrapping_add(1);
                f = (f as f64 + 1.0 / (len.wrapping_sub(self.min_extent).wrapping_add(1)).max(1) as f64) as f32;
                if self.it.next_interval() == NO_MORE_INTERVALS {
                    break;
                }
            }
            self.freq = f;
        }
        self.freq
    }
}

impl Scorable for IntervalScorer {
    fn score(&mut self) -> f32 {
        let f = self.freq_now();
        self.boost * (1.0 - self.pivot / (self.pivot + f))
    }
}

impl Scorer for IntervalScorer {
    fn doc(&self) -> u32 {
        let d = self.it.doc();
        if d < 0 { u32::MAX } else { d as u32 }
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
        self.it.cost()
    }
    fn has_two_phase(&self) -> bool {
        true
    }
    fn approx_next(&mut self) -> u32 {
        let d = self.it.next_doc();
        d.min(NO_MORE) as u32
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        let cur = self.it.doc();
        if cur >= t as i64 {
            return cur as u32;
        }
        self.it.advance(t as i64).min(NO_MORE) as u32
    }
    fn matches(&mut self) -> bool {
        let d = self.it.doc();
        if self.checked.0 != d {
            self.checked = (d, self.it.next_interval() != NO_MORE_INTERVALS);
        }
        self.checked.1
    }
    fn match_cost(&self) -> f32 {
        self.it.match_cost()
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        self.boost
    }
}

/// An interval query as a `QueryRef`.
pub fn interval_query(field: u32, source: IntervalsSource) -> QueryRef {
    Arc::new(IntervalQuery::new(field, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Src {
        Src::term(s.as_bytes().to_vec())
    }

    #[test]
    fn factories_rewrite_like_lucene() {
        assert_eq!(Src::ordered(vec![t("a"), t("a")]).to_lucene(None), "ORDERED(a,a)");
        assert_eq!(Src::ordered(vec![t("a"), t("b"), t("b")]).to_lucene(None), "ORDERED(a,b,b)");
        assert_eq!(Src::unordered(vec![t("b"), t("a"), t("b")]).to_lucene(None), "UNORDERED(b,b,a)");
        // Disjunctions of single terms stay inside; wider disjuncts are pulled up.
        assert_eq!(Src::ordered(vec![Src::or(vec![t("x"), t("y")]), t("b")]).to_lucene(None), "ORDERED(or(x,y),b)");
        let wide = Src::or(vec![Src::phrase_terms(["p", "q"]), t("y")]);
        // `ordered` keeps the disjunction; enclosing filters distribute over it.
        assert_eq!(Src::ordered(vec![wide.clone(), t("b")]).to_lucene(None), "ORDERED(or(BLOCK(p,q),y),b)");
        assert_eq!(
            Src::max_gaps(1, Src::ordered(vec![wide, t("b")])).to_lucene(None),
            "or(MAXGAPS/1(ORDERED(BLOCK(p,q),b)),MAXGAPS/1(ORDERED(y,b)))"
        );
        assert_eq!(Src::phrase(vec![t("a"), Src::phrase_terms(["b", "c"])]).to_lucene(None), "BLOCK(a,b,c)");
        assert_eq!(Src::max_gaps(2, Src::or(vec![t("a"), t("b")])).to_lucene(None), "or(MAXGAPS/2(a),MAXGAPS/2(b))");
        assert_eq!(Src::at_least(3, vec![t("a"), t("b")]).to_lucene(None), "NOMATCH(Too few sources to match minimum of [3]: [a, b])");
        assert_eq!(Src::or(vec![t("a"), t("a")]).to_lucene(None), "a");
    }
}

#[cfg(test)]
mod graph_tests {
    use super::*;

    fn tok(t: &str, inc: u32, len: u32) -> (Vec<u8>, u32, u32) {
        (t.as_bytes().to_vec(), inc, len)
    }

    #[test]
    fn graph_regions_and_limits() {
        // a (b c | d) e: one side-path region between articulation points.
        let toks = [tok("a", 1, 1), tok("b", 1, 1), tok("d", 0, 2), tok("c", 1, 1), tok("e", 1, 1)];
        let s = Src::analyzed(&toks, -1, true).unwrap();
        assert_eq!(s.to_lucene(None), "ORDERED(a,or(BLOCK(b,c),d),e)");
        // A stacked one-position synonym on a region without side paths is dropped (Lucene
        // takes the region's first path).
        let toks = [tok("a", 1, 1), tok("x", 0, 1), tok("b", 1, 1), tok("c", 0, 2), tok("d", 1, 1), tok("e", 1, 1)];
        let s = Src::analyzed(&toks, 0, true).unwrap();
        assert_eq!(s.to_lucene(None), "or(BLOCK(a,b,d,e),BLOCK(a,c,e))");
        // A token starting inside another's span with nothing leading there is dead.
        let toks = [tok("a", 1, 1), tok("b", 1, 2), tok("c", 2, 1)];
        assert_eq!(Src::analyzed(&toks, 0, true).unwrap().to_lucene(None), "NOMATCH(No terms in analyzed text)");
        // Gaps before a path token become `extend`.
        let toks = [tok("a", 1, 1), tok("b", 2, 2), tok("c", 0, 1), tok("d", 1, 1)];
        let s = Src::analyzed(&toks, -1, false).unwrap();
        assert!(s.to_lucene(None).contains("EXTEND"), "{}", s.to_lucene(None));
        // Huge gaps cost nothing (states are renumbered).
        let toks = [tok("a", 1, 1), tok("b", u32::MAX, 2), tok("c", 0, 1), tok("d", 1, 1)];
        assert!(Src::analyzed(&toks, -1, true).is_ok());
        // Malformed streams and path explosions fail as Lucene does.
        let e = Src::analyzed(&[tok("a", 0, 2), tok("b", 1, 1)], -1, true).unwrap_err();
        assert!(e.to_string().contains("start token can't have increment less than 1"), "{e}");
        let mut many = Vec::new();
        // Overlapping two-position synonyms leave no articulation point: Fibonacci many paths.
        for i in 0..16 {
            many.push(tok(&format!("w{i}"), 1, 1));
            many.push(tok(&format!("s{i}"), 0, 2));
        }
        let e = Src::analyzed_with_limit(&many, -1, true, 100).unwrap_err();
        assert!(e.to_string().contains("maxClauseCount is set to 100"), "{e}");
    }
}
