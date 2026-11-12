//! Range tombstones as markers, and their merging.
//!
//! In a partition's item stream a range tombstone is a pair of **markers** at its bounds: one
//! that opens a deletion and one that closes it; where two ranges touch, one *boundary* marker
//! closes the first and opens the second. Each source's markers never overlap, so a source has
//! at most one open deletion at any position, and a reader that starts in the middle of a
//! partition only needs that one deletion (stored in the block index). Merging sources keeps
//! the open deletion of each and emits a marker whenever the biggest one changes.
//!
//! Follows Apache Cassandra 5.0 (Apache-2.0): `RangeTombstoneMarker.Merger`,
//! `RangeTombstoneBoundaryMarker`, and the marker handling of `PurgeFunction`.

use crate::compare::{ClusteringComparator, compare_bounds, compare_clustering_bound};
use crate::model::{Bound, Clustering, DeletionTime, RangeTombstone};
use crate::reconcile::Purger;
use std::cmp::Ordering;

/// A range tombstone marker, in iteration order: `close` ends the deletion open before
/// `bound`, `open` starts one after it (both for a boundary).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Marker {
    pub bound: Bound,
    pub close: Option<DeletionTime>,
    pub open: Option<DeletionTime>,
}

impl Marker {
    pub fn open(bound: Bound, d: DeletionTime) -> Self {
        Self { bound, close: None, open: Some(d) }
    }

    pub fn close(bound: Bound, d: DeletionTime) -> Self {
        Self { bound, close: Some(d), open: None }
    }

    /// The same marker seen in the other direction.
    pub fn reversed(self) -> Self {
        Self { bound: self.bound, close: self.open, open: self.close }
    }

    pub fn is_boundary(&self) -> bool {
        self.close.is_some() && self.open.is_some()
    }

    /// Markers count as tombstones when reading (Cassandra counts each marker once).
    pub fn deletion(&self) -> DeletionTime {
        self.open.or(self.close).unwrap_or(DeletionTime::LIVE)
    }
}

/// Order positions in iteration order.
#[inline]
pub fn dir(o: Ordering, reversed: bool) -> Ordering {
    if reversed { o.reverse() } else { o }
}

/// Turn range tombstones (in any order, possibly overlapping) into disjoint ones in clustering
/// order, each covering its range with the biggest deletion that applies there. Adjacent ranges
/// with the same deletion are joined.
pub fn normalize(cmp: &dyn ClusteringComparator, rts: Vec<RangeTombstone>) -> Vec<RangeTombstone> {
    if rts.len() <= 1 {
        return rts.into_iter().filter(|rt| compare_bounds(cmp, &rt.start, &rt.end) == Ordering::Less).collect();
    }
    // Events: (position, open?, deletion). At equal positions order doesn't matter because the
    // active set is only inspected after the whole group is applied.
    let mut events: Vec<(Bound, bool, DeletionTime)> = Vec::with_capacity(rts.len() * 2);
    for rt in rts {
        if compare_bounds(cmp, &rt.start, &rt.end) != Ordering::Less {
            continue;
        }
        events.push((rt.start, true, rt.deletion));
        events.push((rt.end, false, rt.deletion));
    }
    events.sort_by(|a, b| compare_bounds(cmp, &a.0, &b.0));
    let mut active: Vec<DeletionTime> = Vec::new();
    let mut out: Vec<RangeTombstone> = Vec::new();
    let mut current: Option<(Bound, DeletionTime)> = None;
    let mut i = 0;
    while i < events.len() {
        let mut j = i;
        while j < events.len() && compare_bounds(cmp, &events[i].0, &events[j].0) == Ordering::Equal {
            let (_, open, d) = &events[j];
            if *open {
                active.push(*d);
            } else if let Some(k) = active.iter().position(|a| a == d) {
                active.swap_remove(k);
            }
            j += 1;
        }
        let pos = events[i].0.clone();
        let top = active.iter().copied().reduce(DeletionTime::max);
        match (&current, top) {
            (Some((_, d)), Some(t)) if *d == t => {}
            _ => {
                if let Some((start, d)) = current.take() {
                    out.push(RangeTombstone { start, end: pos.clone(), deletion: d });
                }
                current = top.map(|t| (pos, t));
            }
        }
        i = j;
    }
    out
}

/// Markers of disjoint, sorted range tombstones (as [`normalize`] returns them).
pub fn to_markers(cmp: &dyn ClusteringComparator, rts: &[RangeTombstone]) -> Vec<Marker> {
    let mut out: Vec<Marker> = Vec::with_capacity(rts.len() * 2);
    for rt in rts {
        match out.last_mut() {
            Some(last) if last.open.is_none() && compare_bounds(cmp, &last.bound, &rt.start) == Ordering::Equal => {
                last.open = Some(rt.deletion);
            }
            _ => out.push(Marker::open(rt.start.clone(), rt.deletion)),
        }
        out.push(Marker::close(rt.end.clone(), rt.deletion));
    }
    out
}

/// The deletion covering `c` in disjoint, sorted range tombstones.
pub fn deletion_at(cmp: &dyn ClusteringComparator, rts: &[RangeTombstone], c: &Clustering) -> DeletionTime {
    // The first range whose end is after `c`.
    let i = rts.partition_point(|rt| compare_clustering_bound(cmp, c, &rt.end) == Ordering::Greater);
    match rts.get(i) {
        Some(rt) if compare_clustering_bound(cmp, c, &rt.start) == Ordering::Greater => rt.deletion,
        _ => DeletionTime::LIVE,
    }
}

/// Insert `new` into disjoint, sorted `rts`, keeping them so.
pub fn insert(cmp: &dyn ClusteringComparator, rts: &mut Vec<RangeTombstone>, new: RangeTombstone) {
    if compare_bounds(cmp, &new.start, &new.end) != Ordering::Less || new.deletion.is_live() {
        return;
    }
    // Ranges that overlap or touch `new`: end >= new.start and start <= new.end.
    let lo = rts.partition_point(|rt| compare_bounds(cmp, &rt.end, &new.start) == Ordering::Less);
    let hi = rts.partition_point(|rt| compare_bounds(cmp, &rt.start, &new.end) != Ordering::Greater);
    if lo >= hi {
        rts.insert(lo, new);
        return;
    }
    let mut part: Vec<RangeTombstone> = rts.drain(lo..hi).collect();
    part.push(new);
    let merged = normalize(cmp, part);
    let tail = rts.split_off(lo);
    rts.extend(merged);
    rts.extend(tail);
}

/// Merges the marker streams of several sources (`RangeTombstoneMarker.Merger`).
pub struct MarkerMerger {
    pub partition_deletion: DeletionTime,
    open: Vec<Option<DeletionTime>>,
    current: DeletionTime,
}

impl MarkerMerger {
    pub fn new(sources: usize, partition_deletion: DeletionTime) -> Self {
        Self { partition_deletion, open: vec![None; sources], current: DeletionTime::LIVE }
    }

    /// The deletion open in the merged stream (`LIVE` when none or when the partition deletion
    /// covers it).
    pub fn current(&self) -> DeletionTime {
        self.current
    }

    /// The deletion that applies to rows at the current position.
    pub fn active(&self) -> DeletionTime {
        if self.current.is_live() { self.partition_deletion } else { self.current }
    }

    /// The deletion source `i` has open.
    pub fn source_open(&self, i: usize) -> Option<DeletionTime> {
        self.open[i]
    }

    /// Apply the markers found at one position (`(source, marker)`), returning the merged
    /// marker if the open deletion changed.
    pub fn merge(&mut self, bound: &Bound, markers: &[(usize, &Marker)]) -> Option<Marker> {
        for (i, m) in markers {
            self.open[*i] = m.open;
        }
        let biggest = self.open.iter().flatten().copied().reduce(DeletionTime::max);
        let new = match biggest {
            Some(d) if d.supersedes(&self.partition_deletion) => d,
            _ => DeletionTime::LIVE,
        };
        let prev = std::mem::replace(&mut self.current, new);
        if prev == new {
            return None;
        }
        Some(Marker { bound: bound.clone(), close: (!prev.is_live()).then_some(prev), open: (!new.is_live()).then_some(new) })
    }
}

/// `PurgeFunction.applyToMarker`: drop purgeable deletions from a marker.
pub fn purge_marker(m: Marker, p: &dyn Purger) -> Option<Marker> {
    let close = m.close.filter(|d| !p.should_purge_deletion(d));
    let open = m.open.filter(|d| !p.should_purge_deletion(d));
    if close.is_none() && open.is_none() {
        return None;
    }
    Some(Marker { bound: m.bound, close, open })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::BytesComparator;

    fn ck(s: &str) -> Clustering {
        Clustering::new([s.as_bytes()])
    }

    fn rt(a: &str, b: &str, ts: i64) -> RangeTombstone {
        RangeTombstone::new(Bound::inclusive_start(ck(a)), Bound::inclusive_end(ck(b)), DeletionTime::new(ts, 1))
    }

    #[test]
    fn normalize_overlaps() {
        let cmp = BytesComparator;
        let out = normalize(&cmp, vec![rt("b", "f", 1), rt("d", "h", 2), rt("x", "z", 1)]);
        assert_eq!(
            out,
            vec![
                RangeTombstone::new(Bound::inclusive_start(ck("b")), Bound::inclusive_start(ck("d")), DeletionTime::new(1, 1)),
                RangeTombstone::new(Bound::inclusive_start(ck("d")), Bound::inclusive_end(ck("h")), DeletionTime::new(2, 1)),
                rt("x", "z", 1),
            ]
        );
        // A range inside a bigger deletion disappears; equal deletions join.
        let out = normalize(&cmp, vec![rt("a", "z", 5), rt("c", "d", 2), rt("y", "zz", 5)]);
        assert_eq!(out, vec![RangeTombstone::new(Bound::inclusive_start(ck("a")), Bound::inclusive_end(ck("zz")), DeletionTime::new(5, 1))]);
        // Empty ranges vanish.
        let empty = RangeTombstone::new(Bound::exclusive_start(ck("a")), Bound::inclusive_end(ck("a")), DeletionTime::new(1, 1));
        assert!(normalize(&cmp, vec![empty.clone(), empty]).is_empty());
    }

    #[test]
    fn deletion_lookup_and_insert() {
        let cmp = BytesComparator;
        let mut rts = Vec::new();
        insert(&cmp, &mut rts, rt("m", "p", 3));
        insert(&cmp, &mut rts, rt("a", "c", 1));
        insert(&cmp, &mut rts, rt("b", "n", 2));
        insert(&cmp, &mut rts, rt("z", "a", 9));
        assert_eq!(deletion_at(&cmp, &rts, &ck("a")), DeletionTime::new(1, 1));
        assert_eq!(deletion_at(&cmp, &rts, &ck("c")), DeletionTime::new(2, 1));
        assert_eq!(deletion_at(&cmp, &rts, &ck("n")), DeletionTime::new(3, 1));
        assert_eq!(deletion_at(&cmp, &rts, &ck("q")), DeletionTime::LIVE);
        assert_eq!(rts.len(), 3);
        let markers = to_markers(&cmp, &rts);
        assert_eq!(markers.len(), 4, "open, boundary, boundary, close");
        assert!(markers[1].is_boundary() && markers[2].is_boundary());
    }

    #[test]
    fn merger_emits_changes_only() {
        let cmp = BytesComparator;
        let _ = cmp;
        let mut m = MarkerMerger::new(2, DeletionTime::new(1, 1));
        let a = Marker::open(Bound::inclusive_start(ck("a")), DeletionTime::new(5, 1));
        let b = Marker::open(Bound::inclusive_start(ck("b")), DeletionTime::new(3, 1));
        let out = m.merge(&a.bound, &[(0, &a)]).unwrap();
        assert_eq!(out.open, Some(DeletionTime::new(5, 1)));
        assert_eq!(m.merge(&b.bound, &[(1, &b)]), None, "shadowed by the open 5");
        let c = Marker::close(Bound::inclusive_end(ck("c")), DeletionTime::new(5, 1));
        let out = m.merge(&c.bound, &[(0, &c)]).unwrap();
        assert!(out.is_boundary());
        assert_eq!(out.open, Some(DeletionTime::new(3, 1)));
        // A deletion older than the partition deletion never opens.
        let mut m = MarkerMerger::new(1, DeletionTime::new(10, 1));
        assert_eq!(m.merge(&a.bound, &[(0, &a)]), None);
        assert_eq!(m.active(), DeletionTime::new(10, 1));
    }
}
