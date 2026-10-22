//! Scorer combinators: conjunctions, disjunctions (sum and WAND), exclusions, required plus
//! optional, and the bulk scorers top-k boolean queries run with (MaxScore, score-first
//! block-max conjunctions, windowed disjunctions, exclusion windows).
//!
//! Scores of combined clauses are summed in `f64` and cast to `f32` once, like Lucene: a sum of
//! a few floats is exact in double, so the summation order (which differs between scorer
//! implementations) never changes a score, and every implementation agrees with
//! Elasticsearch to the last bit.
//!
//! Pruning (WAND and MaxScore) relies on `max_score` upper bounds that are themselves rounded
//! up (`sum_upper_bound`), so a competitive doc is never skipped.

use search_codec::{LiveDocs, NO_MORE_DOCS};

use crate::error::Result;
use crate::search::{BATCH, BulkScorer, DocScoreBuf, LeafCollector, Scorable, Scorer};
use crate::util::bitset::FixedBitSet;

/// Upper bound of a sum of `n` floats computed in double in any order (Lucene
/// `MathUtil.sumUpperBound`).
#[inline]
pub fn sum_upper_bound(sum: f64, n: usize) -> f64 {
    if n <= 2 {
        return sum;
    }
    let b = (n - 1) as f64 * f64::EPSILON / 2.0 * 2.0;
    (1.0 + 2.0 * b) * sum
}

/// A score bound as pruning uses it: NaN (an undefined bound, e.g. `inf - inf`) compares as
/// "can't compete", so it becomes unbounded.
#[inline]
pub fn bound(m: f32) -> f32 {
    if m.is_nan() { f32::INFINITY } else { m }
}

/// Lucene `ScorerUtil.costWithMinShouldMatch`: the cost of matching at least `msm` of the
/// clauses is the sum of all but the `msm - 1` most costly.
pub fn cost_with_msm(costs: &mut [u64], msm: usize) -> u64 {
    costs.sort_unstable();
    let n = costs.len();
    let keep = n.saturating_sub(msm.saturating_sub(1));
    costs[..keep].iter().fold(0u64, |a, &c| a.saturating_add(c))
}

#[inline]
fn approx_doc_next(s: &mut dyn Scorer) -> u32 {
    s.approx_next()
}

// ------------------------------------------------------------------ conjunction

/// Docs matching every sub-scorer; scores sum the scoring ones.
pub struct ConjunctionScorer {
    /// Sorted by cost: `subs[0]` leads.
    subs: Vec<Box<dyn Scorer>>,
    scoring: Vec<bool>,
    /// Indices of two-phase subs, cheapest confirmation first.
    two_phase: Vec<usize>,
    doc: u32,
}

impl ConjunctionScorer {
    /// `subs` with flags telling which contribute to the score.
    pub fn new(subs: Vec<(Box<dyn Scorer>, bool)>) -> ConjunctionScorer {
        let mut subs = subs;
        subs.sort_by_key(|(s, _)| s.cost());
        let scoring = subs.iter().map(|(_, b)| *b).collect();
        let subs: Vec<Box<dyn Scorer>> = subs.into_iter().map(|(s, _)| s).collect();
        let mut two_phase: Vec<usize> = (0..subs.len()).filter(|&i| subs[i].has_two_phase()).collect();
        two_phase.sort_by(|&a, &b| subs[a].match_cost().total_cmp(&subs[b].match_cost()));
        ConjunctionScorer { subs, scoring, two_phase, doc: u32::MAX }
    }

    /// Leap-frogs the approximations from the lead's doc `doc`.
    fn do_next(&mut self, mut doc: u32) -> u32 {
        'head: loop {
            if doc == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return NO_MORE_DOCS;
            }
            for i in 1..self.subs.len() {
                let s = &mut self.subs[i];
                let d = s.doc();
                if d == u32::MAX || d < doc {
                    let n = s.approx_advance(doc);
                    if n > doc {
                        doc = self.subs[0].approx_advance(n);
                        continue 'head;
                    }
                }
            }
            self.doc = doc;
            return doc;
        }
    }
}

impl Scorable for ConjunctionScorer {
    fn score(&mut self) -> f32 {
        let mut s = 0f64;
        for (i, sub) in self.subs.iter_mut().enumerate() {
            if self.scoring[i] {
                s += sub.score() as f64;
            }
        }
        s as f32
    }
    fn set_min_competitive_score(&mut self, min: f32) {
        // Only safe to propagate with a single scoring clause.
        let n = self.scoring.iter().filter(|&&b| b).count();
        if n == 1 {
            let i = self.scoring.iter().position(|&b| b).unwrap();
            self.subs[i].set_min_competitive_score(min);
        }
    }
}

impl Scorer for ConjunctionScorer {
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
        self.subs.first().map_or(0, |s| s.cost())
    }
    fn has_two_phase(&self) -> bool {
        !self.two_phase.is_empty()
    }
    fn approx_next(&mut self) -> u32 {
        let d = approx_doc_next(self.subs[0].as_mut());
        self.do_next(d)
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.doc != u32::MAX && t <= self.doc {
            return self.doc;
        }
        let d = self.subs[0].approx_advance(t);
        self.do_next(d)
    }
    fn matches(&mut self) -> bool {
        for k in 0..self.two_phase.len() {
            let i = self.two_phase[k];
            if !self.subs[i].matches() {
                return false;
            }
        }
        true
    }
    fn match_cost(&self) -> f32 {
        self.two_phase.iter().map(|&i| self.subs[i].match_cost()).sum()
    }
    fn advance_shallow(&mut self, t: u32) -> u32 {
        let mut up = NO_MORE_DOCS;
        for (i, s) in self.subs.iter_mut().enumerate() {
            if self.scoring[i] {
                up = up.min(s.advance_shallow(t));
            }
        }
        up
    }
    fn max_score(&mut self, upto: u32) -> f32 {
        let mut m = 0f64;
        let mut n = 0;
        for (i, s) in self.subs.iter_mut().enumerate() {
            if self.scoring[i] {
                m += s.max_score(upto) as f64;
                n += 1;
            }
        }
        sum_upper_bound(m, n) as f32
    }
}

// ------------------------------------------------------------------ disjunction heap

/// A min-heap of sub-scorer indices ordered by their current (approximation) doc.
struct DocHeap {
    heap: Vec<usize>,
}

impl DocHeap {
    fn new() -> DocHeap {
        DocHeap { heap: Vec::new() }
    }

    #[inline]
    fn top(&self) -> Option<usize> {
        self.heap.first().copied()
    }

    fn push(&mut self, i: usize, docs: &[u32]) {
        self.heap.push(i);
        let mut k = self.heap.len() - 1;
        while k > 0 {
            let p = (k - 1) / 2;
            if docs[self.heap[p]] <= docs[self.heap[k]] {
                break;
            }
            self.heap.swap(p, k);
            k = p;
        }
    }

    fn pop(&mut self, docs: &[u32]) -> Option<usize> {
        if self.heap.is_empty() {
            return None;
        }
        let last = self.heap.len() - 1;
        self.heap.swap(0, last);
        let r = self.heap.pop();
        self.sift_down(docs);
        r
    }

    #[inline]
    fn sift_down(&mut self, docs: &[u32]) {
        let n = self.heap.len();
        let mut k = 0;
        loop {
            let l = 2 * k + 1;
            if l >= n {
                break;
            }
            let r = l + 1;
            let c = if r < n && docs[self.heap[r]] < docs[self.heap[l]] { r } else { l };
            if docs[self.heap[k]] <= docs[self.heap[c]] {
                break;
            }
            self.heap.swap(k, c);
            k = c;
        }
    }

    fn len(&self) -> usize {
        self.heap.len()
    }
}

/// Docs matching any sub-scorer; scores sum the matching ones (Lucene `DisjunctionSumScorer`).
pub struct DisjunctionScorer {
    subs: Vec<Box<dyn Scorer>>,
    docs: Vec<u32>,
    heap: DocHeap,
    doc: u32,
    /// Subs positioned on `doc` (approximation), and those confirmed.
    on_doc: Vec<usize>,
    verified: Vec<usize>,
    two_phase: bool,
    cost: u64,
    /// `true`: score = max + tie * (sum - max) (dis_max) instead of sum.
    dismax: Option<f32>,
    needs_scores: bool,
}

impl DisjunctionScorer {
    pub fn new(subs: Vec<Box<dyn Scorer>>, needs_scores: bool) -> DisjunctionScorer {
        let two_phase = subs.iter().any(|s| s.has_two_phase());
        let cost = subs.iter().fold(0u64, |a, s| a.saturating_add(s.cost()));
        let n = subs.len();
        DisjunctionScorer {
            subs,
            docs: vec![u32::MAX; n],
            heap: DocHeap::new(),
            doc: u32::MAX,
            on_doc: Vec::with_capacity(n),
            verified: Vec::with_capacity(n),
            two_phase,
            cost,
            dismax: None,
            needs_scores,
        }
    }

    /// Dis-max scoring: `max + tie_breaker * (sum of others)`.
    pub fn dismax(subs: Vec<Box<dyn Scorer>>, tie: f32, needs_scores: bool) -> DisjunctionScorer {
        let mut d = DisjunctionScorer::new(subs, needs_scores);
        d.dismax = Some(tie);
        d
    }

    fn approx_to(&mut self, target: u32) -> u32 {
        if self.doc == u32::MAX {
            // First move: position every sub, then heapify.
            for i in 0..self.subs.len() {
                let d = self.subs[i].approx_advance(target);
                self.docs[i] = d;
                if d != NO_MORE_DOCS {
                    self.heap.push(i, &self.docs);
                }
            }
        }
        loop {
            let Some(top) = self.heap.top() else {
                self.doc = NO_MORE_DOCS;
                return NO_MORE_DOCS;
            };
            if self.docs[top] >= target {
                break;
            }
            let nd = self.subs[top].approx_advance(target);
            self.docs[top] = nd;
            if nd == NO_MORE_DOCS {
                self.heap.pop(&self.docs);
            } else {
                self.heap.sift_down(&self.docs);
            }
        }
        let top = self.heap.top().unwrap();
        self.doc = self.docs[top];
        self.collect_on_doc();
        self.doc
    }

    fn collect_on_doc(&mut self) {
        self.on_doc.clear();
        self.verified.clear();
        let doc = self.doc;
        // Walk the heap (a subtree is skipped once its root is past doc).
        let mut stack: smallvec::SmallVec<[usize; 16]> = smallvec::SmallVec::new();
        if !self.heap.heap.is_empty() {
            stack.push(0);
        }
        while let Some(k) = stack.pop() {
            let i = self.heap.heap[k];
            if self.docs[i] != doc {
                continue;
            }
            self.on_doc.push(i);
            let l = 2 * k + 1;
            if l < self.heap.len() {
                stack.push(l);
            }
            if l + 1 < self.heap.len() {
                stack.push(l + 1);
            }
        }
        if !self.two_phase {
            self.verified.extend_from_slice(&self.on_doc);
        }
    }
}

impl Scorable for DisjunctionScorer {
    fn score(&mut self) -> f32 {
        if !self.needs_scores {
            return 0.0;
        }
        if self.two_phase && self.verified.is_empty() {
            self.matches();
        }
        match self.dismax {
            None => {
                let mut s = 0f64;
                for k in 0..self.verified.len() {
                    let i = self.verified[k];
                    s += self.subs[i].score() as f64;
                }
                s as f32
            }
            Some(tie) => {
                // Lucene's DisjunctionMaxScorer: max starts at 0, others summed in double.
                let mut max = 0f32;
                let mut other = 0f64;
                for k in 0..self.verified.len() {
                    let i = self.verified[k];
                    let s = self.subs[i].score();
                    if s >= max {
                        other += max as f64;
                        max = s;
                    } else {
                        other += s as f64;
                    }
                }
                if tie == 0.0 {
                    return max;
                }
                (max as f64 + other * tie as f64) as f32
            }
        }
    }
}

impl Scorer for DisjunctionScorer {
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
        self.cost
    }
    fn has_two_phase(&self) -> bool {
        self.two_phase
    }
    fn approx_next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc.saturating_add(1) };
        self.approx_to(t)
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.doc != u32::MAX && t <= self.doc {
            return self.doc;
        }
        self.approx_to(t)
    }
    fn matches(&mut self) -> bool {
        if !self.two_phase {
            return true;
        }
        self.verified.clear();
        let mut on = std::mem::take(&mut self.on_doc);
        on.sort_by(|&a, &b| self.subs[a].match_cost().total_cmp(&self.subs[b].match_cost()));
        for &i in &on {
            if !self.subs[i].has_two_phase() || self.subs[i].matches() {
                self.verified.push(i);
                if !self.needs_scores {
                    break;
                }
            }
        }
        self.on_doc = on;
        !self.verified.is_empty()
    }
    fn match_cost(&self) -> f32 {
        let tp: f32 = self.subs.iter().filter(|s| s.has_two_phase()).map(|s| s.match_cost()).sum();
        tp / self.subs.len().max(1) as f32
    }
    fn advance_shallow(&mut self, t: u32) -> u32 {
        let mut up = NO_MORE_DOCS;
        for (i, s) in self.subs.iter_mut().enumerate() {
            if self.docs[i] == u32::MAX || self.docs[i] <= t || self.docs[i] == self.doc {
                up = up.min(s.advance_shallow(t));
            }
        }
        up
    }
    fn max_score(&mut self, upto: u32) -> f32 {
        match self.dismax {
            None => {
                let mut m = 0f64;
                for (i, s) in self.subs.iter_mut().enumerate() {
                    if self.docs[i] == u32::MAX || self.docs[i] <= upto {
                        m += s.max_score(upto) as f64;
                    }
                }
                sum_upper_bound(m, self.subs.len()) as f32
            }
            Some(tie) => {
                // Lucene's DisjunctionMaxScorer: the others are summed apart from the max, never
                // as `sum - max`, which turns an infinite bound into NaN.
                let mut max = 0f32;
                let mut other = 0f64;
                for (i, s) in self.subs.iter_mut().enumerate() {
                    if self.docs[i] == u32::MAX || self.docs[i] <= upto {
                        let v = s.max_score(upto);
                        if v >= max {
                            other += max as f64;
                            max = v;
                        } else {
                            other += v as f64;
                        }
                    }
                }
                if tie == 0.0 {
                    return max;
                }
                sum_upper_bound(max as f64 + other * tie as f64, self.subs.len()) as f32
            }
        }
    }
}

// ------------------------------------------------------------------ WAND

/// Scales float score bounds to integers so sums are exact (Lucene `WANDScorer`).
fn scaling_factor(f: f32) -> i32 {
    if f <= 0.0 {
        return scaling_factor(f32::MIN_POSITIVE) + 1;
    }
    if f.is_infinite() {
        return scaling_factor(f32::MAX) - 1;
    }
    let exp = ((f as f64).to_bits() >> 52 & 0x7ff) as i32 - 1023;
    24 - 1 - exp
}

const MAX_SCALED: i64 = (1 << 24) - 1;

fn scale_max(v: f32, sf: i32) -> i64 {
    let s = (v as f64) * 2f64.powi(sf);
    // NaN (an undefined bound) must not scale to 0, which would prune the clause.
    if s.is_nan() || s > MAX_SCALED as f64 {
        return MAX_SCALED;
    }
    s.ceil() as i64
}

fn scale_min(v: f32, sf: i32) -> i64 {
    ((v as f64) * 2f64.powi(sf)).floor() as i64
}

/// Weak-AND disjunction for top-k scoring and `minimum_should_match` (Lucene `WANDScorer`):
/// sub-scorers whose summed score bounds can't reach the competitive score (or whose count
/// can't reach `msm`) wait in a "tail" and are advanced only when a candidate needs them.
/// Subs move with their confirmed iterators (as Lucene's do), so every lead matches and its
/// score counts.
pub struct WandScorer {
    subs: Vec<Box<dyn Scorer>>,
    docs: Vec<u32>,
    scaled: Vec<i64>,
    costs: Vec<u64>,
    lead: Vec<usize>,
    lead_score: f64,
    doc: u32,
    head: DocHeap,
    tail: Vec<usize>,
    tail_max: i64,
    up_to: i64,
    msm: usize,
    top_scores: bool,
    needs_scores: bool,
    sf: i32,
    min_competitive: i64,
    cost: u64,
    lead_cost: u64,
    freq: usize,
    /// The doc `matches` last answered for, and the answer.
    lead_doc: u32,
    lead_ok: bool,
}

impl WandScorer {
    pub fn new(mut subs: Vec<Box<dyn Scorer>>, msm: usize, top_scores: bool, needs_scores: bool, lead_cost: u64) -> WandScorer {
        let n = subs.len();
        let sf = if top_scores {
            let mut sum = 0f64;
            for s in subs.iter_mut() {
                s.advance_shallow(0);
                sum += s.max_score(NO_MORE_DOCS) as f64;
            }
            scaling_factor(sum_upper_bound(sum, n) as f32)
        } else {
            0
        };
        let mut costs: Vec<u64> = subs.iter().map(|s| s.cost()).collect();
        let cost = cost_with_msm(&mut costs.clone(), msm.max(1));
        costs = subs.iter().map(|s| s.cost()).collect();
        WandScorer {
            docs: vec![u32::MAX; n],
            scaled: vec![0; n],
            costs,
            lead: (0..n).collect(),
            lead_score: 0.0,
            doc: u32::MAX,
            head: DocHeap::new(),
            tail: Vec::with_capacity(n),
            tail_max: 0,
            up_to: -1,
            msm,
            top_scores,
            needs_scores,
            sf,
            min_competitive: 0,
            cost,
            lead_cost,
            freq: n,
            lead_doc: u32::MAX,
            lead_ok: false,
            subs,
        }
    }

    #[inline]
    fn greater_max(&self, a: usize, b: usize) -> bool {
        self.scaled[a] > self.scaled[b] || (self.scaled[a] == self.scaled[b] && self.costs[a] < self.costs[b])
    }

    fn tail_up(&mut self, mut i: usize) {
        let node = self.tail[i];
        while i > 0 {
            let p = (i - 1) / 2;
            if !self.greater_max(node, self.tail[p]) {
                break;
            }
            self.tail[i] = self.tail[p];
            i = p;
        }
        self.tail[i] = node;
    }

    fn tail_down(&mut self) {
        let size = self.tail.len();
        if size == 0 {
            return;
        }
        let node = self.tail[0];
        let mut i = 0;
        loop {
            let l = 2 * i + 1;
            if l >= size {
                break;
            }
            let r = l + 1;
            let c = if r < size && self.greater_max(self.tail[r], self.tail[l]) { r } else { l };
            if !self.greater_max(self.tail[c], node) {
                break;
            }
            self.tail[i] = self.tail[c];
            i = c;
        }
        self.tail[i] = node;
    }

    fn add_tail(&mut self, s: usize) {
        self.tail.push(s);
        let k = self.tail.len() - 1;
        self.tail_up(k);
    }

    fn pop_tail(&mut self) -> usize {
        let r = self.tail[0];
        let last = self.tail.pop().unwrap();
        if !self.tail.is_empty() {
            self.tail[0] = last;
            self.tail_down();
        }
        self.tail_max -= self.scaled[r];
        r
    }

    /// Adds `s` to the tail unless that would make the tail competitive; returns the scorer
    /// that must go to the head instead (`s` or the evicted tail top).
    fn insert_tail_overflow(&mut self, s: usize) -> Option<usize> {
        if self.tail_max + self.scaled[s] < self.min_competitive || self.tail.len() + 1 < self.msm {
            self.add_tail(s);
            self.tail_max += self.scaled[s];
            None
        } else if self.tail.is_empty() {
            Some(s)
        } else {
            let top = self.tail[0];
            if !self.greater_max(top, s) {
                return Some(s);
            }
            self.tail[0] = s;
            self.tail_down();
            self.tail_max = self.tail_max - self.scaled[top] + self.scaled[s];
            Some(top)
        }
    }

    fn push_back_leads(&mut self, target: u32) {
        let lead = std::mem::take(&mut self.lead);
        for &s in &lead {
            if let Some(e) = self.insert_tail_overflow(s) {
                self.docs[e] = self.subs[e].advance(target);
                self.head.push(e, &self.docs);
            }
        }
        self.lead = lead;
        self.lead.clear();
    }

    fn advance_head(&mut self, target: u32) -> Option<usize> {
        while let Some(top) = self.head.top() {
            if self.docs[top] >= target {
                return Some(top);
            }
            match self.insert_tail_overflow(top) {
                Some(e) => {
                    self.docs[e] = self.subs[e].advance(target);
                    self.head.heap[0] = e;
                    self.head.sift_down(&self.docs);
                }
                None => {
                    self.head.pop(&self.docs);
                }
            }
        }
        None
    }

    fn update_max_scores(&mut self, target: u32) {
        let mut new_up = NO_MORE_DOCS;
        for k in 0..self.head.heap.len() {
            let w = self.head.heap[k];
            if self.docs[w] <= new_up && self.costs[w] <= self.lead_cost {
                let d = self.docs[w];
                new_up = new_up.min(self.subs[w].advance_shallow(d));
            }
        }
        if new_up == NO_MORE_DOCS && !self.tail.is_empty() && self.costs[self.tail[0]] <= self.lead_cost {
            new_up = self.subs[self.tail[0]].advance_shallow(target);
            if let Some(t) = self.head.top() {
                new_up = new_up.max(self.docs[t]);
            }
        }
        self.up_to = new_up as i64;
        for k in 0..self.head.heap.len() {
            let w = self.head.heap[k];
            if self.docs[w] <= new_up {
                self.scaled[w] = scale_max(self.subs[w].max_score(new_up), self.sf);
            }
        }
        self.tail_max = 0;
        for k in 0..self.tail.len() {
            let w = self.tail[k];
            self.subs[w].advance_shallow(target);
            self.scaled[w] = scale_max(self.subs[w].max_score(new_up), self.sf);
            self.tail_up(k);
        }
        for k in 0..self.tail.len() {
            self.tail_max += self.scaled[self.tail[k]];
        }
        while !self.tail.is_empty() && self.tail_max >= self.min_competitive {
            let w = self.pop_tail();
            self.docs[w] = self.subs[w].advance(target);
            self.head.push(w, &self.docs);
        }
    }

    fn move_to_next_block(&mut self, mut target: u32) {
        while self.up_to < NO_MORE_DOCS as i64 {
            match self.head.top() {
                None => {
                    target = target.max((self.up_to + 1) as u32);
                    self.update_max_scores(target);
                }
                Some(t) if self.docs[t] as i64 > self.up_to => {
                    self.update_max_scores(target);
                    break;
                }
                _ => break,
            }
        }
    }

    fn add_lead(&mut self, s: usize) {
        self.lead.push(s);
        self.freq += 1;
        if self.top_scores {
            self.lead_score += self.subs[s].score() as f64;
        }
    }

    fn move_to_next_candidate(&mut self) -> bool {
        let Some(l) = self.head.pop(&self.docs) else { return false };
        self.lead.clear();
        self.lead.push(l);
        self.freq = 1;
        if self.top_scores {
            self.lead_score = self.subs[l].score() as f64;
        }
        while let Some(t) = self.head.top() {
            if self.docs[t] != self.doc {
                break;
            }
            let s = self.head.pop(&self.docs).unwrap();
            self.add_lead(s);
        }
        true
    }

    fn advance_tail_one(&mut self, s: usize) {
        self.docs[s] = self.subs[s].advance(self.doc);
        if self.docs[s] == self.doc {
            self.add_lead(s);
        } else {
            self.head.push(s, &self.docs);
        }
    }

    fn advance_all_tail(&mut self) {
        while let Some(&s) = self.tail.last() {
            self.tail.pop();
            self.advance_tail_one(s);
        }
        self.tail_max = 0;
    }

    fn approx_to(&mut self, target: u32) -> u32 {
        if self.doc != u32::MAX && target <= self.doc {
            return self.doc;
        }
        self.push_back_leads(target);
        let mut top = self.advance_head(target);
        if self.top_scores && top.is_none_or(|t| self.docs[t] as i64 > self.up_to) {
            self.move_to_next_block(target);
            top = self.head.top();
        }
        self.doc = match top {
            None => NO_MORE_DOCS,
            Some(t) => self.docs[t],
        };
        self.doc
    }

    fn scaled_lead(&self) -> i64 {
        if self.top_scores { scale_max(sum_upper_bound(self.lead_score, 24) as f32, self.sf) } else { 0 }
    }
}

impl Scorable for WandScorer {
    fn score(&mut self) -> f32 {
        self.advance_all_tail();
        if !self.needs_scores {
            return 0.0;
        }
        let mut s = 0f64;
        for k in 0..self.lead.len() {
            let i = self.lead[k];
            s += self.subs[i].score() as f64;
        }
        s as f32
    }

    fn set_min_competitive_score(&mut self, min: f32) {
        if self.top_scores {
            let m = scale_min(min, self.sf);
            if m > self.min_competitive {
                self.min_competitive = m;
            }
        }
    }
}

impl Scorer for WandScorer {
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
        self.cost
    }
    fn has_two_phase(&self) -> bool {
        true
    }
    fn approx_next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc.saturating_add(1) };
        self.approx_to(t)
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        self.approx_to(t)
    }
    fn matches(&mut self) -> bool {
        if self.lead_doc == self.doc {
            // Already answered for this doc (leads are in place).
            return self.lead_ok;
        }
        self.lead_doc = self.doc;
        self.lead_ok = false;
        if self.doc == NO_MORE_DOCS || !self.move_to_next_candidate() {
            return false;
        }
        let mut scaled = self.scaled_lead();
        while scaled < self.min_competitive || self.freq < self.msm {
            if scaled + self.tail_max < self.min_competitive || self.freq + self.tail.len() < self.msm {
                return false;
            }
            let before = self.lead.len();
            let s = self.pop_tail();
            self.advance_tail_one(s);
            if self.top_scores && self.lead.len() != before {
                scaled = self.scaled_lead();
            }
        }
        self.lead_ok = true;
        true
    }
    fn match_cost(&self) -> f32 {
        self.subs.len() as f32
    }
    fn advance_shallow(&mut self, t: u32) -> u32 {
        for (i, s) in self.subs.iter_mut().enumerate() {
            if self.docs[i] == u32::MAX || self.docs[i] < t {
                s.advance_shallow(t);
            }
        }
        if (t as i64) <= self.up_to { self.up_to as u32 } else { NO_MORE_DOCS }
    }
    fn max_score(&mut self, upto: u32) -> f32 {
        let mut m = 0f64;
        for (i, s) in self.subs.iter_mut().enumerate() {
            if self.docs[i] == u32::MAX || self.docs[i] <= upto {
                m += s.max_score(upto) as f64;
            }
        }
        sum_upper_bound(m, self.subs.len()) as f32
    }
}

// ------------------------------------------------------------------ exclusion

/// `req` minus docs matching `excl` (Lucene `ReqExclScorer`).
pub struct ReqExclScorer {
    req: Box<dyn Scorer>,
    excl: Box<dyn Scorer>,
}

impl ReqExclScorer {
    pub fn new(req: Box<dyn Scorer>, excl: Box<dyn Scorer>) -> ReqExclScorer {
        ReqExclScorer { req, excl }
    }

    fn excluded(&mut self, doc: u32) -> bool {
        let mut e = self.excl.doc();
        if e == u32::MAX || e < doc {
            e = self.excl.approx_advance(doc);
        }
        e == doc && (!self.excl.has_two_phase() || self.excl.matches())
    }
}

impl Scorable for ReqExclScorer {
    fn score(&mut self) -> f32 {
        self.req.score()
    }
    fn set_min_competitive_score(&mut self, m: f32) {
        self.req.set_min_competitive_score(m)
    }
}

impl Scorer for ReqExclScorer {
    fn doc(&self) -> u32 {
        self.req.doc()
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
        self.req.cost()
    }
    fn has_two_phase(&self) -> bool {
        true
    }
    fn approx_next(&mut self) -> u32 {
        self.req.approx_next()
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        self.req.approx_advance(t)
    }
    fn matches(&mut self) -> bool {
        let doc = self.req.doc();
        // Cheap side first: plain exclusion before an expensive required confirmation.
        if !self.req.has_two_phase() {
            return !self.excluded(doc);
        }
        if self.excluded(doc) {
            return false;
        }
        self.req.matches()
    }
    fn match_cost(&self) -> f32 {
        self.req.match_cost() + self.excl.match_cost() + 1.0
    }
    fn advance_shallow(&mut self, t: u32) -> u32 {
        self.req.advance_shallow(t)
    }
    fn max_score(&mut self, upto: u32) -> f32 {
        self.req.max_score(upto)
    }
}

// ------------------------------------------------------------------ required + optional

/// Required docs, scored with the optional scorer's score added when it matches too
/// (Lucene `ReqOptSumScorer`). Under top-scores, blocks where the required score alone
/// can't compete iterate the conjunction instead.
pub struct ReqOptScorer {
    req: Box<dyn Scorer>,
    opt: Box<dyn Scorer>,
    top_scores: bool,
    min_competitive: f32,
    up_to: u32,
    req_max: f32,
    opt_required: bool,
}

impl ReqOptScorer {
    pub fn new(req: Box<dyn Scorer>, opt: Box<dyn Scorer>, top_scores: bool) -> ReqOptScorer {
        ReqOptScorer { req, opt, top_scores, min_competitive: 0.0, up_to: u32::MAX, req_max: f32::INFINITY, opt_required: false }
    }

    fn opt_on(&mut self, doc: u32) -> bool {
        let mut d = self.opt.doc();
        if d == u32::MAX || d < doc {
            d = self.opt.advance(doc);
        }
        d == doc
    }

    fn update_block(&mut self, target: u32) {
        if !self.top_scores || self.min_competitive <= 0.0 {
            self.opt_required = false;
            return;
        }
        if self.up_to != u32::MAX && target <= self.up_to {
            return;
        }
        let up = self.req.advance_shallow(target).min(self.opt.advance_shallow(target));
        self.up_to = up;
        self.req_max = self.req.max_score(up);
        self.opt_required = self.req_max < self.min_competitive;
    }
}

impl Scorable for ReqOptScorer {
    fn score(&mut self) -> f32 {
        let doc = self.req.doc();
        let mut s = self.req.score() as f64;
        if self.opt_on(doc) {
            s += self.opt.score() as f64;
        }
        s as f32
    }
    fn set_min_competitive_score(&mut self, m: f32) {
        if self.top_scores && m > self.min_competitive {
            self.min_competitive = m;
            self.up_to = u32::MAX;
        }
    }
}

impl Scorer for ReqOptScorer {
    fn doc(&self) -> u32 {
        self.req.doc()
    }
    fn next(&mut self) -> u32 {
        let d = self.req.doc();
        let t = if d == u32::MAX { 0 } else { d + 1 };
        self.advance(t)
    }
    fn advance(&mut self, mut t: u32) -> u32 {
        loop {
            self.update_block(t);
            let d = self.req.advance(t);
            if d == NO_MORE_DOCS {
                return d;
            }
            if self.opt_required && d > self.up_to {
                // The required clause left the block the decision was made for: in its new
                // block it may compete alone (Lucene re-checks the block of the doc it lands on).
                self.update_block(d);
            }
            if !self.opt_required {
                return d;
            }
            if self.opt_on(d) {
                return d;
            }
            let od = self.opt.doc();
            t = if od == NO_MORE_DOCS {
                // Optional exhausted: only blocks where the required side alone competes.
                self.up_to.saturating_add(1).min(NO_MORE_DOCS)
            } else {
                // Past this block the required side may compete alone again: stop at its end
                // and decide anew.
                od.max(d + 1).min(self.up_to.saturating_add(1))
            };
            if t >= NO_MORE_DOCS {
                return self.req.advance(NO_MORE_DOCS);
            }
        }
    }
    fn cost(&self) -> u64 {
        self.req.cost()
    }
    fn has_two_phase(&self) -> bool {
        false
    }
    fn advance_shallow(&mut self, t: u32) -> u32 {
        self.req.advance_shallow(t).min(self.opt.advance_shallow(t))
    }
    fn max_score(&mut self, upto: u32) -> f32 {
        sum_upper_bound(self.req.max_score(upto) as f64 + self.opt.max_score(upto) as f64, 2) as f32
    }
}

// ------------------------------------------------------------------ block-max conjunction

/// Conjunction that skips blocks whose summed score bounds can't compete (top-k `must`).
pub struct BlockMaxConjunction {
    inner: ConjunctionScorer,
    min_competitive: f32,
    up_to: u32,
    block_max: f32,
}

impl BlockMaxConjunction {
    pub fn new(subs: Vec<Box<dyn Scorer>>) -> BlockMaxConjunction {
        let inner = ConjunctionScorer::new(subs.into_iter().map(|s| (s, true)).collect());
        BlockMaxConjunction { inner, min_competitive: 0.0, up_to: u32::MAX, block_max: f32::INFINITY }
    }

    fn competitive(&mut self, mut t: u32) -> u32 {
        if self.min_competitive <= 0.0 {
            return t;
        }
        loop {
            if self.up_to == u32::MAX || t > self.up_to {
                let up = self.inner.advance_shallow(t);
                self.up_to = up;
                self.block_max = bound(self.inner.max_score(up));
            }
            if self.block_max >= self.min_competitive {
                return t;
            }
            if self.up_to >= NO_MORE_DOCS - 1 {
                return NO_MORE_DOCS;
            }
            t = self.up_to + 1;
        }
    }
}

impl Scorable for BlockMaxConjunction {
    fn score(&mut self) -> f32 {
        self.inner.score()
    }
    fn set_min_competitive_score(&mut self, m: f32) {
        if m > self.min_competitive {
            self.min_competitive = m;
        }
    }
}

impl Scorer for BlockMaxConjunction {
    fn doc(&self) -> u32 {
        self.inner.doc()
    }
    fn next(&mut self) -> u32 {
        let d = self.inner.doc();
        let t = if d == u32::MAX { 0 } else { d + 1 };
        self.advance(t)
    }
    fn advance(&mut self, t: u32) -> u32 {
        let mut t = t;
        loop {
            let c = self.competitive(t);
            if c >= NO_MORE_DOCS {
                return self.inner.advance(NO_MORE_DOCS);
            }
            let cur = self.inner.doc();
            // Never re-advance the conjunction to the doc it is on (two-phase subs would
            // confirm it again).
            let d = if cur != u32::MAX && c <= cur { cur } else { self.inner.advance(c) };
            if d == NO_MORE_DOCS || self.min_competitive <= 0.0 || d <= self.up_to {
                return d;
            }
            t = d;
        }
    }
    fn cost(&self) -> u64 {
        self.inner.cost()
    }
    fn advance_shallow(&mut self, t: u32) -> u32 {
        self.inner.advance_shallow(t)
    }
    fn max_score(&mut self, upto: u32) -> f32 {
        self.inner.max_score(upto)
    }
}

// ------------------------------------------------------------------ bulk: block-max conjunction

/// Lucene's `ScorerUtil.minRequiredScore`: the lowest partial score a doc needs when the other
/// clauses add at most `max_remaining`, accounting for float summation error.
fn min_required_score(max_remaining: f64, min_competitive: f32, n: usize) -> f64 {
    let mut r = min_competitive as f64 - max_remaining;
    // The float ulp converges faster than the double one.
    let sub = ulp_f32(min_competitive) as f64;
    while r > 0.0 && (sum_upper_bound(r + max_remaining, n) as f32) >= min_competitive {
        r -= sub;
    }
    r
}

fn ulp_f32(v: f32) -> f32 {
    let a = v.abs();
    if !a.is_finite() {
        return a;
    }
    f32::from_bits(a.to_bits() + 1) - a
}

/// Top-k conjunction of scoring clauses without two-phase iteration, score-first (Lucene's
/// `BlockMaxConjunctionBulkScorer`): windows follow the lead's impact blocks and are skipped
/// when the clauses' summed bounds can't compete; inside a window the lead scores a postings
/// block at once, docs whose partial score plus the remaining clauses' bounds can't compete
/// are dropped before the next clause is even advanced, and only survivors reach the
/// collector. Filters take part as clauses scoring 0.
pub struct BlockMaxConjunctionBulk {
    /// Sorted by cost: `scorers[0]` leads.
    scorers: Vec<Box<dyn Scorer>>,
    /// `sum_of_others[i]`: summed bounds of clauses `i..` in the current window.
    sum_of_others: Vec<f64>,
    max_doc: u32,
    scorable: BulkScorable,
    buf: Box<DocScoreBuf>,
    acc_docs: Box<[u32; BATCH]>,
    acc_scores: Box<[f64; BATCH]>,
}

const MAX_CONJ_WINDOW: u32 = 65536;

#[inline]
fn positioned(d: u32) -> bool {
    d != u32::MAX
}

impl BlockMaxConjunctionBulk {
    pub fn new(max_doc: u32, mut scorers: Vec<Box<dyn Scorer>>) -> BlockMaxConjunctionBulk {
        assert!(scorers.len() > 1);
        scorers.sort_by_key(|s| s.cost());
        let n = scorers.len();
        BlockMaxConjunctionBulk {
            scorers,
            sum_of_others: vec![f64::INFINITY; n],
            max_doc,
            scorable: BulkScorable::default(),
            buf: DocScoreBuf::new(),
            acc_docs: Box::new([0; BATCH]),
            acc_scores: Box::new([0.0; BATCH]),
        }
    }

    fn compute_max_score(&mut self, wmin: u32, wmax: u32) -> f32 {
        for s in self.scorers.iter_mut() {
            s.advance_shallow(wmin);
        }
        let mut total = 0f64;
        for (i, s) in self.scorers.iter_mut().enumerate() {
            let m = bound(s.max_score(wmax));
            self.sum_of_others[i] = m as f64;
            total += m as f64;
        }
        for i in (0..self.sum_of_others.len() - 1).rev() {
            self.sum_of_others[i] += self.sum_of_others[i + 1];
        }
        total as f32
    }

    fn score_window(&mut self, c: &mut dyn LeafCollector, live: Option<&LiveDocs>, min: u32, max: u32, window_max: f32) -> Result<()> {
        if window_max < self.scorable.min_competitive {
            return Ok(());
        }
        let n = self.scorers.len();
        let d = self.scorers[0].doc();
        if !positioned(d) || d < min {
            self.scorers[0].advance(min);
        }
        if self.scorers[0].doc() >= max {
            return Ok(());
        }
        loop {
            self.scorers[0].next_docs_and_scores(max, live, &mut self.buf);
            let mut len = self.buf.len;
            if len == 0 {
                break;
            }
            for i in 0..len {
                self.acc_docs[i] = self.buf.docs[i];
                self.acc_scores[i] = self.buf.scores[i] as f64;
            }
            for i in 1..n {
                let soc = self.sum_of_others[i];
                if soc != self.sum_of_others[i - 1] {
                    // Equal consecutive bounds mean the previous clause scores 0: no new filter.
                    let req = min_required_score(soc, self.scorable.min_competitive, n);
                    if req > 0.0 {
                        let mut k = 0;
                        for j in 0..len {
                            if self.acc_scores[j] >= req {
                                self.acc_docs[k] = self.acc_docs[j];
                                self.acc_scores[k] = self.acc_scores[j];
                                k += 1;
                            }
                        }
                        len = k;
                    }
                }
                len = self.scorers[i].apply_required(&mut self.acc_docs[..], &mut self.acc_scores[..], len);
            }
            for j in 0..len {
                self.scorable.score = self.acc_scores[j] as f32;
                c.collect(self.acc_docs[j], &mut self.scorable)?;
            }
        }
        let mut max_other = 0u32;
        for s in &self.scorers[1..] {
            let d = s.doc();
            if positioned(d) {
                max_other = max_other.max(d);
            }
        }
        let lead = &mut self.scorers[0];
        if lead.doc() < max_other {
            lead.advance(max_other);
        }
        Ok(())
    }
}

impl BulkScorer for BlockMaxConjunctionBulk {
    fn score(&mut self, c: &mut dyn LeafCollector, live: Option<&LiveDocs>, min: u32, max: u32) -> Result<u32> {
        let lead_doc = self.scorers[0].doc();
        let mut wmin = if positioned(lead_doc) { lead_doc.max(min) } else { min };
        // Windows past the last doc would step through the doc id space 64k at a time when
        // the lead's last block was skipped (it is not advanced then).
        let max = max.min(self.max_doc);
        while wmin < max {
            let mut wmax = self.scorers[0].advance_shallow(wmin).min(max - 1);
            wmax = wmax.min(wmin.saturating_add(MAX_CONJ_WINDOW));
            // Bounds only matter once the collector has a minimum score (which never goes back
            // to 0); until then `sum_of_others` stays infinite and filters nothing.
            let m = if self.scorable.min_competitive > 0.0 { self.compute_max_score(wmin, wmax) } else { f32::INFINITY };
            self.score_window(c, live, wmin, wmax + 1, m)?;
            let d = self.scorers[0].doc();
            wmin = (wmax + 1).max(if positioned(d) { d } else { 0 });
        }
        Ok(if wmin >= self.max_doc { NO_MORE_DOCS } else { wmin })
    }

    fn cost(&self) -> u64 {
        self.scorers[0].cost()
    }
}

// ------------------------------------------------------------------ bulk: exhaustive conjunctions

/// Pure conjunctions visiting every match (`Complete`/`CompleteNoScores`, no two-phase clause;
/// Lucene's `ConjunctionBulkScorer`).
///
/// Bounded: only clause positions are left past a window's end, never a confirmed match, so
/// windows and intra-segment partitions don't scan into the next one (iterating
/// [`ConjunctionScorer`] would find the next match past the end first).
///
/// Without a competitive iterator the lead hands out a postings block of docs (with scores) at
/// a time and every other clause filters the batch in turn ([`Scorer::apply_required`],
/// [`Scorer::apply_filter`]): term clauses advance their cursors without a virtual call per doc
/// and score only the survivors, a block at once. With one (sorted collectors), docs leapfrog
/// a doc at a time so non-competitive ranges are jumped.
pub struct ConjunctionBulk {
    sc: ConjScorable,
    buf: Box<DocScoreBuf>,
    acc_docs: Box<[u32; BATCH]>,
    acc_scores: Box<[f64; BATCH]>,
    scorable: BulkScorable,
}

/// The clauses of a [`ConjunctionBulk`], sorted by cost (`subs[0]` leads), as the scorable
/// of doc-at-a-time collection.
struct ConjScorable {
    subs: Vec<Box<dyn Scorer>>,
    scoring: Vec<bool>,
}

impl Scorable for ConjScorable {
    fn score(&mut self) -> f32 {
        let mut s = 0f64;
        for (sub, &scoring) in self.subs.iter_mut().zip(&self.scoring) {
            if scoring {
                s += sub.score() as f64;
            }
        }
        s as f32
    }
}

impl ConjunctionBulk {
    /// `subs` (two or more, none two-phase) with flags telling which contribute to the score.
    pub fn new(subs: Vec<(Box<dyn Scorer>, bool)>) -> ConjunctionBulk {
        assert!(subs.len() > 1);
        debug_assert!(subs.iter().all(|(s, _)| !s.has_two_phase()));
        let mut subs = subs;
        subs.sort_by_key(|(s, _)| s.cost());
        let (subs, scoring) = subs.into_iter().unzip();
        ConjunctionBulk {
            sc: ConjScorable { subs, scoring },
            buf: DocScoreBuf::new(),
            acc_docs: Box::new([0; BATCH]),
            acc_scores: Box::new([0.0; BATCH]),
            scorable: BulkScorable::default(),
        }
    }

    /// Batches from the lead's postings blocks, filtered by the other clauses.
    fn score_batches(&mut self, c: &mut dyn LeafCollector, live: Option<&LiveDocs>, max: u32) -> Result<()> {
        let n = self.sc.subs.len();
        while self.sc.subs[0].doc() < max {
            self.sc.subs[0].next_docs_and_scores(max, live, &mut self.buf);
            let mut len = self.buf.len;
            if len == 0 {
                break;
            }
            self.acc_docs[..len].copy_from_slice(&self.buf.docs[..len]);
            if self.sc.scoring[0] {
                for (a, &s) in self.acc_scores[..len].iter_mut().zip(&self.buf.scores[..len]) {
                    *a = s as f64;
                }
            } else {
                self.acc_scores[..len].fill(0.0);
            }
            for i in 1..n {
                let s = &mut self.sc.subs[i];
                len = if self.sc.scoring[i] {
                    s.apply_required(&mut self.acc_docs[..], &mut self.acc_scores[..], len)
                } else {
                    s.apply_filter(&mut self.acc_docs[..], &mut self.acc_scores[..], len)
                };
                if len == 0 {
                    break;
                }
            }
            for j in 0..len {
                self.scorable.score = self.acc_scores[j] as f32;
                c.collect(self.acc_docs[j], &mut self.scorable)?;
            }
        }
        Ok(())
    }

    /// Leapfrogs a doc at a time, jumping to the collector's next competitive doc.
    fn score_docs(&mut self, c: &mut dyn LeafCollector, live: Option<&LiveDocs>, max: u32) -> Result<()> {
        let n = self.sc.subs.len();
        let mut doc = self.sc.subs[0].doc();
        'head: while doc < max {
            let t = c.competitive_from(doc);
            if t > doc {
                doc = self.sc.subs[0].advance(t);
                continue;
            }
            if live.is_some_and(|l| !l.is_live(doc)) {
                doc = self.sc.subs[0].next();
                continue;
            }
            for i in 1..n {
                let s = &mut self.sc.subs[i];
                let mut d = s.doc();
                if d == u32::MAX || d < doc {
                    d = s.advance(doc);
                }
                if d != doc {
                    doc = self.sc.subs[0].advance(d);
                    continue 'head;
                }
            }
            c.collect(doc, &mut self.sc)?;
            doc = self.sc.subs[0].next();
        }
        Ok(())
    }
}

impl BulkScorer for ConjunctionBulk {
    fn score(&mut self, c: &mut dyn LeafCollector, live: Option<&LiveDocs>, min: u32, max: u32) -> Result<u32> {
        let lead = &mut self.sc.subs[0];
        let d = lead.doc();
        if d == u32::MAX || d < min {
            lead.advance(min);
        }
        if c.skips() {
            self.score_docs(c, live, max)?;
        } else {
            self.score_batches(c, live, max)?;
        }
        Ok(self.sc.subs[0].doc())
    }

    fn cost(&self) -> u64 {
        self.sc.subs[0].cost()
    }
}

// ------------------------------------------------------------------ bulk: MaxScore

const WINDOW: u32 = 1 << 12;

/// The scorable a bulk scorer hands collectors: carries the current score and receives the
/// minimum competitive score.
#[derive(Default)]
pub struct BulkScorable {
    pub score: f32,
    pub min_competitive: f32,
}

impl Scorable for BulkScorable {
    fn score(&mut self) -> f32 {
        self.score
    }
    fn set_min_competitive_score(&mut self, m: f32) {
        if m > self.min_competitive {
            self.min_competitive = m;
        }
    }
}

struct MsClause {
    s: Box<dyn Scorer>,
    doc: u32,
    cost: u64,
    max_window: f32,
}

/// Top-k disjunction (optionally filtered) by block-max MaxScore (Lucene's
/// `MaxScoreBulkScorer`): clauses whose summed block bounds can't reach the competitive score
/// are "non-essential" and only consulted for candidates the essential ones produce.
pub struct MaxScoreBulk {
    max_doc: u32,
    clauses: Vec<MsClause>,
    /// Indices into `clauses`, partitioned: `order[..first_essential]` non-essential (by
    /// increasing bound), the rest essential.
    order: Vec<usize>,
    first_essential: usize,
    /// Non-essential clauses `order[first_required..first_essential]` must match (with a
    /// single essential clause, when the others can't compete without them).
    first_required: usize,
    /// Prefix sums of non-essential bounds.
    max_sums: Vec<f64>,
    next_min: f32,
    filter: Option<Box<dyn Scorer>>,
    scorable: BulkScorable,
    win_scores: Vec<f64>,
    win_bits: FixedBitSet,
    cand_docs: Vec<u32>,
    cand_scores: Vec<f64>,
    batch: Box<DocScoreBuf>,
    cost: u64,
    num_windows: u64,
    num_candidates: u64,
    min_window: u32,
}

impl MaxScoreBulk {
    pub fn new(max_doc: u32, scorers: Vec<Box<dyn Scorer>>, filter: Option<Box<dyn Scorer>>) -> MaxScoreBulk {
        let cost = scorers.iter().fold(0u64, |a, s| a.saturating_add(s.cost()));
        let clauses: Vec<MsClause> =
            scorers.into_iter().map(|s| MsClause { cost: s.cost(), s, doc: u32::MAX, max_window: 0.0 }).collect();
        let n = clauses.len();
        MaxScoreBulk {
            max_doc,
            order: (0..n).collect(),
            first_essential: 0,
            first_required: n,
            max_sums: vec![0.0; n],
            next_min: f32::INFINITY,
            filter,
            scorable: BulkScorable::default(),
            win_scores: vec![0.0; WINDOW as usize],
            win_bits: FixedBitSet::new(WINDOW),
            cand_docs: Vec::with_capacity(WINDOW as usize),
            cand_scores: Vec::with_capacity(WINDOW as usize),
            batch: DocScoreBuf::new(),
            cost,
            num_windows: 0,
            num_candidates: 0,
            min_window: 1,
            clauses,
        }
    }

    fn outer_window_max(&mut self, min: u32) -> u32 {
        let n = self.order.len();
        let first = self.first_essential.min(n - 1);
        let mut wmax = NO_MORE_DOCS;
        let fcost = self.filter.as_ref().map(|f| f.cost());
        for k in first..n {
            let c = &mut self.clauses[self.order[k]];
            if fcost.is_none_or(|fc| c.cost >= fc) {
                let d = if c.doc == u32::MAX { min } else { c.doc.max(min) };
                let up = c.s.advance_shallow(d);
                wmax = wmax.min(up.saturating_add(1));
            }
        }
        if n - first > 1 {
            let threshold = self.num_windows * 32 * n as u64;
            if self.num_candidates < threshold {
                self.min_window = (self.min_window << 1).min(WINDOW);
            } else {
                self.min_window = 1;
            }
            wmax = wmax.max(min.saturating_add(self.min_window).min(i32::MAX as u32));
        }
        wmax
    }

    fn update_max_window_scores(&mut self, min: u32, max: u32) {
        for c in &mut self.clauses {
            if c.doc == u32::MAX || c.doc < max {
                if c.doc == u32::MAX || c.doc < min {
                    c.s.advance_shallow(min);
                }
                c.max_window = bound(c.s.max_score(max - 1));
            } else {
                c.max_window = 0.0;
            }
        }
    }

    /// Splits clauses into non-essential/essential; false when none is essential (no doc in
    /// the window can compete).
    fn partition(&mut self) -> bool {
        let clauses = &self.clauses;
        let mut sorted: Vec<usize> = (0..clauses.len()).collect();
        sorted.sort_by(|&a, &b| {
            let x = clauses[a].max_window as f64 / clauses[a].cost.max(1) as f64;
            let y = clauses[b].max_window as f64 / clauses[b].cost.max(1) as f64;
            x.total_cmp(&y)
        });
        let n = sorted.len();
        let mut sum = 0f64;
        self.first_essential = 0;
        self.next_min = f32::INFINITY;
        let mut non = Vec::with_capacity(n);
        let mut ess = Vec::with_capacity(n);
        for &i in &sorted {
            let ns = sum + self.clauses[i].max_window as f64;
            let bound = sum_upper_bound(ns, non.len() + 1) as f32;
            if bound < self.scorable.min_competitive {
                sum = ns;
                self.max_sums[non.len()] = sum;
                non.push(i);
            } else {
                ess.push(i);
                self.next_min = self.next_min.min(bound);
            }
        }
        self.first_essential = non.len();
        self.order.clear();
        self.order.extend(non);
        self.order.extend(ess);
        self.first_required = n;
        if self.first_essential + 1 == n {
            // Lucene: with one essential clause, if it plus all non-essential clauses but the
            // best can't compete, hits must also match the best one (e.g. `quick fox` with
            // max scores 1 and 1 and a minimum of 1.5), and so on down.
            let mut fr = n - 1;
            let mut max_required = self.clauses[self.order[self.first_essential]].max_window as f64;
            while fr > 0 {
                let mut without_prev = max_required;
                if fr > 1 {
                    without_prev += self.max_sums[fr - 2];
                }
                if without_prev as f32 >= self.scorable.min_competitive {
                    break;
                }
                fr -= 1;
                max_required += self.clauses[self.order[fr]].max_window as f64;
            }
            self.first_required = fr;
        }
        self.first_essential < n
    }

    fn essential_top(&self) -> u32 {
        let mut m = NO_MORE_DOCS;
        for &i in &self.order[self.first_essential..] {
            let d = self.clauses[i].doc;
            m = m.min(if d == u32::MAX { 0 } else { d });
        }
        m
    }

    /// Fills candidates for the inner window starting at the first essential doc.
    fn fill_window(&mut self, live: Option<&LiveDocs>, max: u32) {
        self.cand_docs.clear();
        self.cand_scores.clear();
        let top = self.essential_top();
        if top >= max {
            return;
        }
        let wmin = top;
        let wmax = max.min(wmin.saturating_add(WINDOW));
        let single = self.first_essential == self.order.len() - 1;
        let fe = self.first_essential;
        if single && self.filter.is_none() {
            let c = &mut self.clauses[self.order[fe]];
            if c.doc == u32::MAX {
                c.doc = c.s.advance(wmin);
            }
            loop {
                c.s.next_docs_and_scores(wmax, live, &mut self.batch);
                if self.batch.len == 0 {
                    break;
                }
                self.cand_docs.extend_from_slice(&self.batch.docs[..self.batch.len]);
                self.cand_scores.extend(self.batch.scores[..self.batch.len].iter().map(|&x| x as f64));
            }
            c.doc = c.s.doc();
            return;
        }
        // Several essential clauses (or a filter): accumulate into the window.
        for k in fe..self.order.len() {
            let c = &mut self.clauses[self.order[k]];
            let mut d = if c.doc == u32::MAX || c.doc < wmin { c.s.advance(wmin) } else { c.doc };
            while d < wmax {
                let o = d - wmin;
                self.win_bits.set(o);
                self.win_scores[o as usize] += c.s.score() as f64;
                d = c.s.next();
            }
            c.doc = d;
        }
        let words = self.win_bits.words_mut();
        let nwords = ((wmax - wmin) as usize).div_ceil(64);
        for (wi, w) in words.iter_mut().enumerate().take(nwords) {
            let mut bits = *w;
            *w = 0;
            while bits != 0 {
                let t = bits.trailing_zeros();
                bits &= bits - 1;
                let o = wi as u32 * 64 + t;
                let d = wmin + o;
                let sc = std::mem::take(&mut self.win_scores[o as usize]);
                if live.is_some_and(|l| !l.is_live(d)) {
                    continue;
                }
                self.cand_docs.push(d);
                self.cand_scores.push(sc);
            }
        }
        if let Some(f) = self.filter.as_mut() {
            let mut j = 0;
            for i in 0..self.cand_docs.len() {
                let d = self.cand_docs[i];
                let mut fd = f.doc();
                if fd == u32::MAX || fd < d {
                    fd = f.advance(d);
                }
                if fd == d {
                    self.cand_docs[j] = d;
                    self.cand_scores[j] = self.cand_scores[i];
                    j += 1;
                }
            }
            self.cand_docs.truncate(j);
            self.cand_scores.truncate(j);
        }
    }

    fn score_non_essential(&mut self, c: &mut dyn LeafCollector) -> Result<()> {
        self.num_candidates += self.cand_docs.len() as u64;
        let n = self.clauses.len();
        for k in (0..self.first_essential).rev() {
            let bound_rest = self.max_sums[k];
            let minc = self.scorable.min_competitive;
            let required = k >= self.first_required;
            let cl = &mut self.clauses[self.order[k]];
            let mut j = 0;
            for i in 0..self.cand_docs.len() {
                let d = self.cand_docs[i];
                let mut sc = self.cand_scores[i];
                if (sum_upper_bound(sc + bound_rest, n) as f32) < minc {
                    continue;
                }
                let mut cd = cl.doc;
                if cd == u32::MAX || cd < d {
                    cd = cl.s.advance(d);
                    cl.doc = cd;
                }
                if cd == d {
                    sc += cl.s.score() as f64;
                } else if required {
                    continue;
                }
                self.cand_docs[j] = d;
                self.cand_scores[j] = sc;
                j += 1;
            }
            self.cand_docs.truncate(j);
            self.cand_scores.truncate(j);
        }
        for i in 0..self.cand_docs.len() {
            self.scorable.score = self.cand_scores[i] as f32;
            c.collect(self.cand_docs[i], &mut self.scorable)?;
        }
        Ok(())
    }
}

impl BulkScorer for MaxScoreBulk {
    fn score(&mut self, c: &mut dyn LeafCollector, live: Option<&LiveDocs>, min: u32, max: u32) -> Result<u32> {
        let max = max.min(self.max_doc);
        let mut wmin = min;
        'outer: while wmin < max {
            let mut wmax = self.outer_window_max(wmin).min(max);
            loop {
                self.update_max_window_scores(wmin, wmax);
                if !self.partition() {
                    wmin = wmax;
                    continue 'outer;
                }
                let nw = self.outer_window_max(wmin);
                if nw >= wmax {
                    break;
                }
                wmax = nw;
            }
            // Position essential clauses at the window start.
            for k in self.first_essential..self.order.len() {
                let cl = &mut self.clauses[self.order[k]];
                if cl.doc == u32::MAX || cl.doc < wmin {
                    cl.doc = cl.s.advance(wmin);
                }
            }
            if let Some(f) = self.filter.as_mut()
                && (f.doc() == u32::MAX || f.doc() < wmin)
            {
                f.advance(wmin);
            }
            while self.essential_top() < wmax {
                self.fill_window(live, wmax);
                self.score_non_essential(c)?;
                if self.scorable.min_competitive >= self.next_min {
                    break;
                }
            }
            wmin = self.essential_top().min(wmax);
            self.num_windows += 1;
        }
        if max >= self.max_doc {
            return Ok(NO_MORE_DOCS);
        }
        let mut next = NO_MORE_DOCS;
        for cl in &self.clauses {
            let d = if cl.doc == u32::MAX { 0 } else { cl.doc };
            if d < max {
                return Ok(max);
            }
            next = next.min(d);
        }
        Ok(next)
    }

    fn cost(&self) -> u64 {
        self.cost
    }
}

// ------------------------------------------------------------------ bulk: windowed disjunction

/// Exhaustive disjunctions (counting, full scoring, `msm`): each sub-scorer fills a window of
/// 4096 docs (match bit, match count, score sum) before docs are collected in order — no heap
/// per doc (Lucene's `BooleanScorer`).
pub struct WindowedDisjunctionBulk {
    subs: Vec<Box<dyn Scorer>>,
    docs: Vec<u32>,
    msm: u32,
    needs_scores: bool,
    bits: FixedBitSet,
    counts: Vec<u32>,
    scores: Vec<f64>,
    max_doc: u32,
    cost: u64,
    scorable: BulkScorable,
}

impl WindowedDisjunctionBulk {
    pub fn new(subs: Vec<Box<dyn Scorer>>, msm: usize, needs_scores: bool, max_doc: u32) -> WindowedDisjunctionBulk {
        let n = subs.len();
        let cost = subs.iter().fold(0u64, |a, s| a.saturating_add(s.cost()));
        WindowedDisjunctionBulk {
            subs,
            docs: vec![u32::MAX; n],
            msm: msm.max(1) as u32,
            needs_scores,
            bits: FixedBitSet::new(WINDOW),
            counts: vec![0; WINDOW as usize],
            scores: vec![0.0; WINDOW as usize],
            max_doc,
            cost,
            scorable: BulkScorable::default(),
        }
    }
}

impl BulkScorer for WindowedDisjunctionBulk {
    fn score(&mut self, c: &mut dyn LeafCollector, live: Option<&LiveDocs>, min: u32, max: u32) -> Result<u32> {
        let end = max.min(self.max_doc);
        let mut wmin = min;
        while wmin < end {
            // Skip empty space: the next window starts at the smallest sub doc.
            let mut first = NO_MORE_DOCS;
            for (i, s) in self.subs.iter_mut().enumerate() {
                if self.docs[i] == u32::MAX || self.docs[i] < wmin {
                    self.docs[i] = s.advance(wmin);
                }
                first = first.min(self.docs[i]);
            }
            if first >= end {
                break;
            }
            let ws = first;
            let we = end.min(ws.saturating_add(WINDOW));
            let msm1 = self.msm == 1;
            for (i, s) in self.subs.iter_mut().enumerate() {
                let mut d = self.docs[i];
                while d < we {
                    let o = d - ws;
                    self.bits.set(o);
                    if !msm1 {
                        self.counts[o as usize] += 1;
                    }
                    if self.needs_scores {
                        self.scores[o as usize] += s.score() as f64;
                    }
                    d = s.next();
                }
                self.docs[i] = d;
            }
            let nwords = ((we - ws) as usize).div_ceil(64);
            for wi in 0..nwords {
                let mut bits = self.bits.words()[wi];
                if bits == 0 {
                    continue;
                }
                self.bits.words_mut()[wi] = 0;
                while bits != 0 {
                    let t = bits.trailing_zeros();
                    bits &= bits - 1;
                    let o = (wi as u32 * 64 + t) as usize;
                    let d = ws + o as u32;
                    let cnt = if msm1 { 1 } else { std::mem::take(&mut self.counts[o]) };
                    let sc = if self.needs_scores { std::mem::take(&mut self.scores[o]) } else { 0.0 };
                    if cnt < self.msm || live.is_some_and(|l| !l.is_live(d)) {
                        continue;
                    }
                    self.scorable.score = sc as f32;
                    c.collect(d, &mut self.scorable)?;
                }
            }
            wmin = we;
        }
        if end >= self.max_doc {
            return Ok(NO_MORE_DOCS);
        }
        let mut next = NO_MORE_DOCS;
        for &d in &self.docs {
            next = next.min(if d == u32::MAX { end } else { d });
        }
        Ok(next.max(end))
    }

    fn cost(&self) -> u64 {
        self.cost
    }
}

// ------------------------------------------------------------------ bulk: exclusion windows

/// A bulk scorer minus the docs of an exclusion scorer: scores the gaps between excluded
/// docs (Lucene's `ReqExclBulkScorer`).
pub struct ReqExclBulk {
    pub req: Box<dyn BulkScorer>,
    pub excl: Box<dyn Scorer>,
}

impl BulkScorer for ReqExclBulk {
    fn score(&mut self, c: &mut dyn LeafCollector, live: Option<&LiveDocs>, min: u32, max: u32) -> Result<u32> {
        let mut upto = min;
        let mut ex = self.excl.doc();
        while upto < max {
            if ex == u32::MAX || ex < upto {
                ex = self.excl.advance(upto);
            }
            if ex == upto {
                upto += 1;
                ex = self.excl.next();
                continue;
            }
            let wend = ex.min(max);
            upto = self.req.score(c, live, upto, wend)?;
        }
        Ok(upto)
    }
    fn cost(&self) -> u64 {
        self.req.cost()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Docs with a constant score; impact blocks of `block` docs; counts `advance_shallow` calls.
    struct ListScorer {
        docs: Vec<u32>,
        i: usize,
        doc: u32,
        score: f32,
        block: usize,
        shallow_calls: Arc<AtomicUsize>,
    }

    impl Scorable for ListScorer {
        fn score(&mut self) -> f32 {
            self.score
        }
    }

    impl Scorer for ListScorer {
        fn doc(&self) -> u32 {
            self.doc
        }
        fn next(&mut self) -> u32 {
            let t = if self.doc == u32::MAX { 0 } else { self.doc + 1 };
            self.advance(t)
        }
        fn advance(&mut self, t: u32) -> u32 {
            while self.i < self.docs.len() && self.docs[self.i] < t {
                self.i += 1;
            }
            self.doc = self.docs.get(self.i).copied().unwrap_or(NO_MORE_DOCS);
            self.doc
        }
        fn cost(&self) -> u64 {
            self.docs.len() as u64
        }
        fn advance_shallow(&mut self, t: u32) -> u32 {
            self.shallow_calls.fetch_add(1, Ordering::Relaxed);
            let i = self.docs.partition_point(|&d| d < t);
            // The last region is open-ended, as a postings tail.
            let end = (i / self.block + 1) * self.block;
            if end >= self.docs.len() { NO_MORE_DOCS } else { self.docs[end - 1] }
        }
        fn max_score(&mut self, _upto: u32) -> f32 {
            self.score
        }
    }

    /// Takes the first `k` hits, then asks for scores above any clause can produce.
    struct Greedy {
        seen: Vec<(u32, f32)>,
        k: usize,
    }

    impl LeafCollector for Greedy {
        fn collect(&mut self, doc: u32, s: &mut dyn Scorable) -> Result<()> {
            self.seen.push((doc, s.score()));
            if self.seen.len() == self.k {
                s.set_min_competitive_score(100.0);
            }
            Ok(())
        }
    }

    fn list(docs: Vec<u32>, score: f32, calls: &Arc<AtomicUsize>) -> Box<dyn Scorer> {
        Box::new(ListScorer { docs, i: 0, doc: u32::MAX, score, block: 4, shallow_calls: calls.clone() })
    }

    #[test]
    fn conjunction_windows_stop_at_max_doc() {
        // Once nothing can compete every window is skipped without moving the lead; windows
        // must still end at max_doc rather than walk the doc id space 64k at a time.
        let calls = Arc::new(AtomicUsize::new(0));
        let a = list((0..40).map(|d| d * 3).collect(), 1.0, &calls);
        let b = list((0..120).collect(), 0.5, &calls);
        let mut bulk = BlockMaxConjunctionBulk::new(120, vec![a, b]);
        let mut c = Greedy { seen: Vec::new(), k: 3 };
        let next = bulk.score(&mut c, None, 0, NO_MORE_DOCS).unwrap();
        assert_eq!(next, NO_MORE_DOCS);
        // The rest of the batch in hand is still collected.
        assert_eq!(c.seen[..3], [(0, 1.5), (3, 1.5), (6, 1.5)]);
        assert!(c.seen.len() < 40);
        assert!(calls.load(Ordering::Relaxed) < 200, "{} advance_shallow calls", calls.load(Ordering::Relaxed));
    }

    #[test]
    fn conjunction_batches_match_doc_at_a_time() {
        let calls = Arc::new(AtomicUsize::new(0));
        let a: Vec<u32> = (0..500).filter(|d| d % 3 == 0 || d % 7 == 0).collect();
        let b: Vec<u32> = (0..500).filter(|d| d % 2 == 0).collect();
        let c3: Vec<u32> = (0..500).filter(|d| d % 5 != 1).collect();
        let want: Vec<(u32, f32)> =
            (0..500).filter(|d| a.contains(d) && b.contains(d) && c3.contains(d)).map(|d| (d, (1.0f64 + 0.5 + 0.25) as f32)).collect();
        let mut bulk =
            BlockMaxConjunctionBulk::new(500, vec![list(a, 1.0, &calls), list(b, 0.5, &calls), list(c3, 0.25, &calls)]);
        let mut col = Greedy { seen: Vec::new(), k: usize::MAX };
        bulk.score(&mut col, None, 0, NO_MORE_DOCS).unwrap();
        assert_eq!(col.seen, want);
    }

    /// Collects hits, failing on any doc outside the window; with `skip_mod`, claims docs
    /// whose `doc % skip_mod == 0` can't compete (a competitive iterator).
    struct Windowed {
        seen: Vec<(u32, f32)>,
        min: u32,
        max: u32,
        skip_mod: Option<u32>,
    }

    impl LeafCollector for Windowed {
        fn collect(&mut self, doc: u32, s: &mut dyn Scorable) -> Result<()> {
            assert!(doc >= self.min && doc < self.max, "doc {doc} outside [{}, {})", self.min, self.max);
            if let Some(m) = self.skip_mod {
                assert!(!doc.is_multiple_of(m), "skipped doc {doc} collected");
            }
            self.seen.push((doc, s.score()));
            Ok(())
        }
        fn skips(&self) -> bool {
            self.skip_mod.is_some()
        }
        fn competitive_from(&mut self, doc: u32) -> u32 {
            match self.skip_mod {
                Some(m) if doc.is_multiple_of(m) => doc + 1,
                _ => doc,
            }
        }
    }

    #[test]
    fn exhaustive_conjunction_matches_brute_force_in_bounded_windows() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let calls = Arc::new(AtomicUsize::new(0));
        for round in 0..300 {
            let max_doc = 1 + rnd(3000) as u32;
            let n = 2 + rnd(3) as usize;
            let mut lists = Vec::new();
            for _ in 0..n {
                let density = 1 + rnd(20);
                lists.push((0..max_doc).filter(|_| rnd(density) == 0).collect::<Vec<u32>>());
            }
            let weights: Vec<f32> = (0..n).map(|i| [1.0f32, 0.5, 0.25, 2.0, 0.125][i]).collect();
            let scoring: Vec<bool> = (0..n).map(|_| rnd(3) != 0).collect();
            let mut live = LiveDocs::all_live(max_doc);
            if round % 2 == 1 {
                for d in 0..max_doc {
                    if rnd(5) == 0 {
                        live.delete(d);
                    }
                }
            }
            let live = (round % 2 == 1).then_some(&live);
            let skip_mod = (round % 3 == 2).then(|| 2 + rnd(5) as u32);
            let want: Vec<(u32, f32)> = (0..max_doc)
                .filter(|d| lists.iter().all(|l| l.binary_search(d).is_ok()))
                .filter(|&d| live.is_none_or(|l| l.is_live(d)))
                .filter(|&d| skip_mod.is_none_or(|m| !d.is_multiple_of(m)))
                .map(|d| {
                    let s: f64 = (0..n).filter(|&i| scoring[i]).map(|i| weights[i] as f64).sum();
                    (d, s as f32)
                })
                .collect();
            let subs = (0..n).map(|i| (list(lists[i].clone(), weights[i], &calls), scoring[i])).collect();
            let mut bulk = ConjunctionBulk::new(subs);
            let mut got = Vec::new();
            let mut min = 0u32;
            while min < max_doc {
                let max = if rnd(4) == 0 { NO_MORE_DOCS } else { (min + 1 + rnd(400) as u32).min(max_doc) };
                let mut c = Windowed { seen: Vec::new(), min, max, skip_mod };
                let next = bulk.score(&mut c, live, min, max).unwrap();
                assert!(next >= max, "next {next} before the window end {max}");
                got.extend(c.seen);
                // The next window may start past the returned doc (a gap nobody scores).
                min = if max == NO_MORE_DOCS { max_doc } else { max };
            }
            assert_eq!(got, want, "round {round}");
        }
    }

    #[test]
    fn exhaustive_conjunction_zeroes_a_filter_lead() {
        let calls = Arc::new(AtomicUsize::new(0));
        // The filter is the cheapest clause, so it leads; its 3.0 must not reach the score.
        let filter = list(vec![2, 4, 6], 3.0, &calls);
        let scoring = list((0..10).collect(), 0.5, &calls);
        let mut bulk = ConjunctionBulk::new(vec![(filter, false), (scoring, true)]);
        let mut c = Windowed { seen: Vec::new(), min: 0, max: NO_MORE_DOCS, skip_mod: None };
        assert_eq!(bulk.score(&mut c, None, 0, NO_MORE_DOCS).unwrap(), NO_MORE_DOCS);
        assert_eq!(c.seen, [(2, 0.5), (4, 0.5), (6, 0.5)]);
    }

    /// A scorer without score bounds (two-phase queries, scripts): the default `max_score`.
    struct Unbounded(Box<dyn Scorer>);

    impl Scorable for Unbounded {
        fn score(&mut self) -> f32 {
            self.0.score()
        }
    }

    impl Scorer for Unbounded {
        fn doc(&self) -> u32 {
            self.0.doc()
        }
        fn next(&mut self) -> u32 {
            self.0.next()
        }
        fn advance(&mut self, t: u32) -> u32 {
            self.0.advance(t)
        }
        fn cost(&self) -> u64 {
            self.0.cost()
        }
    }

    #[test]
    fn dismax_with_an_unbounded_clause_stays_unbounded() {
        // `inf - inf` once made the dis_max bound NaN; MaxScore then required the other
        // clause and dropped the best hit.
        let calls = Arc::new(AtomicUsize::new(0));
        let mk = || {
            let a: Box<dyn Scorer> = Box::new(Unbounded(list(vec![30, 40, 90], 4.0, &calls)));
            let b = list(vec![2, 40, 60], 0.5, &calls);
            DisjunctionScorer::dismax(vec![a, b], 0.3, true)
        };
        let mut d = mk();
        assert_eq!(d.max_score(100), f32::INFINITY);
        let other = list((0..100).filter(|x| x % 7 == 0).collect(), 1.0, &calls);
        let excl = list(vec![3, 50], 0.0, &calls);
        let bulk = MaxScoreBulk::new(100, vec![Box::new(mk()), other], None);
        let mut b = ReqExclBulk { req: Box::new(bulk), excl };
        let mut col = Bar { seen: Vec::new() };
        b.score(&mut col, None, 0, NO_MORE_DOCS).unwrap();
        // The bar (3) goes above every score the bounded clause can give after the first
        // exclusion window (docs 0 and 2); the unbounded clause must stay essential and
        // optional: its docs 30 and 90 (score 4) don't match the other clause, which the NaN
        // bound made required.
        let docs: Vec<u32> = col.seen.iter().map(|x| x.0).collect();
        assert_eq!(&docs[..2], &[0, 2]);
        assert!(docs.contains(&30) && docs.contains(&90), "{docs:?}");
    }

    struct Bar {
        seen: Vec<(u32, f32)>,
    }

    impl LeafCollector for Bar {
        fn collect(&mut self, doc: u32, s: &mut dyn Scorable) -> Result<()> {
            self.seen.push((doc, s.score()));
            s.set_min_competitive_score(3.0);
            Ok(())
        }
    }
    /// Docs with per-doc scores and per-block score bounds (blocks of `block` docs).
    struct Blocky {
        docs: Vec<u32>,
        scores: Vec<f32>,
        i: usize,
        doc: u32,
        block: usize,
        shallow: usize,
    }

    impl Blocky {
        fn boxed(docs: Vec<u32>, scores: Vec<f32>, block: usize) -> Box<dyn Scorer> {
            Box::new(Blocky { docs, scores, i: 0, doc: u32::MAX, block, shallow: 0 })
        }
    }

    impl Scorable for Blocky {
        fn score(&mut self) -> f32 {
            self.scores[self.i]
        }
    }

    impl Scorer for Blocky {
        fn doc(&self) -> u32 {
            self.doc
        }
        fn next(&mut self) -> u32 {
            let t = if self.doc == u32::MAX { 0 } else { self.doc + 1 };
            self.advance(t)
        }
        fn advance(&mut self, t: u32) -> u32 {
            while self.i < self.docs.len() && self.docs[self.i] < t {
                self.i += 1;
            }
            self.doc = self.docs.get(self.i).copied().unwrap_or(NO_MORE_DOCS);
            self.doc
        }
        fn cost(&self) -> u64 {
            self.docs.len() as u64
        }
        fn advance_shallow(&mut self, t: u32) -> u32 {
            self.shallow = self.docs.partition_point(|&d| d < t);
            let end = (self.shallow / self.block + 1) * self.block;
            if end >= self.docs.len() { NO_MORE_DOCS } else { self.docs[end - 1] }
        }
        fn max_score(&mut self, upto: u32) -> f32 {
            let from = self.shallow / self.block * self.block;
            (from..self.docs.len()).take_while(|&j| self.docs[j] <= upto).map(|j| self.scores[j]).fold(0.0, f32::max)
        }
    }

    #[test]
    fn req_opt_rechecks_blocks_past_the_one_requiring_the_optional_clause() {
        // Block 1 of the required clause can't compete alone (0.5 < 0.8), so the optional
        // clause is required there; its next doc (50) is past block 2, where the required
        // clause competes alone and must not be skipped.
        let req = Blocky::boxed((0..20).collect(), (0..20).map(|d| if d < 10 { 0.5 } else { 1.0 }).collect(), 10);
        let opt = Blocky::boxed(vec![50], vec![1.0], 10);
        let mut s = ReqOptScorer::new(req, opt, true);
        s.set_min_competitive_score(0.8);
        assert_eq!(s.advance(0), 10);
        assert_eq!(s.next(), 11);
    }
}
