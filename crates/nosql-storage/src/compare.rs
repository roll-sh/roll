//! Ordering of clusterings, bounds and cell paths, supplied by the engine.
//!
//! Storage doesn't know CQL types. The engine implements [`ClusteringComparator`] per table:
//! one comparison per clustering component (by the column's type, already reversed for
//! `CLUSTERING ORDER BY ... DESC` columns) and one per complex column's cell paths (timeuuid
//! for lists, the element type for sets, the key type for maps, the field index for UDTs).
//! Prefix and bound comparison is built on top here, following Cassandra's
//! `ClusteringComparator.compare(ClusteringPrefix, ClusteringPrefix)`.

use crate::model::{Bound, Clustering, ColumnId};
use std::cmp::Ordering;

pub trait ClusteringComparator: Send + Sync + 'static {
    /// Compare component `index` of two clusterings, in the table's clustering order.
    fn compare_component(&self, index: usize, a: &[u8], b: &[u8]) -> Ordering;

    /// Compare two cell paths of complex column `column`.
    fn compare_path(&self, column: ColumnId, a: &[u8], b: &[u8]) -> Ordering;
}

/// Unsigned byte order for every component and path: enough for system tables keyed by text or
/// blobs, and for tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct BytesComparator;

impl ClusteringComparator for BytesComparator {
    fn compare_component(&self, _: usize, a: &[u8], b: &[u8]) -> Ordering {
        a.cmp(b)
    }

    fn compare_path(&self, _: ColumnId, a: &[u8], b: &[u8]) -> Ordering {
        a.cmp(b)
    }
}

/// Compare the common components of two prefixes. Returns the order and whether `a` is shorter,
/// equal, or longer than `b` when all common components are equal.
#[inline]
fn compare_components(cmp: &dyn ClusteringComparator, a: &Clustering, b: &Clustering) -> (Ordering, Ordering) {
    if a.encoded() == b.encoded() {
        return (Ordering::Equal, Ordering::Equal);
    }
    let mut ia = a.components();
    let mut ib = b.components();
    let mut i = 0;
    loop {
        match (ia.next(), ib.next()) {
            (Some(x), Some(y)) => {
                let o = cmp.compare_component(i, x, y);
                if o != Ordering::Equal {
                    return (o, Ordering::Equal);
                }
            }
            (None, None) => return (Ordering::Equal, Ordering::Equal),
            (None, Some(_)) => return (Ordering::Equal, Ordering::Less),
            (Some(_), None) => return (Ordering::Equal, Ordering::Greater),
        }
        i += 1;
    }
}

/// Order of two full clusterings (or two prefixes of equal kind).
pub fn compare_clustering(cmp: &dyn ClusteringComparator, a: &Clustering, b: &Clustering) -> Ordering {
    let (o, len) = compare_components(cmp, a, b);
    o.then(len)
}

/// Where clustering `c` is relative to bound `b`. Never `Equal`: a bound is between clusterings.
pub fn compare_clustering_bound(cmp: &dyn ClusteringComparator, c: &Clustering, b: &Bound) -> Ordering {
    let (o, _) = compare_components(cmp, c, &b.prefix);
    if o != Ordering::Equal {
        return o;
    }
    // The bound's prefix covers `c` (a full clustering is never shorter than a bound's prefix):
    // the bound's side decides.
    if b.after { Ordering::Less } else { Ordering::Greater }
}

/// Order of two bounds. Bounds at the same position (an exclusive end and the inclusive start
/// of the same prefix) compare equal.
pub fn compare_bounds(cmp: &dyn ClusteringComparator, a: &Bound, b: &Bound) -> Ordering {
    let (o, len) = compare_components(cmp, &a.prefix, &b.prefix);
    if o != Ordering::Equal {
        return o;
    }
    match len {
        Ordering::Equal => a.after.cmp(&b.after),
        // `a` is shorter: its side decides (before → less, after → greater).
        Ordering::Less => {
            if a.after {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        Ordering::Greater => {
            if b.after {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
    }
}

/// Order of two cell paths of `column` (`None` sorts first; only simple cells have none).
pub fn compare_paths(cmp: &dyn ClusteringComparator, column: ColumnId, a: Option<&[u8]>, b: Option<&[u8]>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => cmp.compare_path(column, a, b),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
    }
}

/// A clustering is in `[start, end]`.
pub fn slice_contains(cmp: &dyn ClusteringComparator, start: &Bound, end: &Bound, c: &Clustering) -> bool {
    compare_clustering_bound(cmp, c, start) == Ordering::Greater && compare_clustering_bound(cmp, c, end) == Ordering::Less
}

/// A position in a partition's item stream: a row's clustering or a range tombstone marker's
/// bound.
#[derive(Clone, Copy, Debug)]
pub enum Pos<'a> {
    Row(&'a Clustering),
    Bound(&'a Bound),
}

pub fn compare_pos(cmp: &dyn ClusteringComparator, a: Pos, b: Pos) -> Ordering {
    match (a, b) {
        (Pos::Row(x), Pos::Row(y)) => compare_clustering(cmp, x, y),
        (Pos::Row(x), Pos::Bound(y)) => compare_clustering_bound(cmp, x, y),
        (Pos::Bound(x), Pos::Row(y)) => compare_clustering_bound(cmp, y, x).reverse(),
        (Pos::Bound(x), Pos::Bound(y)) => compare_bounds(cmp, x, y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(parts: &[&str]) -> Clustering {
        Clustering::new(parts.iter().map(|p| p.as_bytes()))
    }

    #[test]
    fn bounds_follow_cassandra_kinds() {
        let cmp = BytesComparator;
        let a = c(&["a"]);
        let ab = c(&["a", "b"]);
        let b = c(&["b"]);
        // INCL_START(a) < (a,b) < INCL_END(a) < b
        assert_eq!(compare_clustering_bound(&cmp, &ab, &Bound::inclusive_start(a.clone())), Ordering::Greater);
        assert_eq!(compare_clustering_bound(&cmp, &ab, &Bound::inclusive_end(a.clone())), Ordering::Less);
        assert_eq!(compare_clustering_bound(&cmp, &b, &Bound::inclusive_end(a.clone())), Ordering::Greater);
        // A full clustering vs bounds on itself.
        assert_eq!(compare_clustering_bound(&cmp, &a, &Bound::inclusive_start(a.clone())), Ordering::Greater);
        assert_eq!(compare_clustering_bound(&cmp, &a, &Bound::exclusive_start(a.clone())), Ordering::Less);
        assert_eq!(compare_clustering_bound(&cmp, &a, &Bound::exclusive_end(a.clone())), Ordering::Greater);
        // EXCL_END(a) == INCL_START(a); INCL_END(a) == EXCL_START(a)
        assert_eq!(compare_bounds(&cmp, &Bound::exclusive_end(a.clone()), &Bound::inclusive_start(a.clone())), Ordering::Equal);
        assert_eq!(compare_bounds(&cmp, &Bound::inclusive_end(a.clone()), &Bound::exclusive_start(a.clone())), Ordering::Equal);
        // Shorter bounds enclose longer ones.
        assert_eq!(compare_bounds(&cmp, &Bound::inclusive_start(a.clone()), &Bound::inclusive_start(ab.clone())), Ordering::Less);
        assert_eq!(compare_bounds(&cmp, &Bound::inclusive_end(a.clone()), &Bound::inclusive_end(ab.clone())), Ordering::Greater);
        assert_eq!(compare_bounds(&cmp, &Bound::BOTTOM, &Bound::inclusive_start(a.clone())), Ordering::Less);
        assert_eq!(compare_bounds(&cmp, &Bound::TOP, &Bound::inclusive_end(ab.clone())), Ordering::Greater);
        assert_eq!(compare_clustering_bound(&cmp, &ab, &Bound::TOP), Ordering::Less);
        assert_eq!(compare_clustering_bound(&cmp, &ab, &Bound::BOTTOM), Ordering::Greater);
        assert!(slice_contains(&cmp, &Bound::inclusive_start(a.clone()), &Bound::inclusive_end(a.clone()), &ab));
        assert!(!slice_contains(&cmp, &Bound::exclusive_start(a.clone()), &Bound::TOP, &ab));
    }

    struct Desc;
    impl ClusteringComparator for Desc {
        fn compare_component(&self, _: usize, a: &[u8], b: &[u8]) -> Ordering {
            b.cmp(a)
        }
        fn compare_path(&self, _: ColumnId, a: &[u8], b: &[u8]) -> Ordering {
            a.cmp(b)
        }
    }

    #[test]
    fn engine_order_is_used() {
        assert_eq!(compare_clustering(&Desc, &c(&["a"]), &c(&["b"])), Ordering::Greater);
        assert_eq!(compare_bounds(&Desc, &Bound::inclusive_start(c(&["b"])), &Bound::inclusive_start(c(&["a"]))), Ordering::Less);
    }
}
