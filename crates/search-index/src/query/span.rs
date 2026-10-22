//! Span queries (Lucene's `spans` package): term, near (ordered / unordered with slop), or,
//! not (with pre/post), first, containing, within, field masking and multi-term wrappers.
//!
//! A span query is a tree of [`SpanNode`]s evaluated per segment into [`Spans`]: iterators
//! over docs (an approximation) and, per doc, over match intervals `[start, end)` with a
//! `width` (the slop actually used). Matching logic follows Lucene's `NearSpansOrdered`
//! ("stretch to order"), `NearSpansUnordered` (sliding window over a priority queue),
//! `SpanOr` (union by doc, then by position), `FilterSpans` (not / first) and
//! `ContainSpans`, because the widths feed the score: a doc's frequency is
//! `Σ 1 / (1 + width)` over its matches, scored by the field's similarity with the summed idf
//! of every distinct term in the tree.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use search_codec::{NO_MORE_DOCS, PostingsCursor, PostingsFlags};

use crate::error::{Error, Result};
use crate::query::basic::MatchNoneQuery;
use crate::query::multiterm::MultiTermQuery;
use crate::query::term::{Norms, TermStates, scoring_stats, term_text};
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::similarity::{SimScorer, TermStats};
use crate::util::java::float_to_string;

/// No more positions in the current doc.
pub const NO_MORE_POSITIONS: i32 = i32::MAX;

/// Match intervals of a span query in one segment.
pub trait Spans: Send {
    fn doc(&self) -> u32;
    /// Moves to the next candidate doc (positions unread).
    fn approx_next(&mut self) -> u32;
    fn approx_advance(&mut self, target: u32) -> u32;
    fn cost(&self) -> u64;
    /// Whether the candidate doc has a match; when true the positions are reset so that
    /// `next_start` returns the first match.
    fn matches(&mut self) -> bool;
    fn next_start(&mut self) -> i32;
    /// `-1` before the first `next_start` of the doc.
    fn start(&self) -> i32;
    fn end(&self) -> i32;
    fn width(&self) -> i32;
}

/// A node of a span query.
#[derive(Clone, Debug)]
pub enum SpanNode {
    Term { field: u32, term: Vec<u8> },
    Near { clauses: Vec<SpanNode>, slop: u32, in_order: bool },
    Or(Vec<SpanNode>),
    Not { include: Box<SpanNode>, exclude: Box<SpanNode>, pre: u32, post: u32 },
    First { inner: Box<SpanNode>, end: u32 },
    Containing { big: Box<SpanNode>, little: Box<SpanNode> },
    Within { big: Box<SpanNode>, little: Box<SpanNode> },
    /// `field_masking_span`: the inner spans reported as `field`.
    Mask { inner: Box<SpanNode>, field: u32 },
    /// `span_multi`: expanded to an `Or` of terms when rewritten.
    Multi(Box<MultiTermQuery>),
    /// A gap of `width` positions inside an ordered near (`span_gap`).
    Gap { field: u32, width: u32 },
}

impl SpanNode {
    /// The field the node scores on (Lucene `getField`).
    pub fn field(&self) -> u32 {
        match self {
            SpanNode::Term { field, .. } | SpanNode::Gap { field, .. } | SpanNode::Mask { field, .. } => *field,
            SpanNode::Near { clauses, .. } | SpanNode::Or(clauses) => clauses.first().map_or(0, |c| c.field()),
            SpanNode::Not { include, .. } => include.field(),
            SpanNode::First { inner, .. } => inner.field(),
            SpanNode::Containing { big, .. } | SpanNode::Within { big, .. } => big.field(),
            SpanNode::Multi(m) => m.field,
        }
    }

    fn check_fields(&self) -> Result<()> {
        match self {
            SpanNode::Near { clauses, .. } | SpanNode::Or(clauses) => {
                let f = self.field();
                for c in clauses {
                    if c.field() != f {
                        return Err(Error::illegal("Clauses must have same field."));
                    }
                    c.check_fields()?;
                }
                Ok(())
            }
            SpanNode::Not { include, exclude, .. } => {
                if include.field() != exclude.field() {
                    return Err(Error::illegal("Clauses must have same field."));
                }
                include.check_fields()?;
                exclude.check_fields()
            }
            SpanNode::Containing { big, little } | SpanNode::Within { big, little } => {
                if big.field() != little.field() {
                    return Err(Error::illegal("big and little not same field"));
                }
                big.check_fields()?;
                little.check_fields()
            }
            SpanNode::First { inner, .. } | SpanNode::Mask { inner, .. } => inner.check_fields(),
            _ => Ok(()),
        }
    }

    fn terms(&self, out: &mut Vec<(u32, Vec<u8>)>) {
        match self {
            SpanNode::Term { field, term } => out.push((*field, term.clone())),
            SpanNode::Near { clauses, .. } | SpanNode::Or(clauses) => clauses.iter().for_each(|c| c.terms(out)),
            SpanNode::Not { include, .. } => include.terms(out),
            SpanNode::First { inner, .. } | SpanNode::Mask { inner, .. } => inner.terms(out),
            SpanNode::Containing { big, little } | SpanNode::Within { big, little } => {
                big.terms(out);
                little.terms(out);
            }
            SpanNode::Multi(_) | SpanNode::Gap { .. } => {}
        }
    }

    /// Every term the spans read, including `span_not` excludes (which don't score).
    fn all_terms(&self, out: &mut Vec<(u32, Vec<u8>)>) {
        match self {
            SpanNode::Term { field, term } => out.push((*field, term.clone())),
            SpanNode::Near { clauses, .. } | SpanNode::Or(clauses) => clauses.iter().for_each(|c| c.all_terms(out)),
            SpanNode::Not { include, exclude, .. } => {
                include.all_terms(out);
                exclude.all_terms(out);
            }
            SpanNode::First { inner, .. } | SpanNode::Mask { inner, .. } => inner.all_terms(out),
            SpanNode::Containing { big, little } | SpanNode::Within { big, little } => {
                big.all_terms(out);
                little.all_terms(out);
            }
            SpanNode::Multi(_) | SpanNode::Gap { .. } => {}
        }
    }

    fn has_multi(&self) -> bool {
        match self {
            SpanNode::Multi(_) => true,
            SpanNode::Near { clauses, .. } | SpanNode::Or(clauses) => clauses.iter().any(|c| c.has_multi()),
            SpanNode::Not { include, exclude, .. } => include.has_multi() || exclude.has_multi(),
            SpanNode::First { inner, .. } | SpanNode::Mask { inner, .. } => inner.has_multi(),
            SpanNode::Containing { big, little } | SpanNode::Within { big, little } => big.has_multi() || little.has_multi(),
            _ => false,
        }
    }

    fn expand(&self, s: &Searcher) -> Result<SpanNode> {
        Ok(match self {
            SpanNode::Multi(m) => {
                let terms = crate::query::multiterm::collect_terms(m, s)?;
                SpanNode::Or(terms.into_iter().map(|t| SpanNode::Term { field: m.field, term: t }).collect())
            }
            SpanNode::Near { clauses, slop, in_order } => SpanNode::Near {
                clauses: clauses.iter().map(|c| c.expand(s)).collect::<Result<_>>()?,
                slop: *slop,
                in_order: *in_order,
            },
            SpanNode::Or(clauses) => SpanNode::Or(clauses.iter().map(|c| c.expand(s)).collect::<Result<_>>()?),
            SpanNode::Not { include, exclude, pre, post } => {
                SpanNode::Not { include: Box::new(include.expand(s)?), exclude: Box::new(exclude.expand(s)?), pre: *pre, post: *post }
            }
            SpanNode::First { inner, end } => SpanNode::First { inner: Box::new(inner.expand(s)?), end: *end },
            SpanNode::Mask { inner, field } => SpanNode::Mask { inner: Box::new(inner.expand(s)?), field: *field },
            SpanNode::Containing { big, little } => {
                SpanNode::Containing { big: Box::new(big.expand(s)?), little: Box::new(little.expand(s)?) }
            }
            SpanNode::Within { big, little } => SpanNode::Within { big: Box::new(big.expand(s)?), little: Box::new(little.expand(s)?) },
            other => other.clone(),
        })
    }

    fn lucene(&self, schema: &Schema) -> String {
        match self {
            SpanNode::Term { field, term } => format!("{}:{}", schema.name(*field), term_text(term)),
            SpanNode::Near { clauses, slop, in_order } => {
                let parts: Vec<String> = clauses.iter().map(|c| c.lucene(schema)).collect();
                format!("spanNear([{}], {slop}, {in_order})", parts.join(", "))
            }
            SpanNode::Or(clauses) => {
                let parts: Vec<String> = clauses.iter().map(|c| c.lucene(schema)).collect();
                format!("spanOr([{}])", parts.join(", "))
            }
            SpanNode::Not { include, exclude, pre, post } => {
                format!("spanNot({}, {}, {pre}, {post})", include.lucene(schema), exclude.lucene(schema))
            }
            SpanNode::First { inner, end } => format!("spanFirst({}, {end})", inner.lucene(schema)),
            SpanNode::Containing { big, little } => format!("SpanContaining({}, {})", big.lucene(schema), little.lucene(schema)),
            SpanNode::Within { big, little } => format!("SpanWithin({}, {})", big.lucene(schema), little.lucene(schema)),
            SpanNode::Mask { inner, field } => format!("mask({}) as {}", inner.lucene(schema), schema.name(*field)),
            SpanNode::Multi(m) => format!("SpanMultiTermQueryWrapper({})", m.lucene_string(schema)),
            SpanNode::Gap { field, width } => format!("SpanGap({}:{width})", schema.name(*field)),
        }
    }
}

/// Builds the spans of a node in a segment.
fn build(node: &SpanNode, seg: &SegmentContext, states: &BTreeMap<(u32, Vec<u8>), TermStates>) -> Result<Option<Box<dyn Spans>>> {
    Ok(match node {
        SpanNode::Term { field, term } => {
            let Some(tr) = seg.reader.terms(*field) else { return Ok(None) };
            if !tr.has_positions() {
                return Err(Error::illegal_state(format!(
                    "field \"{}\" was indexed without position data; cannot run SpanTermQuery (term={})",
                    field,
                    term_text(term)
                )));
            }
            let Some(st) = states.get(&(*field, term.clone())).and_then(|s| s.get(seg)) else { return Ok(None) };
            seg.ensure_positions(*field)?;
            Some(Box::new(TermSpans::new(tr.postings(st, PostingsFlags::POSITIONS))))
        }
        SpanNode::Gap { width, .. } => Some(Box::new(GapSpans { width: *width as i32, doc: u32::MAX, start: -1 })),
        SpanNode::Near { clauses, slop, in_order } => {
            let mut subs = Vec::with_capacity(clauses.len());
            for c in clauses {
                match build(c, seg, states)? {
                    Some(s) => subs.push(s),
                    None => return Ok(None),
                }
            }
            if subs.len() == 1 {
                return Ok(subs.pop());
            }
            if *in_order {
                Some(Box::new(NearOrdered { c: Conj::new(subs), slop: *slop as i32, ms: -1, me: -1, mw: -1, pending: false }))
            } else {
                Some(Box::new(NearUnordered::new(Conj::new(subs), *slop as i32)))
            }
        }
        SpanNode::Or(clauses) => {
            let mut subs = Vec::new();
            for c in clauses {
                if let Some(s) = build(c, seg, states)? {
                    subs.push(s);
                }
            }
            match subs.len() {
                0 => None,
                1 => subs.pop(),
                _ => Some(Box::new(OrSpans::new(subs))),
            }
        }
        SpanNode::Not { include, exclude, pre, post } => {
            let Some(inc) = build(include, seg, states)? else { return Ok(None) };
            match build(exclude, seg, states)? {
                None => Some(inc),
                Some(exc) => Some(Box::new(FilterSpans::new(
                    inc,
                    Filter::Not { exc, pre: *pre as i32, post: *post as i32, last: u32::MAX, last_match: false },
                ))),
            }
        }
        SpanNode::First { inner, end } => {
            let Some(inc) = build(inner, seg, states)? else { return Ok(None) };
            Some(Box::new(FilterSpans::new(inc, Filter::First { end: *end as i32 })))
        }
        SpanNode::Mask { inner, .. } => build(inner, seg, states)?,
        SpanNode::Containing { big, little } | SpanNode::Within { big, little } => {
            let (Some(b), Some(l)) = (build(big, seg, states)?, build(little, seg, states)?) else { return Ok(None) };
            let containing = matches!(node, SpanNode::Containing { .. });
            Some(Box::new(ContainSpans { c: Conj::new(vec![b, l]), containing, at_first: false, exhausted: false }))
        }
        SpanNode::Multi(_) => return Err(Error::illegal_state("span_multi must be rewritten before execution")),
    })
}

struct TermSpans {
    c: PostingsCursor,
    count: u32,
    start: i32,
}

impl TermSpans {
    fn new(c: PostingsCursor) -> TermSpans {
        TermSpans { c, count: 0, start: -1 }
    }
}

impl Spans for TermSpans {
    fn doc(&self) -> u32 {
        self.c.doc()
    }
    fn approx_next(&mut self) -> u32 {
        self.start = -1;
        self.count = 0;
        self.c.next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.c.doc() != u32::MAX && t <= self.c.doc() {
            return self.c.doc();
        }
        self.start = -1;
        self.count = 0;
        self.c.advance(t)
    }
    fn cost(&self) -> u64 {
        self.c.cost()
    }
    fn matches(&mut self) -> bool {
        self.count = self.c.freq();
        self.start = -1;
        true
    }
    fn next_start(&mut self) -> i32 {
        if self.count == 0 {
            self.start = NO_MORE_POSITIONS;
        } else {
            self.count -= 1;
            self.start = self.c.next_position() as i32;
        }
        self.start
    }
    fn start(&self) -> i32 {
        self.start
    }
    fn end(&self) -> i32 {
        if self.start == -1 || self.start == NO_MORE_POSITIONS { self.start } else { self.start + 1 }
    }
    fn width(&self) -> i32 {
        0
    }
}

/// `span_gap`: matches everywhere; skips `width` positions (only meaningful in ordered near).
struct GapSpans {
    width: i32,
    doc: u32,
    start: i32,
}

impl Spans for GapSpans {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn approx_next(&mut self) -> u32 {
        self.doc = if self.doc == u32::MAX { 0 } else { self.doc.saturating_add(1).min(NO_MORE_DOCS) };
        self.start = -1;
        self.doc
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        self.doc = t;
        self.start = -1;
        t
    }
    fn cost(&self) -> u64 {
        0
    }
    fn matches(&mut self) -> bool {
        self.start = -1;
        true
    }
    fn next_start(&mut self) -> i32 {
        self.start = if self.start == -1 { 0 } else { self.start.saturating_add(1) };
        self.start
    }
    fn start(&self) -> i32 {
        self.start
    }
    fn end(&self) -> i32 {
        self.start.saturating_add(self.width)
    }
    fn width(&self) -> i32 {
        0
    }
}

/// Doc-level conjunction of sub-spans.
struct Conj {
    subs: Vec<Box<dyn Spans>>,
    doc: u32,
}

impl Conj {
    fn new(subs: Vec<Box<dyn Spans>>) -> Conj {
        Conj { subs, doc: u32::MAX }
    }

    fn lead(&self) -> usize {
        (0..self.subs.len()).filter(|&i| self.subs[i].cost() > 0).min_by_key(|&i| self.subs[i].cost()).unwrap_or(0)
    }

    fn align(&mut self, mut target: u32) -> u32 {
        let lead = self.lead();
        loop {
            let d = self.subs[lead].approx_advance(target);
            if d == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return d;
            }
            let mut ok = true;
            for i in 0..self.subs.len() {
                if i == lead {
                    continue;
                }
                let s = &mut self.subs[i];
                let sd = if s.doc() == u32::MAX || s.doc() < d { s.approx_advance(d) } else { s.doc() };
                if sd > d {
                    target = sd;
                    ok = false;
                    break;
                }
            }
            if ok {
                self.doc = d;
                return d;
            }
        }
    }

    fn next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc + 1 };
        self.align(t)
    }

    fn subs_match(&mut self) -> bool {
        self.subs.iter_mut().all(|s| s.matches())
    }

    fn cost(&self) -> u64 {
        self.subs.iter().map(|s| s.cost()).filter(|&c| c > 0).min().unwrap_or(0)
    }
}

fn advance_position(s: &mut Box<dyn Spans>, pos: i32) -> i32 {
    while s.start() < pos {
        s.next_start();
    }
    s.start()
}

struct NearOrdered {
    c: Conj,
    slop: i32,
    ms: i32,
    me: i32,
    mw: i32,
    /// The first match was found by `matches` and not returned yet.
    pending: bool,
}

impl NearOrdered {
    /// Lucene's `stretchToOrder`: Some(true) on an ordered match, Some(false) when one
    /// sub-spans ran out in the doc.
    fn stretch(&mut self) -> (bool, bool) {
        let subs = &mut self.c.subs;
        self.ms = subs[0].start();
        self.mw = 0;
        for i in 1..subs.len() {
            let prev_end = subs[i - 1].end();
            if advance_position(&mut subs[i], prev_end) == NO_MORE_POSITIONS {
                return (false, true);
            }
            self.mw += subs[i].start() - prev_end;
        }
        self.me = subs[subs.len() - 1].end();
        (true, false)
    }

    fn find(&mut self) -> bool {
        loop {
            if self.c.subs[0].next_start() == NO_MORE_POSITIONS {
                return false;
            }
            let (ok, exhausted) = self.stretch();
            if ok && self.mw <= self.slop {
                return true;
            }
            if exhausted {
                return false;
            }
        }
    }
}

impl Spans for NearOrdered {
    fn doc(&self) -> u32 {
        self.c.doc
    }
    fn approx_next(&mut self) -> u32 {
        self.ms = -1;
        self.c.next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.c.doc != u32::MAX && t <= self.c.doc {
            return self.c.doc;
        }
        self.ms = -1;
        self.c.align(t)
    }
    fn cost(&self) -> u64 {
        self.c.cost()
    }
    fn matches(&mut self) -> bool {
        if !self.c.subs_match() {
            return false;
        }
        let ok = self.find();
        // The first match is reported by the next `next_start`.
        self.pending = ok;
        ok
    }
    fn next_start(&mut self) -> i32 {
        if std::mem::take(&mut self.pending) {
            return self.ms;
        }
        if self.find() {
            self.ms
        } else {
            self.ms = NO_MORE_POSITIONS;
            self.me = NO_MORE_POSITIONS;
            NO_MORE_POSITIONS
        }
    }
    fn start(&self) -> i32 {
        if self.pending { -1 } else { self.ms }
    }
    fn end(&self) -> i32 {
        if self.pending { -1 } else { self.me }
    }
    fn width(&self) -> i32 {
        self.mw
    }
}

struct NearUnordered {
    c: Conj,
    slop: i32,
    /// Indices of subs ordered by (start, end): a binary heap.
    heap: Vec<usize>,
    total_len: i32,
    max_end: i32,
    at_first: bool,
    exhausted: bool,
}

impl NearUnordered {
    fn new(c: Conj, slop: i32) -> NearUnordered {
        NearUnordered { c, slop, heap: Vec::new(), total_len: 0, max_end: -1, at_first: false, exhausted: false }
    }

    fn less(&self, a: usize, b: usize) -> bool {
        let (x, y) = (&self.c.subs[a], &self.c.subs[b]);
        if x.start() == y.start() { x.end() < y.end() } else { x.start() < y.start() }
    }

    fn sift_down(&mut self) {
        let n = self.heap.len();
        let mut k = 0;
        loop {
            let l = 2 * k + 1;
            if l >= n {
                break;
            }
            let r = l + 1;
            let c = if r < n && self.less(self.heap[r], self.heap[l]) { r } else { l };
            if !self.less(self.heap[c], self.heap[k]) {
                break;
            }
            self.heap.swap(c, k);
            k = c;
        }
    }

    fn push(&mut self, i: usize) {
        self.heap.push(i);
        let mut k = self.heap.len() - 1;
        while k > 0 {
            let p = (k - 1) / 2;
            if !self.less(self.heap[k], self.heap[p]) {
                break;
            }
            self.heap.swap(k, p);
            k = p;
        }
    }

    fn start_document(&mut self) {
        self.heap.clear();
        self.total_len = 0;
        self.max_end = -1;
        for i in 0..self.c.subs.len() {
            self.c.subs[i].next_start();
            self.push(i);
            let s = &self.c.subs[i];
            self.max_end = self.max_end.max(s.end());
            self.total_len += s.end() - s.start();
        }
    }

    fn next_position(&mut self) -> bool {
        let top = self.heap[0];
        let len = self.c.subs[top].end() - self.c.subs[top].start();
        if self.c.subs[top].next_start() == NO_MORE_POSITIONS {
            return false;
        }
        self.total_len -= len;
        let s = &self.c.subs[top];
        self.total_len += s.end() - s.start();
        self.max_end = self.max_end.max(s.end());
        self.sift_down();
        true
    }

    fn at_match(&self) -> bool {
        let top = &self.c.subs[self.heap[0]];
        self.max_end - top.start() - self.total_len <= self.slop
    }
}

impl Spans for NearUnordered {
    fn doc(&self) -> u32 {
        self.c.doc
    }
    fn approx_next(&mut self) -> u32 {
        self.c.next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.c.doc != u32::MAX && t <= self.c.doc {
            return self.c.doc;
        }
        self.c.align(t)
    }
    fn cost(&self) -> u64 {
        self.c.cost()
    }
    fn matches(&mut self) -> bool {
        if !self.c.subs_match() {
            return false;
        }
        self.start_document();
        loop {
            if self.at_match() {
                self.at_first = true;
                self.exhausted = false;
                return true;
            }
            if !self.next_position() {
                return false;
            }
        }
    }
    fn next_start(&mut self) -> i32 {
        if self.at_first {
            self.at_first = false;
            return self.c.subs[self.heap[0]].start();
        }
        loop {
            if !self.next_position() {
                self.exhausted = true;
                return NO_MORE_POSITIONS;
            }
            if self.at_match() {
                return self.c.subs[self.heap[0]].start();
            }
        }
    }
    fn start(&self) -> i32 {
        if self.at_first {
            -1
        } else if self.exhausted {
            NO_MORE_POSITIONS
        } else {
            self.c.subs[self.heap[0]].start()
        }
    }
    fn end(&self) -> i32 {
        if self.at_first {
            -1
        } else if self.exhausted {
            NO_MORE_POSITIONS
        } else {
            self.max_end
        }
    }
    fn width(&self) -> i32 {
        self.max_end - self.c.subs[self.heap[0]].start()
    }
}

/// Union of spans: by doc, then by position.
struct OrSpans {
    subs: Vec<Box<dyn Spans>>,
    doc: u32,
    on_doc: Vec<usize>,
    /// Matching subs ordered by (start, end) once positions are read.
    pos: Vec<usize>,
    top: Option<usize>,
    started: bool,
}

impl OrSpans {
    fn new(subs: Vec<Box<dyn Spans>>) -> OrSpans {
        OrSpans { subs, doc: u32::MAX, on_doc: Vec::new(), pos: Vec::new(), top: None, started: false }
    }

    fn to(&mut self, t: u32) -> u32 {
        let mut m = NO_MORE_DOCS;
        for s in self.subs.iter_mut() {
            let d = if s.doc() == u32::MAX || s.doc() < t { s.approx_advance(t) } else { s.doc() };
            m = m.min(d);
        }
        self.doc = m;
        self.on_doc.clear();
        self.started = false;
        self.top = None;
        m
    }

    fn reorder(&mut self) {
        let subs = &self.subs;
        self.pos.sort_by(|&a, &b| {
            let (x, y) = (&subs[a], &subs[b]);
            x.start().cmp(&y.start()).then(x.end().cmp(&y.end()))
        });
    }
}

impl Spans for OrSpans {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn approx_next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc + 1 };
        self.to(t)
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.doc != u32::MAX && t <= self.doc {
            return self.doc;
        }
        self.to(t)
    }
    fn cost(&self) -> u64 {
        self.subs.iter().map(|s| s.cost()).sum()
    }
    fn matches(&mut self) -> bool {
        self.on_doc.clear();
        let d = self.doc;
        for i in 0..self.subs.len() {
            if self.subs[i].doc() == d && self.subs[i].matches() {
                self.on_doc.push(i);
            }
        }
        self.started = false;
        self.top = None;
        !self.on_doc.is_empty()
    }
    fn next_start(&mut self) -> i32 {
        if !self.started {
            self.started = true;
            self.pos.clear();
            for k in 0..self.on_doc.len() {
                let i = self.on_doc[k];
                if self.subs[i].next_start() != NO_MORE_POSITIONS {
                    self.pos.push(i);
                }
            }
        } else if let Some(t) = self.top
            && self.subs[t].next_start() == NO_MORE_POSITIONS
        {
            self.pos.retain(|&x| x != t);
        }
        if self.pos.is_empty() {
            self.top = None;
            return NO_MORE_POSITIONS;
        }
        self.reorder();
        self.top = Some(self.pos[0]);
        self.subs[self.pos[0]].start()
    }
    fn start(&self) -> i32 {
        match self.top {
            Some(t) => self.subs[t].start(),
            None => {
                if self.started { NO_MORE_POSITIONS } else { -1 }
            }
        }
    }
    fn end(&self) -> i32 {
        match self.top {
            Some(t) => self.subs[t].end(),
            None => {
                if self.started { NO_MORE_POSITIONS } else { -1 }
            }
        }
    }
    fn width(&self) -> i32 {
        self.top.map_or(0, |t| self.subs[t].width())
    }
}

enum Filter {
    Not { exc: Box<dyn Spans>, pre: i32, post: i32, last: u32, last_match: bool },
    First { end: i32 },
}

enum Accept {
    Yes,
    No,
    NoMoreInDoc,
}

/// Spans of `inner` filtered position by position (`span_not`, `span_first`).
struct FilterSpans {
    inner: Box<dyn Spans>,
    f: Filter,
    at_first: bool,
    start: i32,
}

impl FilterSpans {
    fn new(inner: Box<dyn Spans>, f: Filter) -> FilterSpans {
        FilterSpans { inner, f, at_first: false, start: -1 }
    }

    fn accept(&mut self) -> Accept {
        let c = &self.inner;
        match &mut self.f {
            Filter::First { end } => {
                if c.start() >= *end {
                    Accept::NoMoreInDoc
                } else if c.end() <= *end {
                    Accept::Yes
                } else {
                    Accept::No
                }
            }
            Filter::Not { exc, pre, post, last, last_match } => {
                let doc = c.doc();
                if exc.doc() == u32::MAX || doc > exc.doc() {
                    if exc.approx_advance(doc) == doc {
                        *last = doc;
                        *last_match = exc.matches();
                    }
                } else if doc == exc.doc() && doc != *last {
                    *last = doc;
                    *last_match = exc.matches();
                }
                if doc != exc.doc() || !*last_match {
                    return Accept::Yes;
                }
                if exc.start() == -1 {
                    exc.next_start();
                }
                while exc.end() <= c.start() - *pre {
                    if exc.next_start() == NO_MORE_POSITIONS {
                        return Accept::Yes;
                    }
                }
                if exc.start() - *post >= c.end() { Accept::Yes } else { Accept::No }
            }
        }
    }

    fn find(&mut self) -> i32 {
        loop {
            let s = self.inner.next_start();
            if s == NO_MORE_POSITIONS {
                return NO_MORE_POSITIONS;
            }
            match self.accept() {
                Accept::Yes => return s,
                Accept::No => {}
                Accept::NoMoreInDoc => return NO_MORE_POSITIONS,
            }
        }
    }
}

impl Spans for FilterSpans {
    fn doc(&self) -> u32 {
        self.inner.doc()
    }
    fn approx_next(&mut self) -> u32 {
        self.inner.approx_next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        self.inner.approx_advance(t)
    }
    fn cost(&self) -> u64 {
        self.inner.cost()
    }
    fn matches(&mut self) -> bool {
        if !self.inner.matches() {
            return false;
        }
        self.start = self.find();
        if self.start == NO_MORE_POSITIONS {
            return false;
        }
        self.at_first = true;
        true
    }
    fn next_start(&mut self) -> i32 {
        if self.at_first {
            self.at_first = false;
            return self.start;
        }
        self.start = self.find();
        self.start
    }
    fn start(&self) -> i32 {
        if self.at_first { -1 } else { self.start }
    }
    fn end(&self) -> i32 {
        if self.at_first {
            -1
        } else if self.start != NO_MORE_POSITIONS {
            self.inner.end()
        } else {
            NO_MORE_POSITIONS
        }
    }
    fn width(&self) -> i32 {
        self.inner.width()
    }
}

/// `span_containing` (big spans containing a little one) / `span_within` (little spans inside
/// a big one).
struct ContainSpans {
    c: Conj,
    containing: bool,
    at_first: bool,
    exhausted: bool,
}

impl ContainSpans {
    fn find(&mut self) -> bool {
        let (b, l) = self.c.subs.split_at_mut(1);
        let (big, little) = (&mut b[0], &mut l[0]);
        if self.containing {
            while big.next_start() != NO_MORE_POSITIONS {
                while little.start() < big.start() {
                    if little.next_start() == NO_MORE_POSITIONS {
                        self.exhausted = true;
                        return false;
                    }
                }
                if big.end() >= little.end() {
                    return true;
                }
            }
        } else {
            while little.next_start() != NO_MORE_POSITIONS {
                while big.end() < little.end() {
                    if big.next_start() == NO_MORE_POSITIONS {
                        self.exhausted = true;
                        return false;
                    }
                }
                if big.start() <= little.start() {
                    return true;
                }
            }
        }
        self.exhausted = true;
        false
    }

    fn source(&self) -> &dyn Spans {
        if self.containing { self.c.subs[0].as_ref() } else { self.c.subs[1].as_ref() }
    }
}

impl Spans for ContainSpans {
    fn doc(&self) -> u32 {
        self.c.doc
    }
    fn approx_next(&mut self) -> u32 {
        self.c.next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.c.doc != u32::MAX && t <= self.c.doc {
            return self.c.doc;
        }
        self.c.align(t)
    }
    fn cost(&self) -> u64 {
        self.c.cost()
    }
    fn matches(&mut self) -> bool {
        if !self.c.subs_match() {
            return false;
        }
        self.exhausted = false;
        let ok = self.find();
        self.at_first = ok;
        ok
    }
    fn next_start(&mut self) -> i32 {
        if self.at_first {
            self.at_first = false;
            return self.source().start();
        }
        if self.find() { self.source().start() } else { NO_MORE_POSITIONS }
    }
    fn start(&self) -> i32 {
        if self.at_first {
            -1
        } else if self.exhausted {
            NO_MORE_POSITIONS
        } else {
            self.source().start()
        }
    }
    fn end(&self) -> i32 {
        if self.at_first {
            -1
        } else if self.exhausted {
            NO_MORE_POSITIONS
        } else {
            self.source().end()
        }
    }
    fn width(&self) -> i32 {
        self.source().width()
    }
}

/// A span query.
#[derive(Clone, Debug)]
pub struct SpanQuery {
    pub node: SpanNode,
}

impl SpanQuery {
    pub fn new(node: SpanNode) -> Result<SpanQuery> {
        node.check_fields()?;
        Ok(SpanQuery { node })
    }
}

struct SpanWeight {
    node: SpanNode,
    field: u32,
    states: BTreeMap<(u32, Vec<u8>), TermStates>,
    sim: Option<Arc<SimScorer>>,
    desc: String,
    scores: bool,
}

struct SpanScorer {
    spans: Box<dyn Spans>,
    sim: Option<Arc<SimScorer>>,
    norms: Norms,
    freq: f32,
    scored_doc: u32,
    /// The doc `matches` last answered for, and the answer (asking again would move the
    /// positions).
    checked: (u32, bool),
}

impl SpanScorer {
    fn freq_now(&mut self) -> f32 {
        let d = self.spans.doc();
        if self.scored_doc != d {
            self.scored_doc = d;
            let mut f = 0f32;
            let mut s = self.spans.next_start();
            while s != NO_MORE_POSITIONS {
                if self.sim.is_none() {
                    f = 1.0;
                    break;
                }
                f = (f as f64 + 1.0 / (1.0 + self.spans.width() as f64)) as f32;
                s = self.spans.next_start();
            }
            self.freq = f;
        }
        self.freq
    }
}

impl Scorable for SpanScorer {
    fn score(&mut self) -> f32 {
        let f = self.freq_now();
        match &self.sim {
            Some(sim) => sim.score(f, self.norms.get(self.spans.doc())),
            None => 0.0,
        }
    }
}

impl Scorer for SpanScorer {
    fn doc(&self) -> u32 {
        self.spans.doc()
    }
    fn next(&mut self) -> u32 {
        loop {
            let d = self.spans.approx_next();
            if d == NO_MORE_DOCS || self.spans.matches() {
                return d;
            }
        }
    }
    fn advance(&mut self, t: u32) -> u32 {
        let d = self.spans.approx_advance(t);
        if d == NO_MORE_DOCS || self.spans.matches() {
            return d;
        }
        self.next()
    }
    fn cost(&self) -> u64 {
        self.spans.cost()
    }
    fn has_two_phase(&self) -> bool {
        true
    }
    fn approx_next(&mut self) -> u32 {
        self.spans.approx_next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        self.spans.approx_advance(t)
    }
    fn matches(&mut self) -> bool {
        let d = self.spans.doc();
        if self.checked.0 != d {
            self.checked = (d, self.spans.matches());
        }
        self.checked.1
    }
    fn match_cost(&self) -> f32 {
        8.0
    }
    fn freq(&mut self) -> f32 {
        self.freq_now()
    }
}

impl SpanWeight {
    fn scorer_impl(&self, seg: &SegmentContext) -> Result<Option<SpanScorer>> {
        let Some(spans) = build(&self.node, seg, &self.states)? else { return Ok(None) };
        let norms = if self.scores { Norms::of(seg, self.field) } else { Norms::None };
        Ok(Some(SpanScorer { spans, sim: self.sim.clone(), norms, freq: 0.0, scored_doc: u32::MAX, checked: (u32::MAX, false) }))
    }
}

impl Weight for SpanWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.scorer_impl(seg)?.map(|s| Box::new(s) as Box<dyn Scorer>))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        if let Some(mut s) = self.scorer_impl(seg)?
            && s.advance(doc) == doc
        {
            let desc = format!("weight({} in {doc}) [{}], result of:", self.desc, crate::query::term::SIMILARITY_NAME);
            let Some(sim) = &self.sim else {
                // Created without scores (a filter, or `boost_mode: replace`): it matches.
                return Ok(Explanation::matched(s.score(), desc, vec![]));
            };
            let f = s.freq_now();
            let fe = Explanation::leaf(f, format!("phraseFreq={}", float_to_string(f)));
            let norm = Norms::of(seg, self.field).get(doc);
            let se = sim.explain(fe, norm);
            return Ok(Explanation::matched(se.value(), desc, vec![se]));
        }
        Ok(Explanation::no_match("no matching term", vec![]))
    }
}

impl Query for SpanQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let mut terms = Vec::new();
        self.node.terms(&mut terms);
        let mut states = BTreeMap::new();
        let mut stats: Vec<TermStats> = Vec::new();
        for (f, t) in terms {
            if states.contains_key(&(f, t.clone())) {
                continue;
            }
            let ts = TermStates::build(s, f, &t);
            if ts.doc_freq > 0 {
                stats.push(ts.stats());
            }
            states.insert((f, t), ts);
        }
        // Terms that only filter (span_not excludes) need states but don't score.
        let mut all = Vec::new();
        self.node.all_terms(&mut all);
        for (f, t) in all {
            if !states.contains_key(&(f, t.clone())) {
                let ts = TermStates::build(s, f, &t);
                states.insert((f, t), ts);
            }
        }
        let field = self.node.field();
        let sim = if mode.needs_scores() && !stats.is_empty() {
            scoring_stats(s, field, mode).map(|c| Arc::new(s.schema().similarity(field).scorer(boost, &c, &stats)))
        } else {
            None
        };
        Ok(Box::new(SpanWeight { node: self.node.clone(), field, states, sim, desc: self.lucene_string(s.schema()), scores: mode.needs_scores() }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        if self.node.has_multi() {
            let n = self.node.expand(s)?;
            if let SpanNode::Or(c) = &n
                && c.is_empty()
            {
                return Ok(Some(Arc::new(MatchNoneQuery::new(""))));
            }
            return Ok(Some(Arc::new(SpanQuery { node: n })));
        }
        Ok(None)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        self.node.lucene(schema)
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        let mut terms = Vec::new();
        self.node.terms(&mut terms);
        for (f, t) in terms {
            v.term(f, &t);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
