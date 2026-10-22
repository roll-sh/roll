//! Phrase queries: `PhraseQuery` (one term per position) and `MultiPhraseQuery` (any of several
//! terms per position), exact or with slop.
//!
//! Matching follows Lucene's `ExactPhraseMatcher` and `SloppyPhraseMatcher` step for step —
//! including repeated terms, which the sloppy matcher keeps from sharing a position — because
//! the phrase frequency (exact: number of matches; sloppy: Σ 1/(1 + match length)) feeds the
//! score. Iteration is two-phase: the approximation is the conjunction of the terms' doc
//! streams and positions are only read for docs every term contains. Under top-scores,
//! candidate docs whose best possible phrase frequency can't compete skip position decoding.

use std::any::Any;
use std::sync::Arc;

use search_codec::{Impact, NO_MORE_DOCS, PostingsCursor, PostingsFlags};

use crate::error::{Error, Result};
use crate::query::boolean::{BooleanQuery, Occur};
use crate::query::term::{AdvanceShallow, ImpactSource, MaxScoreCache, Norms, competitive_target, SIMILARITY_NAME, TermQuery, TermStates, scoring_stats, term_text};
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::similarity::SimScorer;
use crate::util::java::float_to_string;

/// Docs containing the terms at the given relative positions (within `slop` moves).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhraseQuery {
    pub field: u32,
    pub terms: Vec<Vec<u8>>,
    pub positions: Vec<u32>,
    pub slop: u32,
}

impl PhraseQuery {
    /// Terms at consecutive positions.
    pub fn new(field: u32, terms: Vec<Vec<u8>>, slop: u32) -> PhraseQuery {
        let positions = (0..terms.len() as u32).collect();
        PhraseQuery { field, terms, positions, slop }
    }

    pub fn with_positions(field: u32, terms: Vec<Vec<u8>>, positions: Vec<u32>, slop: u32) -> Result<PhraseQuery> {
        if terms.len() != positions.len() {
            return Err(Error::illegal("Must have as many terms as positions"));
        }
        if positions.windows(2).any(|w| w[1] < w[0]) {
            return Err(Error::illegal("Positions must be added in order"));
        }
        Ok(PhraseQuery { field, terms, positions, slop })
    }
}

/// Docs matching a phrase where each position accepts any of several terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiPhraseQuery {
    pub field: u32,
    pub terms: Vec<Vec<Vec<u8>>>,
    pub positions: Vec<u32>,
    pub slop: u32,
}

impl MultiPhraseQuery {
    pub fn new(field: u32, terms: Vec<Vec<Vec<u8>>>, positions: Vec<u32>, slop: u32) -> Result<MultiPhraseQuery> {
        if terms.len() != positions.len() {
            return Err(Error::illegal("Must have as many terms as positions"));
        }
        Ok(MultiPhraseQuery { field, terms, positions, slop })
    }
}

/// Positions of one phrase slot in the current doc: one term's postings or a union.
// The single-term variant is the hot one; boxing its cursor would cost an indirection per
// position read.
#[allow(clippy::large_enum_variant)]
enum Slot {
    One(PostingsCursor),
    Union { subs: Vec<PostingsCursor>, doc: u32, pos: Vec<(u32, u32, u32)>, idx: usize, loaded: bool },
}

// The single-term arms are the hot path and must inline into the matcher; unions take
// out-of-line calls.
impl Slot {
    #[inline(always)]
    fn doc(&self) -> u32 {
        match self {
            Slot::One(c) => c.doc(),
            Slot::Union { doc, .. } => *doc,
        }
    }

    fn cost(&self) -> u64 {
        match self {
            Slot::One(c) => c.cost(),
            Slot::Union { subs, .. } => subs.iter().map(|s| s.cost()).sum(),
        }
    }

    #[inline(always)]
    fn advance(&mut self, t: u32) -> u32 {
        match self {
            Slot::One(c) => c.advance(t),
            _ => self.union_advance(t),
        }
    }

    /// The doc after the current one.
    #[inline(always)]
    fn next(&mut self) -> u32 {
        match self {
            Slot::One(c) => c.next(),
            Slot::Union { doc, .. } => {
                let t = doc.wrapping_add(1);
                self.union_advance(t)
            }
        }
    }

    #[inline(never)]
    fn union_advance(&mut self, t: u32) -> u32 {
        let Slot::Union { subs, doc, loaded, idx, .. } = self else { unreachable!() };
        if *doc != u32::MAX && t <= *doc {
            // Already there: keep the loaded positions.
            return *doc;
        }
        let mut m = NO_MORE_DOCS;
        for s in subs.iter_mut() {
            let d = if s.doc() == u32::MAX || s.doc() < t { s.advance(t) } else { s.doc() };
            m = m.min(d);
        }
        *doc = m;
        *loaded = false;
        *idx = 0;
        m
    }

    #[inline(always)]
    fn freq(&mut self) -> u32 {
        match self {
            Slot::One(c) => c.freq(),
            _ => self.union_freq(),
        }
    }

    #[inline(never)]
    fn union_freq(&mut self) -> u32 {
        let Slot::Union { subs, doc, pos, loaded, .. } = self else { unreachable!() };
        if subs.is_empty() && *loaded {
            // In-memory positions (`memory_phrase_freq`).
            return pos.len() as u32;
        }
        let d = *doc;
        subs.iter_mut().filter(|s| s.doc() == d).map(|s| s.freq()).sum()
    }

    #[inline(always)]
    fn next_position(&mut self) -> u32 {
        match self {
            Slot::One(c) => c.next_position(),
            _ => self.union_next_position(),
        }
    }

    #[inline(never)]
    fn union_next_position(&mut self) -> u32 {
        let Slot::Union { subs, doc, pos, idx, loaded } = self else { unreachable!() };
        if !*loaded {
            pos.clear();
            let d = *doc;
            for s in subs.iter_mut() {
                if s.doc() == d {
                    for _ in 0..s.freq() {
                        let p = s.next_position();
                        pos.push((p, s.start_offset(), s.end_offset()));
                    }
                }
            }
            pos.sort_unstable();
            *loaded = true;
            *idx = 0;
        }
        let p = pos.get(*idx).map_or(0, |x| x.0);
        *idx += 1;
        p
    }
}

/// The phrase frequency of one document given as positions: per phrase slot its terms, its
/// offset in the phrase and the sorted positions of those terms in the doc. Runs the same
/// exact / sloppy matcher as the index (`SourceConfirmedQuery`'s fast path).
pub(crate) fn memory_phrase_freq(slots: Vec<(Vec<Vec<u8>>, i64, Vec<u32>)>, slop: u32) -> f32 {
    let Some(mut m) = memory_matcher(slots, slop) else { return 0.0 };
    if !m.next_match() {
        return 0.0;
    }
    let mut f = m.sloppy_weight();
    while m.next_match() {
        f += m.sloppy_weight();
    }
    f
}

/// Every match of a phrase in one document given as positions (as [`memory_phrase_freq`]):
/// the first and last term position of each, in the order the matcher finds them (Lucene's
/// `PhraseMatcher.startPosition` / `endPosition`, what weight-matches highlighting shows).
/// Sloppy matches obey Lucene's rule (the spread of all slots at most the slop, a position
/// used by one slot only), unlike checking each slot on its own.
pub(crate) fn memory_phrase_matches(slots: Vec<(Vec<Vec<u8>>, i64, Vec<u32>)>, slop: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let Some(mut m) = memory_matcher(slots, slop) else { return out };
    while m.next_match() {
        out.push((m.start_pos.max(0) as u32, m.end_pos.max(m.start_pos).max(0) as u32));
    }
    out
}

/// A matcher positioned on the one in-memory doc, or `None` when a slot has no position.
fn memory_matcher(slots: Vec<(Vec<Vec<u8>>, i64, Vec<u32>)>, slop: u32) -> Option<Matcher> {
    let mut pps = Vec::with_capacity(slots.len());
    for (k, (terms, offset, positions)) in slots.into_iter().enumerate() {
        if positions.is_empty() {
            return None;
        }
        let pos = positions.into_iter().map(|p| (p, 0, 0)).collect();
        let slot = Slot::Union { subs: Vec::new(), doc: 0, pos, idx: 0, loaded: true };
        pps.push(PP { slot, offset, ord: k, terms, freq: 0, count: 0, upto: 0, position: -1, rpt_group: -1, rpt_ind: 0 });
    }
    if pps.is_empty() {
        return None;
    }
    let mut m = Matcher::new(pps, slop);
    if m.approx_advance(0) != 0 {
        return None;
    }
    m.reset_positions();
    Some(m)
}

/// A slot with its phrase offset and per-doc iteration state.
struct PP {
    slot: Slot,
    offset: i64,
    ord: usize,
    terms: Vec<Vec<u8>>,
    freq: u32,
    count: i64,
    upto: u32,
    position: i64,
    rpt_group: i32,
    rpt_ind: usize,
}

impl PP {
    fn first_position(&mut self) {
        self.count = self.freq as i64;
        self.next_position();
    }

    fn next_position(&mut self) -> bool {
        if self.count > 0 {
            self.count -= 1;
            self.position = self.slot.next_position() as i64 - self.offset;
            true
        } else {
            self.count -= 1;
            false
        }
    }
}

/// Exact or sloppy phrase matching over the slots of one doc.
pub(crate) struct Matcher {
    pps: Vec<PP>,
    slop: u32,
    lead: usize,
    /// Slot indices by ascending cost (`order[0] == lead`).
    order: Vec<usize>,
    // Sloppy state.
    pq: Vec<usize>,
    end: i64,
    match_length: i64,
    positioned: bool,
    checked_rpts: bool,
    has_rpts: bool,
    multi_term_rpts: bool,
    rpt_groups: Vec<Vec<usize>>,
    doc: u32,
    freqs_loaded: bool,
    start_pos: i64,
    end_pos: i64,
}

impl Matcher {
    fn new(mut pps: Vec<PP>, slop: u32) -> Matcher {
        if slop == 0 {
            // Lucene sorts by phrase position (then term) and leads with the first.
            pps.sort_by(|a, b| a.offset.cmp(&b.offset).then(a.terms.len().cmp(&b.terms.len())).then(a.terms.cmp(&b.terms)));
        }
        for (i, p) in pps.iter_mut().enumerate() {
            p.ord = i;
        }
        // Lucene's conjunction leads with the cheapest iterator and checks the others in
        // ascending cost.
        let mut order: Vec<usize> = (0..pps.len()).collect();
        order.sort_by_key(|&i| pps[i].slot.cost());
        let lead = order.first().copied().unwrap_or(0);
        Matcher {
            pps,
            slop,
            lead,
            order,
            pq: Vec::new(),
            end: 0,
            match_length: 0,
            positioned: false,
            checked_rpts: false,
            has_rpts: false,
            multi_term_rpts: false,
            rpt_groups: Vec::new(),
            doc: u32::MAX,
            freqs_loaded: false,
            start_pos: 0,
            end_pos: 0,
        }
    }

    fn cost(&self) -> u64 {
        self.pps[self.lead].slot.cost()
    }

    /// Conjunction of the slots' docs.
    #[inline]
    fn approx_advance(&mut self, mut target: u32) -> u32 {
        if let Some(d) = self.approx_advance2(target) {
            return d;
        }
        if let Some(d) = self.approx_advance3(target) {
            return d;
        }
        loop {
            let lead = self.lead;
            let slot = &mut self.pps[lead].slot;
            let cur = slot.doc();
            // Sequential iteration steps the lead's buffer instead of searching it.
            let doc = if cur != u32::MAX && cur.wrapping_add(1) == target { slot.next() } else { slot.advance(target) };
            if doc == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return NO_MORE_DOCS;
            }
            let mut ok = true;
            for k in 1..self.order.len() {
                let s = &mut self.pps[self.order[k]].slot;
                let d = if s.doc() == u32::MAX || s.doc() < doc { s.advance(doc) } else { s.doc() };
                if d > doc {
                    target = d;
                    ok = false;
                    break;
                }
            }
            if ok {
                self.doc = doc;
                self.freqs_loaded = false;
                return doc;
            }
            if target == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return NO_MORE_DOCS;
            }
            target = self.dense_target(target);
        }
    }

    /// [`approx_advance`](Self::approx_advance) for two single-term slots (most phrases): the
    /// leapfrog on the two cursors directly. `None` for other shapes.
    #[inline]
    fn approx_advance2(&mut self, mut target: u32) -> Option<u32> {
        let lead_first = self.lead == 0;
        let [p0, p1] = &mut self.pps[..] else { return None };
        let (Slot::One(c0), Slot::One(c1)) = (&mut p0.slot, &mut p1.slot) else { return None };
        let (lead, other) = if lead_first { (c0, c1) } else { (c1, c0) };
        loop {
            let cur = lead.doc();
            let doc = if cur != u32::MAX && cur.wrapping_add(1) == target { lead.next() } else { lead.advance(target) };
            if doc == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Some(NO_MORE_DOCS);
            }
            let od = other.doc();
            let d = if od == u32::MAX || od < doc { other.advance(doc) } else { od };
            if d == doc {
                self.doc = doc;
                self.freqs_loaded = false;
                return Some(doc);
            }
            if d == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Some(NO_MORE_DOCS);
            }
            target = d;
            // Both in dense blocks: jump to their next common doc.
            if let (Some(a), Some(b)) = (lead.dense_block(), other.dense_block())
                && a.2 >= target
                && b.2 >= target
            {
                target = match crate::util::dense::first_common(&[a, b], target) {
                    Ok(d) => d,
                    Err(end) => end.saturating_add(1).max(target),
                };
            }
        }
    }

    /// [`approx_advance`](Self::approx_advance) for three single-term slots: the leapfrog on
    /// the cursors directly, others checked in ascending cost. `None` for other shapes.
    #[inline]
    fn approx_advance3(&mut self, mut target: u32) -> Option<u32> {
        if self.pps.len() != 3 {
            return None;
        }
        let (o1, o2) = (self.order[1], self.order[2]);
        let lead_i = self.lead;
        let [p0, p1, p2] = &mut self.pps[..] else { return None };
        let (Slot::One(c0), Slot::One(c1), Slot::One(c2)) = (&mut p0.slot, &mut p1.slot, &mut p2.slot) else { return None };
        let mut cs = [Some(c0), Some(c1), Some(c2)];
        let lead = cs[lead_i].take().unwrap();
        let a = cs[o1].take().unwrap();
        let b = cs[o2].take().unwrap();
        loop {
            let cur = lead.doc();
            let doc = if cur != u32::MAX && cur.wrapping_add(1) == target { lead.next() } else { lead.advance(target) };
            if doc == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Some(NO_MORE_DOCS);
            }
            let ad = a.doc();
            let d = if ad == u32::MAX || ad < doc { a.advance(doc) } else { ad };
            if d == doc {
                let bd = b.doc();
                let e = if bd == u32::MAX || bd < doc { b.advance(doc) } else { bd };
                if e == doc {
                    self.doc = doc;
                    self.freqs_loaded = false;
                    return Some(doc);
                }
                target = e;
            } else {
                target = d;
            }
            if target == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Some(NO_MORE_DOCS);
            }
            if let (Some(x), Some(y), Some(z)) = (lead.dense_block(), a.dense_block(), b.dense_block())
                && x.2 >= target
                && y.2 >= target
                && z.2 >= target
            {
                target = match crate::util::dense::first_common(&[x, y, z], target) {
                    Ok(d) => d,
                    Err(end) => end.saturating_add(1).max(target),
                };
            }
        }
    }

    /// When every slot sits in a dense block reaching `target`, the next doc `>= target` they
    /// all contain within those blocks, or the doc after the first block end. Every slot's
    /// cursor got to its block by `advance` with targets up to `target`, so the block's bits at
    /// or above `target` are exactly the term's docs there.
    fn dense_target(&self, target: u32) -> u32 {
        let mut blocks: [crate::util::dense::DenseBlock<'_>; 4] = [(0, &[], 0); 4];
        if self.pps.len() > blocks.len() {
            return target;
        }
        for (b, p) in blocks.iter_mut().zip(&self.pps) {
            let Slot::One(c) = &p.slot else { return target };
            match c.dense_block() {
                Some(d) if d.2 >= target => *b = d,
                _ => return target,
            }
        }
        match crate::util::dense::first_common(&blocks[..self.pps.len()], target) {
            Ok(d) => d,
            Err(end) => end.saturating_add(1).max(target),
        }
    }

    /// Upper bound of the phrase frequency (sloppy: sum of freqs; exact: one match per lead
    /// position, so the lead's freq, or the min freq when the lead's positions are distinct —
    /// a union lead can repeat a position, and then two matches share the other slots'
    /// positions).
    fn max_freq(&mut self) -> f32 {
        for p in self.pps.iter_mut() {
            p.freq = p.slot.freq();
        }
        self.freqs_loaded = true;
        if self.slop == 0 {
            if matches!(self.pps[0].slot, Slot::Union { .. }) {
                return self.pps[0].freq as f32;
            }
            self.pps.iter().map(|p| p.freq).min().unwrap_or(0) as f32
        } else {
            self.pps.iter().map(|p| p.freq as f32).sum()
        }
    }

    #[inline]
    fn reset_positions(&mut self) {
        if !self.freqs_loaded {
            for p in self.pps.iter_mut() {
                p.freq = p.slot.freq();
            }
        }
        self.freqs_loaded = false;
        if self.slop == 0 {
            for p in self.pps.iter_mut() {
                p.position = -1;
                p.upto = 0;
            }
        } else {
            self.reset_sloppy();
        }
    }

    #[inline(never)]
    fn reset_sloppy(&mut self) {
        self.positioned = self.init_phrase_positions();
        self.match_length = i64::MAX;
    }

    fn sloppy_weight(&self) -> f32 {
        if self.slop == 0 { 1.0 } else { 1f32 / (1f32 + self.match_length as f32) }
    }

    #[inline]
    fn next_match(&mut self) -> bool {
        if self.slop == 0 { self.exact_next() } else { self.sloppy_next() }
    }

    // ------------------------------------------------------------ exact

    #[inline]
    fn adv_pos(p: &mut PP, target: i64) -> bool {
        while p.position < target {
            if p.upto == p.freq {
                return false;
            }
            p.position = p.slot.next_position() as i64;
            p.upto += 1;
        }
        true
    }

    /// [`exact_next`](Self::exact_next) for two single-term slots: the same walk over the two
    /// cursors' positions with the state in locals.
    #[inline]
    fn exact_next2(&mut self) -> Option<bool> {
        let [a, b] = &mut self.pps[..] else { return None };
        let (Slot::One(ca), Slot::One(cb)) = (&mut a.slot, &mut b.slot) else { return None };
        if a.upto >= a.freq {
            return Some(false);
        }
        // b must sit at a's position + delta.
        let delta = b.offset - a.offset;
        let (fa, fb) = (a.freq, b.freq);
        let (mut pa, mut ua) = (ca.next_position() as i64, a.upto + 1);
        let (mut pb, mut ub) = (b.position, b.upto);
        let found = 'head: loop {
            let want = pa + delta;
            while pb < want {
                if ub == fb {
                    break 'head false;
                }
                pb = cb.next_position() as i64;
                ub += 1;
            }
            if pb == want {
                break true;
            }
            let t = pb - delta;
            while pa < t {
                if ua == fa {
                    break 'head false;
                }
                pa = ca.next_position() as i64;
                ua += 1;
            }
        };
        (a.position, a.upto, b.position, b.upto) = (pa, ua, pb, ub);
        if found {
            self.start_pos = pa;
            self.end_pos = pb;
        }
        Some(found)
    }

    #[inline]
    fn exact_next(&mut self) -> bool {
        if let Some(r) = self.exact_next2() {
            return r;
        }
        {
            let lead = &mut self.pps[0];
            if lead.upto < lead.freq {
                lead.position = lead.slot.next_position() as i64;
                lead.upto += 1;
            } else {
                return false;
            }
        }
        'head: loop {
            let phrase_pos = self.pps[0].position - self.pps[0].offset;
            for j in 1..self.pps.len() {
                let expected = phrase_pos + self.pps[j].offset;
                if !Self::adv_pos(&mut self.pps[j], expected) {
                    return false;
                }
                if self.pps[j].position != expected {
                    let t = self.pps[j].position - self.pps[j].offset + self.pps[0].offset;
                    if Self::adv_pos(&mut self.pps[0], t) {
                        continue 'head;
                    } else {
                        return false;
                    }
                }
            }
            self.start_pos = self.pps[0].position;
            self.end_pos = self.pps[self.pps.len() - 1].position;
            return true;
        }
    }

    // ------------------------------------------------------------ sloppy

    fn less(&self, a: usize, b: usize) -> bool {
        let (x, y) = (&self.pps[a], &self.pps[b]);
        if x.position == y.position {
            if x.offset == y.offset { x.ord < y.ord } else { x.offset < y.offset }
        } else {
            x.position < y.position
        }
    }

    fn pq_add(&mut self, i: usize) {
        self.pq.push(i);
        let mut k = self.pq.len() - 1;
        while k > 0 {
            let p = (k - 1) / 2;
            if !self.less(self.pq[k], self.pq[p]) {
                break;
            }
            self.pq.swap(k, p);
            k = p;
        }
    }

    fn pq_pop(&mut self) -> usize {
        let n = self.pq.len() - 1;
        self.pq.swap(0, n);
        let r = self.pq.pop().unwrap();
        let len = self.pq.len();
        let mut k = 0;
        loop {
            let l = 2 * k + 1;
            if l >= len {
                break;
            }
            let rr = l + 1;
            let c = if rr < len && self.less(self.pq[rr], self.pq[l]) { rr } else { l };
            if !self.less(self.pq[c], self.pq[k]) {
                break;
            }
            self.pq.swap(c, k);
            k = c;
        }
        r
    }

    fn advance_pp(&mut self, i: usize) -> bool {
        if !self.pps[i].next_position() {
            return false;
        }
        if self.pps[i].position > self.end {
            self.end = self.pps[i].position;
        }
        true
    }

    fn tp_pos(&self, i: usize) -> i64 {
        self.pps[i].position + self.pps[i].offset
    }

    fn collide(&self, i: usize) -> Option<usize> {
        let tp = self.tp_pos(i);
        let g = &self.rpt_groups[self.pps[i].rpt_group as usize];
        g.iter().copied().find(|&j| j != i && self.tp_pos(j) == tp).map(|j| self.pps[j].rpt_ind)
    }

    fn lesser(&self, a: usize, b: usize) -> usize {
        let (x, y) = (&self.pps[a], &self.pps[b]);
        if x.position < y.position || (x.position == y.position && x.offset < y.offset) { a } else { b }
    }

    fn advance_rpts(&mut self, mut i: usize) -> bool {
        if self.pps[i].rpt_group < 0 {
            return true;
        }
        let g = self.pps[i].rpt_group as usize;
        let k0 = self.pps[i].rpt_ind;
        let mut marked = vec![false; self.rpt_groups[g].len()];
        while let Some(k) = self.collide(i) {
            let other = self.rpt_groups[g][k];
            i = self.lesser(i, other);
            if !self.advance_pp(i) {
                return false;
            }
            if k != k0 {
                marked[k] = true;
            }
        }
        let mut stack = Vec::new();
        while marked.iter().any(|&b| b) {
            let p = self.pq_pop();
            stack.push(p);
            if self.pps[p].rpt_group >= 0 && self.pps[p].rpt_ind < marked.len() && marked[self.pps[p].rpt_ind] {
                marked[self.pps[p].rpt_ind] = false;
            }
        }
        for &p in stack.iter().rev() {
            self.pq_add(p);
        }
        true
    }

    #[inline(never)]
    fn sloppy_next(&mut self) -> bool {
        if !self.positioned {
            return false;
        }
        let mut pp = self.pq_pop();
        self.match_length = self.end - self.pps[pp].position;
        self.capture(pp);
        let mut next = self.pps[self.pq[0]].position;
        while self.advance_pp(pp) {
            if self.has_rpts && !self.advance_rpts(pp) {
                break;
            }
            if self.pps[pp].position > next {
                self.pq_add(pp);
                if self.match_length <= self.slop as i64 {
                    return true;
                }
                pp = self.pq_pop();
                next = self.pps[self.pq[0]].position;
                self.match_length = self.end - self.pps[pp].position;
            } else {
                let ml2 = self.end - self.pps[pp].position;
                if ml2 < self.match_length {
                    self.match_length = ml2;
                }
            }
            self.capture(pp);
        }
        self.positioned = false;
        self.match_length <= self.slop as i64
    }

    fn capture(&mut self, pp: usize) {
        let lead = self.pps[pp].position + self.pps[pp].offset;
        self.start_pos = lead;
        let mut s = lead;
        let mut e = lead;
        for (i, p) in self.pps.iter().enumerate() {
            let tp = p.position + p.offset;
            s = s.min(tp);
            if i != pp {
                e = e.max(tp);
            }
        }
        self.start_pos = s;
        self.end_pos = e;
    }

    fn init_phrase_positions(&mut self) -> bool {
        self.end = i64::MIN;
        if !self.checked_rpts {
            return self.init_first_time();
        }
        if !self.has_rpts {
            self.pq.clear();
            for i in 0..self.pps.len() {
                self.pps[i].first_position();
                if self.pps[i].position > self.end {
                    self.end = self.pps[i].position;
                }
                self.pq_add(i);
            }
            return true;
        }
        for p in self.pps.iter_mut() {
            p.first_position();
        }
        if !self.advance_repeat_groups() {
            return false;
        }
        self.fill_queue();
        true
    }

    fn fill_queue(&mut self) {
        self.pq.clear();
        for i in 0..self.pps.len() {
            if self.pps[i].position > self.end {
                self.end = self.pps[i].position;
            }
            self.pq_add(i);
        }
    }

    fn advance_repeat_groups(&mut self) -> bool {
        for g in 0..self.rpt_groups.len() {
            if self.multi_term_rpts {
                let mut i = 0;
                while i < self.rpt_groups[g].len() {
                    let mut incr = 1;
                    let pp = self.rpt_groups[g][i];
                    while let Some(k) = self.collide(pp) {
                        let other = self.rpt_groups[g][k];
                        let pp2 = self.lesser(pp, other);
                        if !self.advance_pp(pp2) {
                            return false;
                        }
                        if self.pps[pp2].rpt_ind < i {
                            incr = 0;
                            break;
                        }
                    }
                    i += incr;
                }
            } else {
                let rg = self.rpt_groups[g].clone();
                for (j, &p) in rg.iter().enumerate().skip(1) {
                    for _ in 0..j {
                        if !self.pps[p].next_position() {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }

    fn init_first_time(&mut self) -> bool {
        self.checked_rpts = true;
        for p in self.pps.iter_mut() {
            p.first_position();
        }
        // Terms appearing in more than one slot.
        let mut counts: Vec<(Vec<u8>, usize)> = Vec::new();
        let mut rpt_terms: Vec<Vec<u8>> = Vec::new();
        for p in &self.pps {
            for t in &p.terms {
                match counts.iter_mut().find(|(x, _)| x == t) {
                    Some(c) => {
                        c.1 += 1;
                        if c.1 == 2 {
                            rpt_terms.push(t.clone());
                        }
                    }
                    None => counts.push((t.clone(), 1)),
                }
            }
        }
        self.has_rpts = !rpt_terms.is_empty();
        if self.has_rpts {
            let rpp: Vec<usize> = (0..self.pps.len()).filter(|&i| self.pps[i].terms.iter().any(|t| rpt_terms.contains(t))).collect();
            self.multi_term_rpts = rpp.iter().any(|&i| self.pps[i].terms.len() > 1);
            let mut groups: Vec<Vec<usize>> = Vec::new();
            if !self.multi_term_rpts {
                for a in 0..rpp.len() {
                    let i = rpp[a];
                    if self.pps[i].rpt_group >= 0 {
                        continue;
                    }
                    let tp = self.tp_pos(i);
                    for &j in &rpp[a + 1..] {
                        if self.pps[j].rpt_group >= 0 || self.pps[j].offset == self.pps[i].offset || self.tp_pos(j) != tp {
                            continue;
                        }
                        let mut g = self.pps[i].rpt_group;
                        if g < 0 {
                            g = groups.len() as i32;
                            self.pps[i].rpt_group = g;
                            groups.push(vec![i]);
                        }
                        self.pps[j].rpt_group = g;
                        groups[g as usize].push(j);
                    }
                }
            } else {
                // Union of term groups that share a term.
                let mut sets: Vec<Vec<usize>> = rpp
                    .iter()
                    .map(|&i| (0..rpt_terms.len()).filter(|&o| self.pps[i].terms.contains(&rpt_terms[o])).collect())
                    .collect();
                let mut i = 0;
                while i + 1 < sets.len() {
                    let mut incr = 1;
                    let mut j = i + 1;
                    while j < sets.len() {
                        if sets[i].iter().any(|o| sets[j].contains(o)) {
                            let add = sets.remove(j);
                            for o in add {
                                if !sets[i].contains(&o) {
                                    sets[i].push(o);
                                }
                            }
                            incr = 0;
                        } else {
                            j += 1;
                        }
                    }
                    i += incr;
                    if incr == 0 {
                        continue;
                    }
                }
                let group_of = |o: usize| sets.iter().position(|s| s.contains(&o)).unwrap();
                groups = vec![Vec::new(); sets.len()];
                for &p in &rpp {
                    for t in self.pps[p].terms.clone() {
                        if let Some(o) = rpt_terms.iter().position(|x| *x == t) {
                            let g = group_of(o);
                            if !groups[g].contains(&p) {
                                groups[g].push(p);
                            }
                            self.pps[p].rpt_group = g as i32;
                        }
                    }
                }
            }
            for g in groups.iter_mut() {
                g.sort_by_key(|&i| self.pps[i].offset);
                for (k, &i) in g.iter().enumerate() {
                    self.pps[i].rpt_ind = k;
                }
            }
            self.rpt_groups = groups;
            if !self.advance_repeat_groups() {
                return false;
            }
        }
        self.fill_queue();
        true
    }
}

struct PhraseWeight {
    field: u32,
    slots: Vec<(Vec<Vec<u8>>, i64)>,
    states: Vec<Vec<TermStates>>,
    slop: u32,
    sim: Arc<SimScorer>,
    scores: bool,
    mode: ScoreMode,
    desc: String,
}

impl PhraseWeight {
    fn new(s: &Searcher, field: u32, slots: Vec<(Vec<Vec<u8>>, i64)>, slop: u32, mode: ScoreMode, boost: f32, desc: String) -> PhraseWeight {
        let mut states = Vec::new();
        let mut stats = Vec::new();
        for (terms, _) in &slots {
            let mut v = Vec::new();
            for t in terms {
                let ts = TermStates::build(s, field, t);
                if mode.needs_scores() && ts.doc_freq > 0 {
                    stats.push(ts.stats());
                }
                v.push(ts);
            }
            states.push(v);
        }
        let sim = match scoring_stats(s, field, mode) {
            Some(c) if !stats.is_empty() => Some(s.schema().similarity(field).scorer(boost, &c, &stats)),
            _ => None,
        };
        let scores = sim.is_some();
        let sim = Arc::new(sim.unwrap_or(SimScorer::Boolean(1.0)));
        PhraseWeight { field, slots, states, slop, sim, scores, mode, desc }
    }

    fn matcher(&self, seg: &SegmentContext, offsets: bool) -> Result<Option<Matcher>> {
        let Some(tr) = seg.reader.terms(self.field) else { return Ok(None) };
        if !tr.has_positions() {
            return Err(Error::illegal_state(format!(
                "field \"{}\" was indexed without position data; cannot run PhraseQuery (phrase={})",
                self.desc.split(':').next().unwrap_or(""),
                self.desc
            )));
        }
        let flags = if offsets && tr.has_offsets() { PostingsFlags::OFFSETS } else { PostingsFlags::POSITIONS };
        seg.ensure_positions(self.field)?;
        let mut pps = Vec::with_capacity(self.slots.len());
        for (k, (terms, off)) in self.slots.iter().enumerate() {
            let mut subs = Vec::new();
            for (j, _) in terms.iter().enumerate() {
                if let Some(st) = self.states[k][j].get(seg) {
                    subs.push(tr.postings(st, flags));
                }
            }
            if subs.is_empty() {
                return Ok(None);
            }
            let slot = if subs.len() == 1 {
                Slot::One(subs.pop().unwrap())
            } else {
                Slot::Union { subs, doc: u32::MAX, pos: Vec::new(), idx: 0, loaded: false }
            };
            pps.push(PP {
                slot,
                offset: *off,
                ord: k,
                terms: terms.clone(),
                freq: 0,
                count: 0,
                upto: 0,
                position: -1,
                rpt_group: -1,
                rpt_ind: 0,
            });
        }
        Ok(Some(Matcher::new(pps, self.slop)))
    }

    fn scorer_impl(&self, seg: &SegmentContext) -> Result<Option<PhraseScorer>> {
        let Some(m) = self.matcher(seg, false)? else { return Ok(None) };
        let norms = if self.scores && self.mode.needs_scores() { Norms::of(seg, self.field) } else { Norms::None };
        let cache = MaxScoreCache::new(&self.sim);
        // Block-max bounds (Lucene's `ExactPhraseMatcher.mergeImpacts`): a phrase occurs in a
        // doc at most as often as any of its single terms, provided matches start at distinct
        // positions — true when the first slot is a single term (a union there can repeat a
        // position).
        let impacts = if self.slop == 0 && self.mode == ScoreMode::TopScores && m.pps.first().is_some_and(|p| matches!(p.slot, Slot::One(_))) {
            let tr = seg.reader.terms(self.field);
            let mut cursors = Vec::new();
            for (k, (terms, _)) in self.slots.iter().enumerate() {
                let present: Vec<_> = (0..terms.len()).filter_map(|j| self.states[k][j].get(seg)).collect();
                if let ([st], Some(tr)) = (present.as_slice(), tr.as_ref()) {
                    cursors.push(tr.postings(st, PostingsFlags::FREQS));
                }
            }
            if cursors.is_empty() { None } else { Some(PhraseImpacts::new(cursors)) }
        } else {
            None
        };
        Ok(Some(PhraseScorer {
            m,
            norms,
            sim: self.sim.clone(),
            freq: 0.0,
            doc: u32::MAX,
            min_competitive: 0.0,
            top_scores: self.mode == ScoreMode::TopScores,
            scores: self.mode.needs_scores(),
            cache,
            impacts,
            skip_upto: u32::MAX,
        }))
    }
}

/// The merged impacts of an exact phrase's single-term slots (Lucene's
/// `ExactPhraseMatcher.mergeImpacts`). Block boundaries come from the rarest term (the lead);
/// every other term contributes its narrowest level covering the lead's block, and the
/// merged frontier pairs each frequency with the largest norm any term allows at it: a doc
/// where the phrase occurs `f` times has every term at least `f` times, so its norm is at
/// least what each term's impacts require for `f`.
pub(crate) struct PhraseImpacts {
    cursors: Vec<PostingsCursor>,
    lead: usize,
    lists: Vec<Vec<Impact>>,
    merged: Vec<Impact>,
}

impl PhraseImpacts {
    fn new(cursors: Vec<PostingsCursor>) -> PhraseImpacts {
        let lead = (0..cursors.len()).min_by_key(|&i| cursors[i].cost()).unwrap_or(0);
        PhraseImpacts { lists: vec![Vec::new(); cursors.len()], cursors, lead, merged: Vec::new() }
    }
}

impl AdvanceShallow for PhraseImpacts {
    fn shallow(&mut self, target: u32) {
        for c in self.cursors.iter_mut() {
            c.advance_shallow(target);
        }
    }
}

impl ImpactSource for PhraseImpacts {
    fn impact_levels(&mut self) -> usize {
        self.cursors[self.lead].impact_levels()
    }

    fn impact_doc_upto(&mut self, level: usize) -> u32 {
        self.cursors[self.lead].impact_doc_upto(level)
    }

    fn impacts(&mut self, level: usize) -> &[Impact] {
        let upto = self.cursors[self.lead].impact_doc_upto(level);
        let mut n = 0;
        for c in self.cursors.iter_mut() {
            let levels = c.impact_levels();
            // A term without a level reaching the lead's block end bounds nothing: skip it.
            let Some(l) = (0..levels).find(|&l| c.impact_doc_upto(l) >= upto) else { continue };
            let list = c.impacts(l);
            if list.is_empty() {
                // That term has no docs left, so neither has the phrase.
                self.merged.clear();
                return &self.merged;
            }
            self.lists[n].clear();
            self.lists[n].extend_from_slice(list);
            n += 1;
        }
        self.merged.clear();
        if n == 0 {
            self.merged.push(Impact { freq: u32::MAX, norm: 0 });
            return &self.merged;
        }
        if n == 1 {
            self.merged.extend_from_slice(&self.lists[0]);
            return &self.merged;
        }
        merge_impacts(&self.lists[..n], &mut self.merged);
        &self.merged
    }
}

/// Merges impact frontiers (each sorted by increasing freq and norm) into the frontier of
/// "every list allows it": walking all lists by increasing freq, the current pair is the
/// smallest current freq with the largest norm reached so far by any list.
fn merge_impacts(lists: &[Vec<Impact>], out: &mut Vec<Impact>) {
    let mut idx = [0usize; 64];
    let lists = &lists[..lists.len().min(idx.len())];
    let min_list = |idx: &[usize]| -> usize { (0..lists.len()).min_by_key(|&i| lists[i][idx[i]].freq).unwrap_or(0) };
    let mut freq = lists.iter().map(|l| l[0].freq).min().unwrap_or(0);
    let mut norm = lists.iter().map(|l| l[0].norm).max().unwrap_or(0);
    loop {
        match out.last_mut() {
            Some(last) if last.norm == norm => last.freq = freq,
            _ => out.push(Impact { freq, norm }),
        }
        loop {
            let i = min_list(&idx);
            idx[i] += 1;
            if idx[i] == lists[i].len() {
                return;
            }
            norm = norm.max(lists[i][idx[i]].norm);
            let j = min_list(&idx);
            if lists[j][idx[j]].freq != freq {
                freq = lists[j][idx[j]].freq;
                break;
            }
        }
    }
}

/// Two-phase phrase scorer.
pub(crate) struct PhraseScorer {
    pub(crate) m: Matcher,
    norms: Norms,
    sim: Arc<SimScorer>,
    freq: f32,
    doc: u32,
    min_competitive: f32,
    top_scores: bool,
    scores: bool,
    cache: MaxScoreCache,
    impacts: Option<PhraseImpacts>,
    /// Last doc of the impacts block already found competitive (`u32::MAX`: none).
    skip_upto: u32,
}

impl PhraseScorer {
    /// Lucene's `ImpactsDISI` for phrases: skips blocks of the lead term whose best score
    /// (the phrase freq can't exceed the term's) can't reach the minimum competitive score.
    fn competitive_target(&mut self, target: u32) -> u32 {
        if !self.top_scores || self.min_competitive <= 0.0 || !self.sim.is_monotonic() {
            return target;
        }
        let Some(c) = self.impacts.as_mut() else { return target };
        if self.skip_upto != u32::MAX && target <= self.skip_upto {
            return target;
        }
        match competitive_target(c, &mut self.cache, &self.sim, self.min_competitive, target) {
            Some((t, upto)) => {
                self.skip_upto = upto;
                t
            }
            None => NO_MORE_DOCS,
        }
    }

    fn phrase_freq(&mut self) -> f32 {
        if self.freq == 0.0 {
            let mut f = self.m.sloppy_weight();
            while self.m.next_match() {
                f += self.m.sloppy_weight();
            }
            self.freq = f;
        }
        self.freq
    }
}

impl Scorable for PhraseScorer {
    fn score(&mut self) -> f32 {
        if !self.scores {
            return 0.0;
        }
        let f = self.phrase_freq();
        self.sim.score(f, self.norms.get(self.doc))
    }
    fn set_min_competitive_score(&mut self, m: f32) {
        if m > self.min_competitive {
            self.min_competitive = m;
            self.skip_upto = u32::MAX;
        }
    }
}

impl Scorer for PhraseScorer {
    fn doc(&self) -> u32 {
        self.doc
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
        self.m.cost()
    }
    fn has_two_phase(&self) -> bool {
        true
    }
    fn approx_next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc + 1 };
        self.approx_advance(t)
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.doc != u32::MAX && t <= self.doc {
            return self.doc;
        }
        let mut t = t;
        loop {
            let c = self.competitive_target(t);
            if c >= NO_MORE_DOCS {
                self.doc = self.m.approx_advance(NO_MORE_DOCS);
                return self.doc;
            }
            self.doc = self.m.approx_advance(c);
            // Landed past the competitive block: check the new block.
            if self.doc == NO_MORE_DOCS || self.skip_upto == u32::MAX || self.doc <= self.skip_upto || self.min_competitive <= 0.0 {
                return self.doc;
            }
            t = self.doc;
            self.skip_upto = u32::MAX;
        }
    }
    fn matches(&mut self) -> bool {
        if self.top_scores && self.min_competitive > 0.0 {
            let mf = self.m.max_freq();
            if self.sim.score(mf, self.norms.get(self.doc)) < self.min_competitive {
                return false;
            }
        }
        self.m.reset_positions();
        self.freq = 0.0;
        self.m.next_match()
    }
    fn match_cost(&self) -> f32 {
        self.m.pps.len() as f32 * 4.0
    }
    fn advance_shallow(&mut self, t: u32) -> u32 {
        match self.impacts.as_mut() {
            Some(c) => {
                c.shallow(t);
                c.impact_doc_upto(0)
            }
            None => NO_MORE_DOCS,
        }
    }
    fn max_score(&mut self, upto: u32) -> f32 {
        match self.impacts.as_mut() {
            Some(c) => self.cache.max_score(c, &self.sim, upto),
            None => {
                if self.sim.is_monotonic() {
                    self.sim.score(f32::MAX, 1)
                } else {
                    f32::INFINITY
                }
            }
        }
    }
    fn freq(&mut self) -> f32 {
        self.phrase_freq()
    }
}

impl Weight for PhraseWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.scorer_impl(seg)?.map(|s| Box::new(s) as Box<dyn Scorer>))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let Some(mut m) = self.matcher(seg, false)? else {
            return Ok(Explanation::no_match("no matching terms", vec![]));
        };
        if m.approx_advance(doc) != doc {
            return Ok(Explanation::no_match("no matching terms", vec![]));
        }
        m.reset_positions();
        if !m.next_match() {
            return Ok(Explanation::no_match("no matching phrase", vec![]));
        }
        let mut f = m.sloppy_weight();
        while m.next_match() {
            f += m.sloppy_weight();
        }
        let fe = Explanation::leaf(f, format!("phraseFreq={}", float_to_string(f)));
        let norm = if self.mode.needs_scores() { Norms::of(seg, self.field).get(doc) } else { 1 };
        let se = if self.scores {
            self.sim.explain(fe, norm)
        } else {
            Explanation::leaf(1f32, "")
        };
        Ok(Explanation::matched(
            se.value(),
            format!("weight({} in {doc}) [{SIMILARITY_NAME}], result of:", self.desc),
            vec![se],
        ))
    }
}

impl Query for PhraseQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let slots = self.terms.iter().zip(&self.positions).map(|(t, p)| (vec![t.clone()], *p as i64)).collect();
        Ok(Box::new(PhraseWeight::new(s, self.field, slots, self.slop, mode, boost, self.lucene_string(s.schema()))))
    }

    fn rewrite(&self, _s: &Searcher) -> Result<Option<QueryRef>> {
        if self.terms.is_empty() {
            return Ok(Some(Arc::new(crate::query::basic::MatchNoneQuery::new("empty PhraseQuery"))));
        }
        if self.terms.len() == 1 {
            return Ok(Some(Arc::new(TermQuery::new(self.field, self.terms[0].clone()))));
        }
        if self.positions[0] != 0 {
            let p0 = self.positions[0];
            return Ok(Some(Arc::new(PhraseQuery {
                field: self.field,
                terms: self.terms.clone(),
                positions: self.positions.iter().map(|p| p - p0).collect(),
                slop: self.slop,
            })));
        }
        Ok(None)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let mut b = format!("{}:\"", schema.name(self.field));
        let max = self.positions.last().copied().map_or(-1i64, |p| p as i64);
        let mut pieces: Vec<Option<String>> = vec![None; (max + 1) as usize];
        for (t, &p) in self.terms.iter().zip(&self.positions) {
            let s = &mut pieces[p as usize];
            *s = Some(match s.take() {
                None => term_text(t),
                Some(prev) => format!("{prev}|{}", term_text(t)),
            });
        }
        for (i, p) in pieces.iter().enumerate() {
            if i > 0 {
                b.push(' ');
            }
            match p {
                None => b.push('?'),
                Some(s) => b.push_str(s),
            }
        }
        b.push('"');
        if self.slop != 0 {
            b.push('~');
            b.push_str(&self.slop.to_string());
        }
        b
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        let slots: Vec<(u32, Vec<Vec<u8>>)> = self.terms.iter().zip(&self.positions).map(|(t, &p)| (p, vec![t.clone()])).collect();
        v.phrase(self.field, &slots, self.slop);
    }

    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "PhraseQuery", self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Query for MultiPhraseQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let slots = self.terms.iter().zip(&self.positions).map(|(t, p)| (t.clone(), *p as i64)).collect();
        Ok(Box::new(PhraseWeight::new(s, self.field, slots, self.slop, mode, boost, self.lucene_string(s.schema()))))
    }

    fn rewrite(&self, _s: &Searcher) -> Result<Option<QueryRef>> {
        if self.terms.is_empty() {
            return Ok(Some(Arc::new(crate::query::basic::MatchNoneQuery::new("empty MultiPhraseQuery"))));
        }
        if self.terms.len() == 1 {
            let mut b = BooleanQuery::new();
            for t in &self.terms[0] {
                b = b.add(Occur::Should, Arc::new(TermQuery::new(self.field, t.clone())));
            }
            return Ok(Some(Arc::new(b)));
        }
        Ok(None)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let mut b = format!("{}:\"", schema.name(self.field));
        let mut last: i64 = -1;
        for (i, (terms, &p)) in self.terms.iter().zip(&self.positions).enumerate() {
            if i != 0 {
                b.push(' ');
                for _ in 1..(p as i64 - last) {
                    b.push_str("? ");
                }
            }
            if terms.len() > 1 {
                b.push('(');
                let parts: Vec<String> = terms.iter().map(|t| term_text(t)).collect();
                b.push_str(&parts.join(" "));
                b.push(')');
            } else if let Some(t) = terms.first() {
                b.push_str(&term_text(t));
            }
            last = p as i64;
        }
        b.push('"');
        if self.slop != 0 {
            b.push('~');
            b.push_str(&self.slop.to_string());
        }
        b
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        let slots: Vec<(u32, Vec<Vec<u8>>)> = self.terms.iter().cloned().zip(self.positions.iter().copied()).map(|(t, p)| (p, t)).collect();
        v.phrase(self.field, &slots, self.slop);
    }

    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        crate::search::hash_debug(h, "MultiPhraseQuery", self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Elasticsearch's `MultiPhrasePrefixQuery` (`match_phrase_prefix`): a multi-phrase whose last
/// position accepts every term starting with one of its prefixes. Rewriting expands the
/// prefixes against the index — segment by segment, terms in order, at most `max_expansions`
/// distinct terms — into a [`MultiPhraseQuery`]; no expansion matches nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiPhrasePrefixQuery {
    pub field: u32,
    /// Terms per position; the last entry holds the prefixes.
    pub terms: Vec<Vec<Vec<u8>>>,
    pub positions: Vec<u32>,
    pub slop: u32,
    pub max_expansions: usize,
}

impl MultiPhrasePrefixQuery {
    pub fn new(field: u32, terms: Vec<Vec<Vec<u8>>>, positions: Vec<u32>, slop: u32, max_expansions: usize) -> Result<MultiPhrasePrefixQuery> {
        if terms.len() != positions.len() {
            return Err(Error::illegal("Must have as many terms as positions"));
        }
        Ok(MultiPhrasePrefixQuery { field, terms, positions, slop, max_expansions })
    }

    /// The prefixes' terms, in Elasticsearch's collection order (deduplicated).
    fn expansions(&self, s: &Searcher) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let Some(prefixes) = self.terms.last() else { return out };
        'prefixes: for prefix in prefixes {
            for seg in s.segments() {
                let Some(tr) = seg.reader.terms(self.field) else { continue };
                let mut it = tr.seek_ceil(prefix);
                while let Some(t) = it.next() {
                    if !t.starts_with(prefix) {
                        break;
                    }
                    if seen.insert(t.to_vec()) {
                        out.push(t.to_vec());
                    }
                    if out.len() >= self.max_expansions {
                        continue 'prefixes;
                    }
                }
            }
            if out.len() > self.max_expansions {
                break;
            }
        }
        out.sort();
        out
    }
}

impl Query for MultiPhrasePrefixQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let r = self.rewrite(s)?.expect("always rewrites");
        r.weight(s, mode, boost)
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        if self.terms.is_empty() {
            return Ok(Some(Arc::new(crate::query::basic::MatchNoneQuery::new(""))));
        }
        let expanded = self.expansions(s);
        if expanded.is_empty() {
            return Ok(Some(Arc::new(crate::query::basic::MatchNoneQuery::new(
                "No terms supplied for org.elasticsearch.common.lucene.search.MultiPhrasePrefixQuery",
            ))));
        }
        let mut terms = self.terms.clone();
        *terms.last_mut().unwrap() = expanded;
        Ok(Some(Arc::new(MultiPhraseQuery::new(self.field, terms, self.positions.clone(), self.slop)?)))
    }

    /// Elasticsearch's `toString`: the last position's prefixes end with `*`.
    fn lucene_string(&self, schema: &Schema) -> String {
        let mut b = format!("{}:\"", schema.name(self.field));
        let n = self.terms.len();
        for (i, terms) in self.terms.iter().enumerate() {
            let last = i + 1 == n;
            if terms.len() > 1 {
                b.push('(');
                for (j, t) in terms.iter().enumerate() {
                    b.push_str(&term_text(t));
                    if j + 1 < terms.len() {
                        b.push_str(if last { "* " } else { " " });
                    }
                }
                b.push_str(if last { "*)" } else { ") " });
            } else if let Some(t) = terms.first() {
                b.push_str(&term_text(t));
                b.push_str(if last { "*" } else { " " });
            }
        }
        b.push('"');
        if self.slop != 0 {
            b.push('~');
            b.push_str(&self.slop.to_string());
        }
        b
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        let n = self.terms.len();
        for (i, terms) in self.terms.iter().enumerate() {
            if i + 1 < n {
                for t in terms {
                    v.term(self.field, t);
                }
            } else {
                let prefixes = terms.clone();
                v.multi_term(self.field, Arc::new(move |t: &[u8]| prefixes.iter().any(|p| t.starts_with(p))));
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frontier: increasing freq and norm (each pair is competitive).
    fn frontier(seed: &mut u64, len: usize) -> Vec<Impact> {
        let mut next = || {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed
        };
        let (mut f, mut n) = (0u32, 0u8);
        (0..len)
            .map(|_| {
                f += 1 + (next() % 3) as u32;
                n = n.saturating_add(1 + (next() % 4) as u8);
                Impact { freq: f, norm: n }
            })
            .collect()
    }

    fn allows(list: &[Impact], f: u32, n: u8) -> bool {
        list.iter().any(|i| i.freq >= f && i.norm <= n)
    }

    #[test]
    fn merged_impacts_bound_every_doc_all_terms_allow() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..500 {
            let k = 2 + round % 3;
            let lists: Vec<Vec<Impact>> = (0..k).map(|i| frontier(&mut seed, 1 + (round + i) % 5)).collect();
            let mut merged = Vec::new();
            merge_impacts(&lists, &mut merged);
            assert!(merged.windows(2).all(|w| w[0].freq < w[1].freq && w[0].norm < w[1].norm), "{merged:?}");
            for f in 1..40u32 {
                for n in 0..=60u8 {
                    let every = lists.iter().all(|l| allows(l, f, n));
                    // Every (freq, norm) a phrase doc can have is dominated by a merged impact,
                    // and the merge allows nothing some term forbids.
                    assert_eq!(allows(&merged, f, n), every, "f={f} n={n} lists={lists:?} merged={merged:?}");
                }
            }
        }
    }

    #[test]
    fn merged_impacts_of_lucene_example() {
        let l = |v: &[(u32, u8)]| v.iter().map(|&(freq, norm)| Impact { freq, norm }).collect::<Vec<_>>();
        let mut out = Vec::new();
        merge_impacts(&[l(&[(1, 10), (3, 20)]), l(&[(2, 5)])], &mut out);
        assert_eq!(out, l(&[(1, 10), (2, 20)]));
    }
}
