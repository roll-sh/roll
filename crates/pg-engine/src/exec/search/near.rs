//! Nearest-first walks of the search structures, for KNN index scans (`ORDER BY col <->
//! arg`): the candidate rows in ascending order of a lower bound on their distance, as far
//! as they are asked for (`exec/nearest.rs` reads them and puts them in their exact order,
//! as IndexNextWithReorder does). A `Grid` bounds an entry by its mapped box or interval
//! (`Quant` is monotone, so the box holds the value); `Trigrams` bounds a row by the query
//! trigrams it shares.

use super::{BLOCK, Ent, Grid, Quant, Query, State, Trigrams};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// A lower bound a little under the distance `d` (between values of magnitude `scale`):
/// PostgreSQL computes it in steps of its own, some of them in float4.
fn under(d: f64, scale: f64) -> f64 {
    let slack = d.abs() * 1e-6 + scale * 1e-14;
    if slack.is_finite() { d - slack } else { f64::NEG_INFINITY }
}

#[derive(Clone, Copy)]
enum Step {
    Row(u32),
    /// A `Grid` partition's entries `[i, end)` (low ends at or past the origin's), and
    /// entries `[start, j)` (before it).
    Right(usize, usize),
    Left(usize, usize),
    /// `Trigrams`' rows from this one on that share no trigram with the query.
    Tail(u32),
}

/// A step and the least distance anything it yields may have (the queue's least first).
struct Item {
    bound: f64,
    step: Step,
}

impl Ord for Item {
    fn cmp(&self, o: &Item) -> Ordering {
        o.bound.total_cmp(&self.bound)
    }
}
impl PartialOrd for Item {
    fn partial_cmp(&self, o: &Item) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl PartialEq for Item {
    fn eq(&self, o: &Item) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Item {}

/// The value of an `ord_key` (of a float, or else an integer).
fn key_value(k: u64, float: bool) -> f64 {
    if !float {
        return ((k ^ (1 << 63)) as i64) as f64;
    }
    f64::from_bits(if k >> 63 == 1 { k & !(1 << 63) } else { !k })
}

/// The least and greatest values whose keys `qz` maps to `q`.
fn values(qz: Quant, q: u32, float: bool) -> (f64, f64) {
    let lo = if q == 0 { 0 } else { qz.base.saturating_add((q as u64) << qz.shift) };
    let hi = if q == u32::MAX { u64::MAX } else { qz.base.saturating_add(((q as u64 + 1) << qz.shift) - 1) };
    let (a, b) = (key_value(lo, float), key_value(hi, float));
    (if a.is_nan() { f64::NEG_INFINITY } else { a }, if b.is_nan() { f64::INFINITY } else { b })
}

/// How far `at` is from `[lo, hi]`, and the magnitude of the end it is measured to.
fn gap(at: f64, (lo, hi): (f64, f64)) -> (f64, f64) {
    if lo > at {
        (lo - at, lo.abs())
    } else if hi < at {
        (at - hi, hi.abs())
    } else {
        (0.0, 0.0)
    }
}

/// A `Grid` walked outward from a point (two-dimensional ones) or a value: from the
/// partition's entries at the origin's low end on, rightward in their order, and leftward
/// before it (bounded by the greatest high end so far, `Grid::pmax`).
pub struct GridNear {
    st: State,
    merges: u64,
    prefix: Vec<u8>,
    /// The origin, its x as a key, and whether the column's keys are floats'.
    at: (f64, f64),
    at_key: u64,
    float: bool,
    q: Query,
    heap: BinaryHeap<Item>,
}

impl GridNear {
    pub(super) fn new(st: State, g: &Grid, prefix: Vec<u8>, at: (f64, f64), at_key: u64, float: bool, q: Query) -> GridNear {
        let mut n = GridNear { st, merges: 0, prefix, at, at_key, float, q, heap: BinaryHeap::new() };
        n.start(g);
        n
    }

    fn start(&mut self, g: &Grid) {
        self.merges = g.merges;
        let only = self.q.part.as_ref().map(|p| g.parts.get(p).copied());
        let parts: Vec<u32> = match only {
            Some(p) => p.into_iter().collect(),
            None => (0..g.parts.len() as u32).collect(),
        };
        let (x, y) = (g.qx.span(&self.q.x), g.qy.span(&self.q.y));
        let s = &g.sorted;
        let qx = g.qx.q(self.at_key);
        let mut items = Vec::new();
        for p in parts {
            let a = s.partition_point(|e| e.part < p);
            let b = s.partition_point(|e| e.part <= p);
            let m = a + s[a..b].partition_point(|e| e.lo < qx);
            if m < b {
                items.push(Item { bound: self.right(g, m), step: Step::Right(m, b) });
            }
            if a < m {
                items.push(Item { bound: self.left(g, m), step: Step::Left(a, m) });
            }
        }
        for e in &g.delta {
            if only.is_none_or(|p| p == Some(e.part)) && g.ent_ok(&x, &y, e) {
                items.push(Item { bound: self.ent(g, e), step: Step::Row(e.row) });
            }
        }
        items.extend(g.always.iter().map(|&r| Item { bound: f64::NEG_INFINITY, step: Step::Row(r) }));
        // (A null key's distance is null: last.)
        items.extend(g.nulls.iter().map(|&r| Item { bound: f64::INFINITY, step: Step::Row(r) }));
        self.heap = BinaryHeap::from(items);
    }

    fn ent(&self, g: &Grid, e: &Ent) -> f64 {
        let (dx, sx) = gap(self.at.0, (values(g.qx, e.lo, self.float).0, values(g.qx, e.hi, self.float).1));
        if !g.two_d() {
            return under(dx, self.at.0.abs() + sx);
        }
        let (lo, hi) = g.ys[e.row as usize];
        let (dy, sy) = gap(self.at.1, (values(g.qy, lo, true).0, values(g.qy, hi, true).1));
        under(dx.hypot(dy), self.at.0.abs() + self.at.1.abs() + sx + sy)
    }

    /// The bound of entries `i..` (their low ends from `sorted[i]`'s on).
    fn right(&self, g: &Grid, i: usize) -> f64 {
        let (d, s) = gap(self.at.0, (values(g.qx, g.sorted[i].lo, self.float).0, f64::INFINITY));
        under(d, self.at.0.abs() + s)
    }

    /// The bound of entries `..j` (their high ends at most the greatest up to `j`'s block).
    fn left(&self, g: &Grid, j: usize) -> f64 {
        let (d, s) = gap(self.at.0, (f64::NEG_INFINITY, values(g.qx, g.pmax[(j - 1) / BLOCK], self.float).1));
        under(d, self.at.0.abs() + s)
    }

    fn expand(&mut self, g: &Grid, step: Step) {
        let (x, y) = (g.qx.span(&self.q.x), g.qy.span(&self.q.y));
        let (from, to) = match step {
            Step::Right(i, end) => {
                let to = ((i / BLOCK + 1) * BLOCK).min(end);
                if to < end {
                    self.heap.push(Item { bound: self.right(g, to), step: Step::Right(to, end) });
                }
                (i, to)
            }
            Step::Left(start, j) => {
                let from = ((j - 1) / BLOCK * BLOCK).max(start);
                if from > start {
                    self.heap.push(Item { bound: self.left(g, from), step: Step::Left(start, from) });
                }
                (from, j)
            }
            Step::Row(_) | Step::Tail(_) => return,
        };
        for e in &g.sorted[from..to] {
            if g.ent_ok(&x, &y, e) {
                self.heap.push(Item { bound: self.ent(g, e), step: Step::Row(e.row) });
            }
        }
    }

    /// The least distance a row not yet returned may have (None: every row returned).
    /// (Entries merged since the walk began: it begins again, the rows returned already
    /// coming again.)
    pub fn bound(&mut self) -> Option<f64> {
        let st = self.st.clone();
        let g = st.lock();
        let g = g.downcast_ref::<Grid>()?;
        if g.merges != self.merges {
            self.start(g);
        }
        self.heap.peek().map(|i| i.bound)
    }

    /// The next candidate: its bound and data key.
    pub fn next(&mut self) -> Option<(f64, Vec<u8>)> {
        let st = self.st.clone();
        let g = st.lock();
        let g = g.downcast_ref::<Grid>()?;
        if g.merges != self.merges {
            self.start(g);
        }
        while let Some(it) = self.heap.pop() {
            match it.step {
                Step::Row(r) => return Some((it.bound, g.rows.key(&self.prefix, r))),
                step => self.expand(g, step),
            }
        }
        None
    }
}

/// A pg_trgm GiST index's rows by distance from a query string (`<->`: one minus the
/// similarity, at most the shared trigrams' share of the union of both; `<->>` and
/// `<->>>`: one minus a word similarity, at most the share of the query's trigrams the row
/// has). Rows sharing none are at distance 1, null ones last.
pub struct TrgmNear {
    st: State,
    prefix: Vec<u8>,
    heap: BinaryHeap<Item>,
    /// The rows not in the tail (bits), and how many rows there were.
    placed: Vec<u64>,
    rows: u32,
}

impl TrgmNear {
    pub(super) fn new(st: State, tg: &Trigrams, prefix: Vec<u8>, op: &str, arg: &str) -> TrgmNear {
        let q = crate::ext::pg_trgm::trgm::generate(arg);
        let m = q.len() as f64;
        let rows = tg.rows.len() as u32;
        let mut placed = vec![0u64; (rows as usize).div_ceil(64)];
        let mut mark = |r: u32| placed[r as usize / 64] |= 1 << (r % 64);
        let mut items = Vec::new();
        for (r, c) in tg.shared(&q) {
            mark(r);
            let c = c as f64;
            let most = if op == "<->" {
                let n = (tg.counts[r as usize] as f64).max(c);
                c / (n + m - c)
            } else {
                c / m
            };
            items.push(Item { bound: (1.0 - most) - 1e-6, step: Step::Row(r) });
        }
        for &r in &tg.always {
            mark(r);
            items.push(Item { bound: f64::NEG_INFINITY, step: Step::Row(r) });
        }
        for &r in &tg.nulls {
            mark(r);
            items.push(Item { bound: f64::INFINITY, step: Step::Row(r) });
        }
        if rows > 0 {
            items.push(Item { bound: 1.0, step: Step::Tail(0) });
        }
        TrgmNear { st, prefix, heap: BinaryHeap::from(items), placed, rows }
    }

    pub fn bound(&mut self) -> Option<f64> {
        self.heap.peek().map(|i| i.bound)
    }

    pub fn next(&mut self) -> Option<(f64, Vec<u8>)> {
        let st = self.st.clone();
        let g = st.lock();
        let tg = g.downcast_ref::<Trigrams>()?;
        while let Some(it) = self.heap.pop() {
            match it.step {
                Step::Row(r) => return Some((it.bound, tg.rows.key(&self.prefix, r))),
                Step::Tail(from) => {
                    let to = (from + 64).min(self.rows);
                    for r in from..to {
                        if self.placed[r as usize / 64] & (1 << (r % 64)) == 0 {
                            self.heap.push(Item { bound: 1.0, step: Step::Row(r) });
                        }
                    }
                    if to < self.rows {
                        self.heap.push(Item { bound: 1.0, step: Step::Tail(to) });
                    }
                }
                Step::Right(..) | Step::Left(..) => {}
            }
        }
        None
    }
}

/// A nearest-first walk: its candidates in ascending order of their bounds, the
/// transaction's own writes (not in the structure) first.
pub enum Walk {
    Grid(GridNear),
    Trgm(TrgmNear),
}

pub struct Near {
    pub(super) own: std::vec::IntoIter<Vec<u8>>,
    pub(super) walk: Walk,
}

impl Near {
    /// A lower bound on the distance of every candidate not returned yet (None: none left).
    pub fn bound(&mut self) -> Option<f64> {
        if self.own.len() > 0 {
            return Some(f64::NEG_INFINITY);
        }
        match &mut self.walk {
            Walk::Grid(g) => g.bound(),
            Walk::Trgm(t) => t.bound(),
        }
    }

    /// The next candidate's data key.
    pub fn next_key(&mut self) -> Option<Vec<u8>> {
        if let Some(k) = self.own.next() {
            return Some(k);
        }
        match &mut self.walk {
            Walk::Grid(g) => g.next(),
            Walk::Trgm(t) => t.next(),
        }
        .map(|(_, k)| k)
    }
}
