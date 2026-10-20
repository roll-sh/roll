//! Vectors from `numeric_range_tree`'s tests (integration and debug snapshots), the HLL crate,
//! and the pytests' `NUMIDX_SUMMARY` / `DUMP_NUMIDXTREE` expectations.

use super::hll::{Hll, hash_value};
use super::*;

fn build_tree(n: u64, compress: bool, max_depth_range: usize) -> NumericRangeTree {
    let mut t = NumericRangeTree::new(compress);
    for i in 1..=n {
        t.add(i, i as f64, false, max_depth_range);
    }
    t
}

/// `populated_tree` of the debug tests: values `start + (i - 1) * step`.
fn populated(count: u64, start: f64, step: f64, compress: bool) -> NumericRangeTree {
    let mut t = NumericRangeTree::new(compress);
    for i in 1..=count {
        t.add(i, start + (i - 1) as f64 * step, false, 0);
    }
    t
}

fn hll_of(values: &[f64]) -> usize {
    let mut h = Hll::new();
    values.iter().for_each(|&v| h.add(v));
    h.count()
}

/// `check_tree_invariants` + `check_memoized_stats`.
fn check_invariants(t: &NumericRangeTree) {
    fn walk(t: &NumericRangeTree, id: NodeId) -> (f64, f64) {
        match t.node(id) {
            Node::Leaf(r) => (r.min_val(), r.max_val()),
            Node::Internal(n) => {
                let (lmin, lmax) = walk(t, n.left);
                let (rmin, rmax) = walk(t, n.right);
                let (ld, rd) = (t.node(n.left).max_depth(), t.node(n.right).max_depth());
                assert_eq!(n.max_depth, ld.max(rd) + 1);
                assert!(ld.abs_diff(rd) <= MAXIMUM_DEPTH_IMBALANCE, "imbalance {ld} vs {rd}");
                let lempty = lmin == f64::INFINITY && lmax == f64::NEG_INFINITY;
                let rempty = rmin == f64::INFINITY && rmax == f64::NEG_INFINITY;
                if !lempty {
                    assert!(lmax < n.value);
                }
                if !rempty {
                    assert!(rmin >= n.value);
                }
                let emin = if lempty { rmin } else { lmin };
                let emax = if rempty { lmax } else { rmax };
                if let Some(r) = &n.range
                    && !(lempty && rempty)
                {
                    assert!(r.min_val() <= emin && r.max_val() >= emax);
                }
                (emin, emax)
            }
        }
    }
    walk(t, t.root_id());
    let (mut ranges, mut leaves, mut entries, mut size, mut empty) = (0, 0, 0, 0, 0);
    for node in t.iter() {
        leaves += usize::from(node.is_leaf());
        if let Some(r) = node.range() {
            ranges += 1;
            size += r.memory_usage();
            if node.is_leaf() {
                entries += r.num_entries();
                empty += usize::from(r.num_docs() == 0);
            }
        }
    }
    assert_eq!(t.num_ranges(), ranges);
    assert_eq!(t.num_leaves(), leaves);
    assert_eq!(t.num_entries(), entries);
    assert_eq!(t.inverted_indexes_size(), size);
    assert_eq!(t.empty_leaves(), empty);
}

/// `check_delta_invariants`: an add's deltas equal the change in the recomputed totals.
fn add_checked(t: &mut NumericRangeTree, doc: u64, v: f64, multi: bool, mdr: usize) -> AddResult {
    let records = |t: &NumericRangeTree| t.ranges().map(NumericRange::num_entries).sum::<usize>() as i64;
    let blocks = |t: &NumericRangeTree| t.ranges().map(NumericRange::num_blocks).sum::<usize>() as i64;
    let (r0, b0, s0, n0, l0, rev0) = (records(t), blocks(t), t.inverted_indexes_size(), t.num_ranges(), t.num_leaves(), t.revision_id());
    let rv = t.add(doc, v, multi, mdr);
    assert_eq!(records(t) - r0, i64::from(rv.num_records_delta));
    assert_eq!(blocks(t) - b0, i64::from(rv.block_count_delta));
    assert_eq!(t.inverted_indexes_size() as i64 - s0 as i64, rv.size_delta);
    assert_eq!(t.num_ranges() as i64 - n0 as i64, i64::from(rv.num_ranges_delta));
    assert_eq!(t.num_leaves() as i64 - l0 as i64, i64::from(rv.num_leaves_delta));
    assert_eq!(t.revision_id() != rev0, rv.changed);
    check_invariants(t);
    rv
}

#[test]
fn sizes() {
    assert_eq!(NumericRangeTree::base_size(), 112);
    assert_eq!(NumericRange::new(false).memory_usage(), 32);
    assert_eq!(NumericRange::new(true).memory_usage(), 32);
    assert_eq!(NumericRangeTree::new(false).initial_size(), 32);
    // `test_debug_summary_empty`: 112 + 32 + 4 arena slots of 176.
    assert_eq!(NumericRangeTree::new(false).mem_usage(), 848);
}

#[test]
fn split_thresholds() {
    let got: Vec<usize> = (0..7).map(split_cardinality).collect();
    assert_eq!(got, [16, 64, 256, 1024, 2500, 2500, 2500]);
}

#[test]
fn hll_rank_and_hash() {
    // `test_rank_function` (rank bits 26 here).
    let mut h = Hll::new();
    h.add_hash(0);
    assert_eq!(h.registers()[0], 27);
    h.add_hash(0xFC00_0004);
    assert_eq!(h.registers()[63], 3);
    // The hash is over raw bits.
    assert_ne!(hash_value(0.0), hash_value(-0.0));
    assert_eq!(Hll::new().count(), 0);
}

#[test]
fn hll_counts_match_reference() {
    // Cards in the debug snapshots and `testDebugRangeTree`.
    let r = |a: i32, b: i32| (a..=b).map(f64::from).collect::<Vec<_>>();
    assert_eq!(hll_of(&r(1, 10)), 10);
    assert_eq!(hll_of(&r(0, 7)), 8);
    assert_eq!(hll_of(&r(8, 42)), 31);
    assert_eq!(hll_of(&r(43, 99)), 62);
    assert_eq!(hll_of(&[1.0, 2.0, 3.0, 1.0, 2.0, 3.0, 3.0, 4.0, 5.0]), 5);
    let mut a = Hll::new();
    r(0, 7).iter().for_each(|&v| a.add(v));
    let mut b = Hll::new();
    r(8, 42).iter().for_each(|&v| b.add(v));
    a.merge(&b);
    assert_eq!(a.count(), hll_of(&r(0, 42)));
}

// ── range.rs ──

#[test]
fn range_bounds_and_counts() {
    let mut r = NumericRange::new(false);
    assert_eq!((r.min_val(), r.max_val(), r.num_entries(), r.cardinality()), (f64::INFINITY, f64::NEG_INFINITY, 0, 0));
    r.add(1, 5.0);
    assert_eq!((r.min_val(), r.max_val()), (5.0, 5.0));
    r.add(2, 10.0);
    r.add(3, 2.0);
    assert_eq!((r.min_val(), r.max_val(), r.num_docs()), (2.0, 10.0, 3));
    assert!((2..=4).contains(&r.cardinality()));
    assert_eq!(r.iter().collect::<Vec<_>>(), [(1, 5.0), (2, 10.0), (3, 2.0)]);

    let mut r = NumericRange::new(false);
    r.add(1, 5.0);
    r.add(2, 10.0);
    assert!(r.contained_in(0.0, 20.0) && r.contained_in(5.0, 10.0));
    assert!(!r.contained_in(6.0, 20.0) && !r.contained_in(0.0, 9.0));
    assert!(r.overlaps(0.0, 6.0) && r.overlaps(8.0, 20.0) && r.overlaps(6.0, 8.0) && r.overlaps(0.0, 20.0));
    assert!(!r.overlaps(11.0, 20.0) && !r.overlaps(0.0, 4.0));

    let mut w = NumericRange::new(false);
    w.add_without_cardinality(1, 5.0);
    w.add_without_cardinality(2, 10.0);
    w.add_without_cardinality(3, 2.0);
    assert_eq!((w.min_val(), w.max_val(), w.num_entries(), w.cardinality()), (2.0, 10.0, 3, 0));
}

// ── tree.rs ──

#[test]
fn new_and_basic_adds() {
    let mut t = NumericRangeTree::new(false);
    assert_eq!((t.num_ranges(), t.num_leaves(), t.num_entries(), t.last_doc_id(), t.revision_id(), t.empty_leaves()), (1, 1, 0, 0, 0, 1));
    assert!(t.add(1, 5.0, false, 0).size_delta > 0);
    assert!(t.add(2, 10.0, false, 0).size_delta > 0);
    assert_eq!((t.num_entries(), t.last_doc_id(), t.empty_leaves()), (2, 2, 0));
    assert_ne!(NumericRangeTree::new(false).unique_id(), NumericRangeTree::new(false).unique_id());
    assert_eq!(t.increment_revision(), 1);
    assert_eq!(t.increment_revision(), 2);
}

#[test]
fn duplicates() {
    let mut t = NumericRangeTree::new(false);
    t.add(5, 10.0, false, 0);
    assert_eq!(t.add(5, 20.0, false, 0), AddResult::default());
    assert_eq!(t.add(3, 15.0, false, 0), AddResult::default());
    assert_eq!(t.num_entries(), 1);

    let mut t = NumericRangeTree::new(false);
    t.add(5, 10.0, true, 0);
    assert!(t.add(5, 20.0, true, 0).size_delta > 0);
    assert_eq!(t.num_entries(), 2);
}

#[test]
fn first_add_deltas() {
    let mut t = NumericRangeTree::new(false);
    // One 1-byte record: block array header + block (56) and a 1-byte buffer.
    let rv = t.add(1, 0.0, false, 0);
    assert_eq!(rv, AddResult { size_delta: 57, num_records_delta: 1, block_count_delta: 1, ..AddResult::default() });
    assert_eq!(t.inverted_indexes_size(), 89);
}

#[test]
fn identical_values_never_split() {
    let mut t = NumericRangeTree::new(false);
    for i in 1..=500 {
        t.add(i, 42.0, false, 0);
    }
    assert!(t.root().is_leaf());
    assert_eq!(t.num_entries(), 500);
}

#[test]
fn split_triggers_at_cardinality() {
    for compress in [false, true] {
        let t = build_tree(20, compress, 0);
        assert!(t.num_leaves() > 1 && t.num_ranges() > 1 && !t.root().is_leaf());
        assert!(t.root().split_value().unwrap() > 0.0);
        assert!(t.root().max_depth() >= 1);
    }
    // The first split comes exactly when the 16th distinct value arrives.
    assert_eq!(build_tree(15, false, 0).num_leaves(), 1);
    let t = build_tree(16, false, 0);
    assert_eq!((t.num_leaves(), t.root().split_value()), (2, Some(9.0)));
}

#[test]
fn compression_collapse_counts_empty_leaf() {
    let mut t = NumericRangeTree::new(true);
    let value_at = |i: u64| 100.5 + (i - 1) as f64 * 1e-7;
    let mut split_at = None;
    for i in 1..=28 {
        add_checked(&mut t, i, value_at(i), false, 0);
        if t.num_leaves() == 2 {
            split_at = Some(i);
            break;
        }
    }
    let split_at = split_at.expect("split");
    assert_eq!(t.empty_leaves(), 1);
    add_checked(&mut t, split_at + 1, value_at(split_at + 1), false, 0);
    assert_eq!(t.empty_leaves(), 0);
}

#[test]
fn balancing_keeps_invariants() {
    let mut t = NumericRangeTree::new(false);
    for i in 1..=5000u64 {
        add_checked(&mut t, i, i as f64, false, 0);
    }
    let mut t = NumericRangeTree::new(false);
    for i in (1..=5000u64).rev() {
        add_checked(&mut t, 5001 - i, i as f64, true, 0);
    }
    let mut t = NumericRangeTree::new(false);
    let mut doc = 1;
    for batch in 0..10u64 {
        let vals: Vec<u64> = (batch * 500 + 1..=batch * 500 + 500).collect();
        let vals: Vec<u64> = if batch % 2 == 0 { vals } else { vals.into_iter().rev().collect() };
        for v in vals {
            add_checked(&mut t, doc, v as f64, true, 0);
            doc += 1;
        }
    }
}

#[test]
fn max_depth_range_retention() {
    let t = build_tree(100, false, 0);
    assert!(t.num_leaves() > 1 && t.root().range().is_none());
    for compress in [false, true] {
        let t = build_tree(200, compress, 2);
        assert!(t.num_ranges() > t.num_leaves());
        let t = build_tree(5000, compress, 1);
        for node in t.iter() {
            if !node.is_leaf() && node.max_depth() > 1 {
                assert!(node.range().is_none());
            }
        }
    }
    let mut t = NumericRangeTree::new(false);
    for i in 1..=1000 {
        add_checked(&mut t, i, i as f64, false, 2);
    }
}

// ── debug snapshots ──

#[test]
fn snapshot_summary_50() {
    let t = populated(50, 0.0, 1.0, false);
    let got = (t.num_ranges(), t.num_leaves(), t.num_entries(), t.last_doc_id(), t.revision_id(), t.empty_leaves(), t.root().max_depth());
    assert_eq!(got, (2, 2, 50, 50, 1, 0, 1));
    assert_eq!(t.mem_usage(), 1155);
}

#[test]
fn snapshot_tree_10() {
    for compress in [false, true] {
        let t = populated(10, 1.0, 1.0, compress);
        let Node::Leaf(r) = t.root() else { panic!() };
        assert_eq!((r.min_val(), r.max_val(), r.memory_usage(), r.cardinality(), r.num_blocks()), (1.0, 10.0, 114, 10, 1));
        assert_eq!(r.iter().collect::<Vec<_>>(), (1..=10).map(|i| (i, i as f64)).collect::<Vec<_>>());
        assert_eq!(r.entries().flags(), flags::STORE_NUMERIC);
    }
}

#[test]
fn snapshot_tree_100() {
    for compress in [false, true] {
        let t = populated(100, 0.0, 1.0, compress);
        assert_eq!((t.num_ranges(), t.num_entries(), t.revision_id(), t.empty_leaves()), (3, 100, 2, 0));
        let Node::Internal(root) = t.root() else { panic!() };
        assert_eq!((root.value, root.max_depth, root.range.is_none()), (8.0, 2, true));
        let Node::Internal(right) = t.node(root.right) else { panic!() };
        assert_eq!((right.value, right.max_depth), (43.0, 1));
        let leaf = |id| t.node(id).range().unwrap();
        let summary = |r: &NumericRange| (r.min_val(), r.max_val(), r.memory_usage(), r.cardinality(), r.num_docs(), r.entries().last_doc_id());
        assert_eq!(summary(leaf(root.left)), (0.0, 7.0, 105, 8, 8, Some(8)));
        assert_eq!(summary(leaf(right.left)), (8.0, 42.0, 209, 31, 35, Some(43)));
        assert_eq!(summary(leaf(right.right)), (43.0, 99.0, 264, 62, 57, Some(100)));
        // `DUMP_NUMIDX` order: reverse pre-order.
        let firsts: Vec<_> = t.ranges().map(|r| r.iter().next().unwrap().0).collect();
        assert_eq!(firsts, [44, 9, 1]);
    }
}

#[test]
fn multi_value_tree() {
    // `testDebugRangeTree`.
    let mut t = NumericRangeTree::new(false);
    for (doc, vals) in [(1, [1.0, 2.0, 3.0]), (2, [1.0, 2.0, 3.0]), (3, [3.0, 4.0, 5.0])] {
        for v in vals {
            add_checked(&mut t, doc, v, true, 0);
        }
    }
    let r = t.root().range().unwrap();
    assert_eq!((t.num_ranges(), t.num_entries(), t.last_doc_id(), t.revision_id()), (1, 9, 3, 0));
    assert_eq!((r.min_val(), r.max_val(), r.memory_usage(), r.cardinality(), r.num_docs(), r.num_entries()), (1.0, 5.0, 99, 5, 3, 9));
    assert_eq!(r.iter().map(|(d, _)| d).collect::<Vec<_>>(), [1, 1, 1, 2, 2, 2, 3, 3, 3]);
    assert_ne!(r.entries().flags() & flags::HAS_MULTI_VALUE, 0);
}

// ── pytests ──

#[test]
fn sanity_100k() {
    // `testSanity`: values i % 100 for docs 1..=100000.
    let mut t = NumericRangeTree::new(false);
    for i in 0..100_000u64 {
        t.add(i + 1, (i % 100) as f64, false, 0);
    }
    let got = (t.num_ranges(), t.num_leaves(), t.num_entries(), t.last_doc_id(), t.revision_id(), t.empty_leaves(), t.root().max_depth());
    assert_eq!(got, (13, 13, 100_000, 100_000, 12, 0, 4));
    check_invariants(&t);
}

#[test]
fn range_parents_config() {
    // `testRangeParentsConfig`: 1000 docs valued 0..999.
    for (mdr, ranges) in [(0, 4), (2, 6)] {
        let mut t = NumericRangeTree::new(false);
        for i in 0..1000u64 {
            t.add(i + 1, i as f64, false, mdr);
        }
        assert_eq!(t.num_ranges(), ranges, "parents {mdr}");
        check_invariants(&t);
    }
}

// ── find.rs ──

fn large_tree(compress: bool) -> NumericRangeTree {
    let mut t = NumericRangeTree::new(compress);
    for i in 1..=50_000u64 {
        t.add(i, ((i - 1) % 5000 + 1) as f64, false, 0);
    }
    t
}

fn check_find(ranges: &[&NumericRange], f: &NumericFilter) {
    for r in ranges {
        assert!(r.overlaps(f.min, f.max));
    }
    for w in ranges.windows(2) {
        if f.ascending {
            assert!(w[0].max_val() < w[1].min_val());
        } else {
            assert!(w[0].min_val() > w[1].max_val());
        }
    }
}

#[test]
fn find_large_tree() {
    for compress in [false, true] {
        let t = large_tree(compress);
        assert_eq!(t.num_entries(), 50_000);
        for (min, max) in [(0.0, 100.0), (10.0, 1000.0), (2500.0, 3500.0), (0.0, 5000.0), (4999.0, 4999.0), (0.0, 0.0)] {
            for ascending in [true, false] {
                let f = NumericFilter { ascending, ..NumericFilter::range(min, max) };
                check_find(&t.find(&f), &f);
            }
        }
    }
    let t = large_tree(false);
    let all = t.find(&NumericFilter::range(f64::NEG_INFINITY, f64::INFINITY));
    assert!(!all.is_empty());
    assert_eq!(all.iter().map(|r| r.num_docs() as usize).sum::<usize>(), t.num_entries());
    assert!(t.find(&NumericFilter::range(-1000.0, -1.0)).is_empty());
    let point = t.find(&NumericFilter::range(42.0, 42.0));
    assert!(!point.is_empty() && point.iter().all(|r| r.overlaps(42.0, 42.0)));

    let n_all = t.find(&NumericFilter::range(0.0, 5000.0)).len();
    let limited = NumericFilter { limit: 10, ..NumericFilter::range(0.0, 5000.0) };
    assert!(t.find(&limited).len() <= n_all);
    let offset = NumericFilter { offset: 100, limit: 10, ..NumericFilter::range(0.0, 5000.0) };
    assert!(t.find(&offset).len() <= n_all);

    let found = t.find_ranges(&NumericFilter::range(10.0, 1000.0));
    let docs = |fr: &FoundRange<'_>| fr.range.iter().filter(|&(_, v)| (10.0..=1000.0).contains(&v)).count();
    assert_eq!(found.iter().map(docs).sum::<usize>(), 991 * 10);
    for fr in &found {
        assert_eq!(fr.contained, fr.range.min_val() >= 10.0 && fr.range.max_val() <= 1000.0);
    }
}

#[test]
fn find_on_empty_tree() {
    let t = NumericRangeTree::new(false);
    assert!(t.find(&NumericFilter::range(0.0, 100.0)).is_empty());
    assert!(t.find(&NumericFilter::range(f64::NEG_INFINITY, f64::INFINITY)).is_empty());
}

#[test]
fn find_prefers_retained_parent() {
    let t = build_tree(1000, false, 2);
    let root = t.root().range().is_some();
    let all = t.find(&NumericFilter::range(f64::NEG_INFINITY, f64::INFINITY));
    if root {
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].num_entries(), 1000);
    }
    let f = NumericFilter { limit: 5, ..NumericFilter::range(0.0, 1000.0) };
    let limited = t.find(&f);
    check_find(&limited, &f);
}

// ── self-derived regression vectors ──

#[test]
fn sequential_5000_and_lcg() {
    let mut t = NumericRangeTree::new(false);
    let mut size = t.initial_size() as i64;
    let mut records = 0i64;
    for i in 1..=5000u64 {
        let rv = t.add(i, i as f64, false, 0);
        size += rv.size_delta;
        records += i64::from(rv.num_records_delta);
    }
    check_invariants(&t);
    assert_eq!(size, t.inverted_indexes_size() as i64);
    assert_eq!(records, t.ranges().map(NumericRange::num_entries).sum::<usize>() as i64);
    let seq = (t.num_ranges(), t.num_leaves(), t.revision_id(), t.root().max_depth(), t.inverted_indexes_size(), t.mem_usage());

    let mut t = NumericRangeTree::new(false);
    let mut x: u64 = 12345;
    for i in 1..=5000u64 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let v = ((x >> 33) % 100_000) as f64 / 10.0;
        add_checked(&mut t, i, v, false, 0);
    }
    let lcg = (t.num_ranges(), t.num_leaves(), t.revision_id(), t.root().max_depth(), t.inverted_indexes_size(), t.mem_usage());
    assert_eq!(seq, (7, 7, 6, 4, 24732, 27660));
    assert_eq!(lcg, (8, 8, 7, 3, 50303, 53231));
}
