//! Partition items, and restricting a source's items to the requested slices.
//!
//! A source (memtable partition or SSTable cursor) produces its rows and markers in iteration
//! order; the [`Clipper`] drops what's outside the slices and closes/opens range tombstones at
//! slice bounds, so every source hands the merge well-formed marker pairs within the slices
//! (as Cassandra's slice iterators do).

use crate::compare::{ClusteringComparator, Pos, compare_bounds, compare_pos};
use crate::model::{Bound, DeletionTime, Row, Slice};
use crate::tombstones::{Marker, dir};
use std::cmp::Ordering;
use std::collections::VecDeque;

/// One element of a partition's content, in clustering (or reversed) order.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Row(Row),
    Marker(Marker),
}

impl Item {
    pub fn pos(&self) -> Pos<'_> {
        match self {
            Item::Row(r) => Pos::Row(&r.clustering),
            Item::Marker(m) => Pos::Bound(&m.bound),
        }
    }
}

/// Slices in iteration order with direction-aware bounds.
#[derive(Clone, Debug)]
pub struct Slices {
    /// In clustering order, disjoint.
    pub slices: Vec<Slice>,
    pub reversed: bool,
}

impl Slices {
    pub fn all(reversed: bool) -> Self {
        Self { slices: vec![Slice::ALL], reversed }
    }

    pub fn len(&self) -> usize {
        self.slices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slices.is_empty()
    }

    /// The `i`-th slice in iteration order.
    pub fn get(&self, i: usize) -> &Slice {
        if self.reversed { &self.slices[self.slices.len() - 1 - i] } else { &self.slices[i] }
    }

    /// Where iteration enters slice `i`.
    pub fn enter(&self, i: usize) -> &Bound {
        let s = self.get(i);
        if self.reversed { &s.end } else { &s.start }
    }

    pub fn exit(&self, i: usize) -> &Bound {
        let s = self.get(i);
        if self.reversed { &s.start } else { &s.end }
    }

    /// Sort and merge overlapping slices, dropping empty ones.
    pub fn normalize(cmp: &dyn ClusteringComparator, mut slices: Vec<Slice>, reversed: bool) -> Self {
        slices.retain(|s| compare_bounds(cmp, &s.start, &s.end) == Ordering::Less);
        slices.sort_by(|a, b| compare_bounds(cmp, &a.start, &b.start));
        let mut out: Vec<Slice> = Vec::with_capacity(slices.len());
        for s in slices {
            match out.last_mut() {
                Some(last) if compare_bounds(cmp, &s.start, &last.end) != Ordering::Greater => {
                    if compare_bounds(cmp, &s.end, &last.end) == Ordering::Greater {
                        last.end = s.end;
                    }
                }
                _ => out.push(s),
            }
        }
        Self { slices: out, reversed }
    }

    /// Whether `[lo, hi]` (a source's clustering range) intersects any slice.
    pub fn intersects(&self, cmp: &dyn ClusteringComparator, lo: &Bound, hi: &Bound) -> bool {
        self.slices.iter().any(|s| compare_bounds(cmp, &s.start, hi) != Ordering::Greater && compare_bounds(cmp, lo, &s.end) != Ordering::Greater)
    }
}

/// Restricts one source's items to [`Slices`].
pub struct Clipper {
    pub slices: Slices,
    idx: usize,
    entered: bool,
    /// The deletion open in the source at the current position.
    open: Option<DeletionTime>,
    out: VecDeque<Item>,
}

impl Clipper {
    pub fn new(slices: Slices) -> Self {
        Self { slices, idx: 0, entered: false, open: None, out: VecDeque::new() }
    }

    /// Every slice is behind us: the source can stop reading this partition.
    pub fn done(&self) -> bool {
        self.idx >= self.slices.len()
    }

    /// Where the next wanted item starts, when the source may skip ahead to it.
    pub fn seek_target(&self) -> Option<&Bound> {
        (!self.done() && !self.entered).then(|| self.slices.enter(self.idx))
    }

    /// The source jumped (to the start of a continuation block) where `open` is open.
    pub fn set_open(&mut self, open: Option<DeletionTime>) {
        debug_assert!(!self.entered);
        self.open = open;
    }

    pub fn pop(&mut self) -> Option<Item> {
        self.out.pop_front()
    }

    pub fn has_output(&self) -> bool {
        !self.out.is_empty()
    }

    /// Feed the source's next item.
    pub fn push(&mut self, cmp: &dyn ClusteringComparator, item: Item) {
        let rev = self.slices.reversed;
        loop {
            if self.done() {
                self.track(&item);
                return;
            }
            let exit = self.slices.exit(self.idx);
            let o_exit = dir(compare_pos(cmp, item.pos(), Pos::Bound(exit)), rev);
            if o_exit == Ordering::Greater {
                if let Some(d) = self.open {
                    // A deletion open across the whole slice covers it even without items.
                    if !self.entered {
                        self.out.push_back(Item::Marker(Marker::open(self.slices.enter(self.idx).clone(), d)));
                    }
                    self.out.push_back(Item::Marker(Marker::close(exit.clone(), d)));
                }
                self.idx += 1;
                self.entered = false;
                continue;
            }
            let enter = self.slices.enter(self.idx);
            let o_enter = dir(compare_pos(cmp, item.pos(), Pos::Bound(enter)), rev);
            if o_enter == Ordering::Less {
                self.track(&item);
                return;
            }
            let on_enter = o_enter == Ordering::Equal;
            let on_exit = o_exit == Ordering::Equal;
            if !self.entered {
                self.entered = true;
                if !on_enter && let Some(d) = self.open {
                    self.out.push_back(Item::Marker(Marker::open(enter.clone(), d)));
                }
            }
            match item {
                Item::Marker(m) => {
                    self.open = m.open;
                    let close = if on_enter { None } else { m.close };
                    let open = if on_exit { None } else { m.open };
                    if close.is_some() || open.is_some() {
                        self.out.push_back(Item::Marker(Marker { bound: m.bound, close, open }));
                    }
                    if on_exit {
                        self.idx += 1;
                        self.entered = false;
                    }
                }
                row @ Item::Row(_) => self.out.push_back(row),
            }
            return;
        }
    }

    /// The source's partition ended.
    pub fn finish(&mut self) {
        if !self.done() && self.entered && let Some(d) = self.open.take() {
            self.out.push_back(Item::Marker(Marker::close(self.slices.exit(self.idx).clone(), d)));
        }
        self.idx = self.slices.len();
    }

    fn track(&mut self, item: &Item) {
        if let Item::Marker(m) = item {
            self.open = m.open;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::BytesComparator;
    use crate::model::{Clustering, Row};

    fn ck(s: &str) -> Clustering {
        Clustering::new([s.as_bytes()])
    }
    fn row(s: &str) -> Item {
        Item::Row(Row::new(ck(s)))
    }
    fn d(ts: i64) -> DeletionTime {
        DeletionTime::new(ts, 1)
    }

    fn run(slices: Slices, items: Vec<Item>) -> Vec<Item> {
        let cmp = BytesComparator;
        let mut c = Clipper::new(slices);
        let mut out = Vec::new();
        for i in items {
            c.push(&cmp, i);
            while let Some(x) = c.pop() {
                out.push(x);
            }
        }
        c.finish();
        while let Some(x) = c.pop() {
            out.push(x);
        }
        out
    }

    #[test]
    fn clips_markers_to_slices() {
        let cmp = BytesComparator;
        let slices = Slices::normalize(&cmp, vec![Slice::new(Bound::inclusive_start(ck("c")), Bound::inclusive_end(ck("e")))], false);
        let items = vec![
            Item::Marker(Marker::open(Bound::inclusive_start(ck("a")), d(5))),
            row("b"),
            row("d"),
            Item::Marker(Marker::close(Bound::inclusive_end(ck("g")), d(5))),
            row("h"),
        ];
        assert_eq!(
            run(slices, items),
            vec![
                Item::Marker(Marker::open(Bound::inclusive_start(ck("c")), d(5))),
                row("d"),
                Item::Marker(Marker::close(Bound::inclusive_end(ck("e")), d(5))),
            ]
        );
    }

    #[test]
    fn reversed_and_multiple_slices() {
        let cmp = BytesComparator;
        let slices = Slices::normalize(
            &cmp,
            vec![Slice::prefix(ck("b")), Slice::prefix(ck("d")), Slice::new(Bound::inclusive_start(ck("c")), Bound::exclusive_end(ck("c")))],
            true,
        );
        assert_eq!(slices.len(), 2, "the empty slice is dropped");
        let items = vec![row("e"), row("d"), row("c"), row("b"), row("a")];
        assert_eq!(run(slices, items), vec![row("d"), row("b")]);
    }

    #[test]
    fn markers_on_slice_bounds() {
        let cmp = BytesComparator;
        let slices = Slices::normalize(&cmp, vec![Slice::new(Bound::inclusive_start(ck("c")), Bound::inclusive_end(ck("e")))], false);
        // A boundary exactly at the slice start only opens; one at the end only closes.
        let items = vec![
            Item::Marker(Marker::open(Bound::inclusive_start(ck("a")), d(1))),
            Item::Marker(Marker { bound: Bound::inclusive_start(ck("c")), close: Some(d(1)), open: Some(d(2)) }),
            Item::Marker(Marker { bound: Bound::inclusive_end(ck("e")), close: Some(d(2)), open: Some(d(3)) }),
            Item::Marker(Marker::close(Bound::inclusive_end(ck("z")), d(3))),
        ];
        assert_eq!(
            run(slices, items),
            vec![
                Item::Marker(Marker::open(Bound::inclusive_start(ck("c")), d(2))),
                Item::Marker(Marker::close(Bound::inclusive_end(ck("e")), d(2))),
            ]
        );
    }

    #[test]
    fn open_at_seek() {
        let cmp = BytesComparator;
        let slices = Slices::all(false);
        let mut c = Clipper::new(slices);
        c.set_open(Some(d(4)));
        c.push(&cmp, row("m"));
        assert_eq!(c.pop(), Some(Item::Marker(Marker::open(Bound::BOTTOM, d(4)))));
        assert_eq!(c.pop(), Some(row("m")));
    }
}
