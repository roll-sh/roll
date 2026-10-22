//! Points: a BKD tree per field for numeric, date, ip, range and geo values (1–8 index
//! dimensions, up to 16 data dimensions, up to 32 bytes each).
//!
//! # Build
//!
//! Points are bulk-loaded: the writer collects `(doc, packed value)` pairs and recursively
//! splits them at the median of the index dimension with the largest spread, until leaves hold
//! at most [`MAX_POINTS_IN_LEAF`] points. Packed values of at most 12 bytes (numbers, dates, geo
//! points) become one `u128` key each (value above doc), so selections and sorts run over a
//! contiguous integer array; one-dimensional fields sort once and cut the tree from the sorted
//! run (about 60–80 ns per point, 130 for 2-d geo points). The working arrays are spill-backed
//! (page cache rather than heap past the budget) and leaves stream out as they are cut. The tree is a complete binary tree in heap order
//! (node 1 is the root, node `i` has children `2i` and `2i + 1`, leaves are `L..2L`), so inner
//! nodes are a flat array of `(split dim, split value)` and no child pointers are stored. Point
//! counts per node are not stored either: the split rule (`left = n * leaves(left) /
//! leaves(node)`) is deterministic, so readers recompute them while descending.
//!
//! # Leaves
//!
//! A leaf stores its doc ids (minimum + bit-packed deltas), then per data dimension the prefix
//! shared by all its values, the exact min/max suffixes of its index dimensions, and the
//! remaining suffix bytes of every value — or, when equal values repeat enough (status codes,
//! years, enums), of every run of equal values with its length (like Lucene's low-cardinality
//! leaves). Points within a leaf are sorted by value, so equal values are adjacent and a
//! crossing leaf hands each run of them to the visitor at once
//! ([`PointVisitor::visit_value_docs`]): low-cardinality fields compare each value once.
//!
//! # Intersect
//!
//! `intersect` walks the tree with the cell implied by split values; the visitor classifies each
//! cell as inside, outside or crossing its query. Inside subtrees stream their doc ids without
//! decoding values, outside ones are skipped. A crossing leaf is classified again in runs of 32
//! points (values are sorted, so a run's first and last value bound its first dimension): runs
//! outside are skipped, runs inside are visited without decoding values, and only crossing runs
//! are checked value by value. [`PointsReader::estimate_point_count`] runs the same walk without touching leaves.

use crate::bitpack::direct;
use crate::error::{Result, corrupt};
use crate::info::PointsSpec;
use crate::io::{In, Out, SinkOut};
use crate::slice::Slice;
use crate::spill::{Spill, SpillOptions, SpillSlice, SpillVec};

/// Leaf capacity (Lucene's default `maxPointsInLeafNode`).
pub const MAX_POINTS_IN_LEAF: usize = 512;
/// Points per run classified on its own inside a crossing leaf.
const SUB_BLOCK: usize = 32;

/// How a cell relates to a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    /// Every point in the cell matches.
    CellInsideQuery,
    /// No point in the cell matches.
    CellOutsideQuery,
    /// Some points may match: values must be checked.
    CellCrossesQuery,
}

/// Receives the results of [`PointsReader::intersect`].
pub trait PointVisitor {
    /// Classifies a cell given the min and max packed values of its index dimensions.
    fn compare(&mut self, min_packed: &[u8], max_packed: &[u8]) -> Relation;
    /// A doc whose point lies in a cell inside the query (no value check needed).
    fn visit(&mut self, doc: u32);
    /// A doc whose point (all data dimensions) must be checked against the query.
    fn visit_value(&mut self, doc: u32, packed: &[u8]);
    /// Bulk form of [`visit`](Self::visit).
    fn visit_docs(&mut self, docs: &[u32]) {
        for &d in docs {
            self.visit(d);
        }
    }
    /// The docs `start..end`, all inside the query (leaves whose docs are consecutive: time
    /// series in time order, index-sorted fields). Defaults to [`visit`](Self::visit) per doc;
    /// bit set collectors set the range at once.
    fn visit_doc_range(&mut self, start: u32, end: u32) {
        for d in start..end {
            self.visit(d);
        }
    }
    /// Several docs sharing one value that must be checked (low-cardinality fields): check the
    /// value once. Defaults to [`visit_value`](Self::visit_value) per doc.
    fn visit_value_docs(&mut self, docs: &[u32], packed: &[u8]) {
        for &d in docs {
            self.visit_value(d, packed);
        }
    }
    /// Hint: about `count` more docs are coming.
    fn grow(&mut self, _count: usize) {}
}

/// Number of leaves of the subtree rooted at `node` in a complete tree with `leaves` leaves.
fn subtree_leaves(node: usize, leaves: usize) -> usize {
    // Walk the leftmost and rightmost descendants level by level.
    let (mut lo, mut hi) = (node, node);
    let mut count = 0;
    loop {
        if lo >= leaves {
            count += hi.min(2 * leaves - 1) + 1 - lo;
            return count;
        }
        if hi >= leaves {
            // Mixed depths: nodes in [lo, leaves) are inner, [leaves, hi] are leaves.
            count += hi + 1 - leaves;
            hi = leaves - 1;
        }
        lo *= 2;
        hi = 2 * hi + 1;
    }
}

/// Writes the BKD tree of one field. `values` holds `docs.len()` packed values back to back.
///
/// Output is append-only: leaves are encoded into a [`Spill`] while the tree is built (the
/// split values and leaf offsets, one entry per 512 points, stay in memory), then the header,
/// the inner nodes and the leaves follow in `out`. The point permutation the build sorts is a
/// [`SpillVec`], so a huge field's working set is page cache rather than heap.
pub(crate) fn write_points<O: Out + ?Sized>(
    spec: &PointsSpec,
    docs: &[u32],
    values: &[u8],
    spill: &SpillOptions,
    out: &mut O,
) -> Result<()> {
    write_points_impl(spec, docs, values, spill, out, true)
}

fn write_points_impl<O: Out + ?Sized>(
    spec: &PointsSpec,
    docs: &[u32],
    values: &[u8],
    spill: &SpillOptions,
    out: &mut O,
    fast: bool,
) -> Result<()> {
    spec.validate()?;
    let plen = spec.packed_len();
    let bpd = spec.bytes_per_dim as usize;
    let n = docs.len();
    debug_assert_eq!(values.len(), n * plen);
    if n > u32::MAX as usize {
        return Err(crate::error::invalid("too many points in one field"));
    }
    let leaves = n.div_ceil(MAX_POINTS_IN_LEAF).max(1);
    // Distinct docs, with a bitset rather than a sorted copy (multi-valued docs repeat).
    let doc_count = {
        let max = docs.iter().copied().max().map_or(0, |m| m as usize + 1);
        let mut seen = vec![0u64; max.div_ceil(64)];
        let mut count = 0u32;
        for &d in docs {
            let w = &mut seen[d as usize >> 6];
            count += (*w >> (d & 63) & 1 == 0) as u32;
            *w |= 1 << (d & 63);
        }
        count
    };
    let mut global_min = vec![0u8; plen];
    let mut global_max = vec![0u8; plen];
    if n > 0 {
        global_min.copy_from_slice(&values[..plen]);
        global_max.copy_from_slice(&values[..plen]);
        for i in 1..n {
            let v = &values[i * plen..(i + 1) * plen];
            for d in 0..spec.dims as usize {
                let r = d * bpd..(d + 1) * bpd;
                if v[r.clone()] < global_min[r.clone()] {
                    global_min[r.clone()].copy_from_slice(&v[r.clone()]);
                }
                if v[r.clone()] > global_max[r.clone()] {
                    global_max[r.clone()].copy_from_slice(&v[r.clone()]);
                }
            }
        }
    }

    let mut split_dims = vec![0u8; leaves];
    let mut split_vals = vec![0u8; leaves * bpd];
    let mut offsets = vec![0u64; leaves];
    let mut leaf_data = Spill::new(spill);
    let mut leaf_out = SinkOut::new(&mut leaf_data);
    if n == 0 {
        // An empty leaf keeps readers uniform.
        leaf_out.put_vu32(0);
    } else if fast && plen <= 12 {
        let mut keys = SpillVec::<u128>::new(spill);
        for (i, &d) in docs.iter().enumerate() {
            let mut be = [0u8; 16];
            be[16 - plen..].copy_from_slice(&values[i * plen..(i + 1) * plen]);
            keys.push((u128::from_be_bytes(be) << 32) | d as u128)?;
        }
        let mut keys = keys.finish()?;
        let sorted = spec.dims == 1;
        if sorted {
            keys.sort_unstable();
        }
        let mut b = KeyBuilder {
            spec,
            leaves,
            split_dims: &mut split_dims,
            split_vals: &mut split_vals,
            offsets: &mut offsets,
            leaf_docs: Vec::with_capacity(MAX_POINTS_IN_LEAF),
            leaf_values: Vec::with_capacity(MAX_POINTS_IN_LEAF * plen),
            out: &mut leaf_out,
        };
        b.build(1, &mut keys, sorted);
    } else {
        let mut order = SpillVec::<u32>::new(spill);
        for i in 0..n as u32 {
            order.push(i)?;
        }
        let mut order = order.finish()?;
        let scratch = if bpd <= 8 { Some(SpillSlice::<u128>::zeroed(n, spill)?) } else { None };
        let mut b = Builder {
            scratch,
            spec,
            plen,
            bpd,
            docs,
            values,
            leaves,
            split_dims,
            split_vals,
            offsets,
            leaf_docs: Vec::with_capacity(MAX_POINTS_IN_LEAF),
            leaf_values: Vec::with_capacity(MAX_POINTS_IN_LEAF * plen),
            out: leaf_out,
        };
        b.build(1, &mut order[..]);
        (split_dims, split_vals, offsets, leaf_out) = (b.split_dims, b.split_vals, b.offsets, b.out);
    }
    leaf_out.finish()?;

    out.put_u8(spec.dims);
    out.put_u8(spec.index_dims);
    out.put_u8(spec.bytes_per_dim);
    out.put_u64(n as u64);
    out.put_u32(doc_count);
    out.put_u32(leaves as u32);
    out.put_bytes(&global_min);
    out.put_bytes(&global_max);
    out.put_bytes(&split_dims);
    out.put_bytes(&split_vals);
    for o in &offsets {
        out.put_u64(*o);
    }
    leaf_data.copy_with(&mut |chunk| {
        out.put_bytes(chunk);
        Ok(())
    })
}

struct Builder<'a> {
    spec: &'a PointsSpec,
    plen: usize,
    bpd: usize,
    docs: &'a [u32],
    values: &'a [u8],
    leaves: usize,
    split_dims: Vec<u8>,
    split_vals: Vec<u8>,
    /// Offset of each leaf in the leaf data.
    offsets: Vec<u64>,
    leaf_docs: Vec<u32>,
    leaf_values: Vec<u8>,
    /// Selection keys for values of at most 8 bytes (one per point).
    scratch: Option<SpillSlice<u128>>,
    out: SinkOut<'a>,
}

impl Builder<'_> {
    fn val(&self, i: u32) -> &[u8] {
        &self.values[i as usize * self.plen..(i as usize + 1) * self.plen]
    }

    fn dim(&self, i: u32, d: usize) -> &[u8] {
        &self.val(i)[d * self.bpd..(d + 1) * self.bpd]
    }

    fn build(&mut self, node: usize, pts: &mut [u32]) {
        if node >= self.leaves {
            self.write_leaf(node, pts);
            return;
        }
        let ll = subtree_leaves(2 * node, self.leaves);
        let tl = subtree_leaves(node, self.leaves);
        let left = pts.len() * ll / tl;
        let (bpd, plen, values, docs) = (self.bpd, self.plen, self.values, self.docs);
        let best = match self.scratch.take() {
            Some(mut scratch) => {
                // Values of at most 8 bytes compare as integers: spans and the median selection
                // run over contiguous (value, doc, index) keys instead of indirect slices.
                let load = |i: u32, d: usize| {
                    let mut b = [0u8; 8];
                    b[8 - bpd..].copy_from_slice(&values[i as usize * plen + d * bpd..][..bpd]);
                    u64::from_be_bytes(b)
                };
                let mut best = 0usize;
                let mut best_span = 0u64;
                for d in 0..self.spec.index_dims as usize {
                    let (lo, hi) = pts.iter().fold((u64::MAX, 0u64), |(lo, hi), &p| {
                        let v = load(p, d);
                        (lo.min(v), hi.max(v))
                    });
                    let span = hi - lo;
                    if d == 0 || span > best_span {
                        best = d;
                        best_span = span;
                    }
                }
                if left > 0 && left < pts.len() {
                    let keys = &mut scratch[..pts.len()];
                    for (k, &p) in keys.iter_mut().zip(pts.iter()) {
                        *k = ((load(p, best) as u128) << 64) | ((docs[p as usize] as u128) << 32) | p as u128;
                    }
                    keys.select_nth_unstable(left);
                    for (p, &k) in pts.iter_mut().zip(keys.iter()) {
                        *p = k as u32;
                    }
                }
                self.scratch = Some(scratch);
                best
            }
            None => {
                // Split dimension: largest spread among index dims.
                let mut best = 0usize;
                let mut best_span = vec![0u8; bpd];
                for d in 0..self.spec.index_dims as usize {
                    let mut lo = self.dim(pts[0], d);
                    let mut hi = lo;
                    for &p in pts.iter() {
                        let v = self.dim(p, d);
                        if v < lo {
                            lo = v;
                        }
                        if v > hi {
                            hi = v;
                        }
                    }
                    let span = sub_be(hi, lo);
                    if d == 0 || span > best_span {
                        best = d;
                        best_span = span;
                    }
                }
                let key = |i: u32| &values[i as usize * plen + best * bpd..i as usize * plen + (best + 1) * bpd];
                if left > 0 && left < pts.len() {
                    pts.select_nth_unstable_by(left, |&a, &b| {
                        key(a).cmp(key(b)).then(docs[a as usize].cmp(&docs[b as usize]))
                    });
                }
                best
            }
        };
        let split_at = left.min(pts.len() - 1);
        self.split_dims[node] = best as u8;
        let p = pts[split_at] as usize;
        self.split_vals[node * bpd..(node + 1) * bpd]
            .copy_from_slice(&values[p * plen + best * bpd..p * plen + (best + 1) * bpd]);
        let (l, r) = pts.split_at_mut(left);
        self.build(2 * node, l);
        self.build(2 * node + 1, r);
    }

    fn write_leaf(&mut self, node: usize, pts: &mut [u32]) {
        let (plen, values, docs) = (self.plen, self.values, self.docs);
        self.leaf_docs.clear();
        self.leaf_values.clear();
        if plen <= 12 {
            // Whole values (≤ 12 bytes) and the doc pack into one integer key per point.
            let mut keys = [0u128; MAX_POINTS_IN_LEAF];
            let keys = &mut keys[..pts.len()];
            for (k, &p) in keys.iter_mut().zip(pts.iter()) {
                let mut be = [0u8; 16];
                be[16 - plen..].copy_from_slice(&values[p as usize * plen..(p as usize + 1) * plen]);
                *k = (u128::from_be_bytes(be) << 32) | docs[p as usize] as u128;
            }
            keys.sort_unstable();
            for &k in keys.iter() {
                self.leaf_docs.push(k as u32);
                self.leaf_values.extend_from_slice(&(k >> 32).to_be_bytes()[16 - plen..]);
            }
        } else {
            pts.sort_unstable_by(|&a, &b| {
                values[a as usize * plen..(a as usize + 1) * plen]
                    .cmp(&values[b as usize * plen..(b as usize + 1) * plen])
                    .then(docs[a as usize].cmp(&docs[b as usize]))
            });
            for &p in pts.iter() {
                self.leaf_docs.push(docs[p as usize]);
                self.leaf_values.extend_from_slice(&values[p as usize * plen..(p as usize + 1) * plen]);
            }
        }
        self.offsets[node - self.leaves] = self.out.len();
        encode_leaf(self.spec, &self.leaf_docs, &self.leaf_values, &mut self.out);
    }
}

/// Encodes one leaf: `docs` and their packed `values`, sorted by value then doc.
fn encode_leaf<O: Out + ?Sized>(spec: &PointsSpec, docs: &[u32], values: &[u8], out: &mut O) {
    let n = docs.len();
    out.put_vu32(n as u32);
    if n == 0 {
        return;
    }
    let bpd = spec.bytes_per_dim as usize;
    let plen = spec.packed_len();
    let dims = spec.dims as usize;
    let (min_doc, max_doc) = docs.iter().fold((u32::MAX, 0), |(lo, hi), &d| (lo.min(d), hi.max(d)));
    out.put_vu32(min_doc);
    if n > 1 && docs.windows(2).all(|w| w[1] == w[0] + 1) {
        // Docs in value order are consecutive (time series indexed in time order, index-sorted
        // fields): nothing to store, and inside cells visit them as one range.
        out.put_u8(DOCS_CONSECUTIVE);
    } else {
        let bits = direct::bits_for((max_doc - min_doc) as u64);
        out.put_u8(bits as u8);
        direct::write(docs.iter().map(|&d| (d - min_doc) as u64), bits, out);
    }
    let val = |i: usize, d: usize| &values[i * plen + d * bpd..i * plen + (d + 1) * bpd];
    let mut cps = [0usize; 16];
    for (d, cp_out) in cps.iter_mut().enumerate().take(dims) {
        let first = val(0, d);
        let mut cp = bpd;
        for i in 1..n {
            cp = cp.min(first.iter().zip(val(i, d)).take_while(|(a, b)| a == b).count());
        }
        out.put_u8(cp as u8);
        out.put_bytes(&first[..cp]);
        *cp_out = cp;
    }
    // Exact leaf bounds of index dims (suffixes).
    for (d, &cp) in cps.iter().enumerate().take(spec.index_dims as usize) {
        let mut lo: &[u8] = &val(0, d)[cp..];
        let mut hi = lo;
        for i in 1..n {
            let v = &val(i, d)[cp..];
            if v < lo {
                lo = v;
            }
            if v > hi {
                hi = v;
            }
        }
        out.put_bytes(lo);
        out.put_bytes(hi);
    }
    // Values: one suffix row per point, or — when equal values repeat enough (low cardinality:
    // status codes, years, enums) — one row per run of equal values with its length, which the
    // reader hands to the visitor as a whole.
    let row: usize = (0..dims).map(|d| bpd - cps[d]).sum();
    let mut runs = 0usize;
    let mut run_bytes = 0usize;
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && values[j * plen..(j + 1) * plen] == values[i * plen..(i + 1) * plen] {
            j += 1;
        }
        runs += 1;
        run_bytes += row + crate::io::vlen((j - i) as u64);
        i = j;
    }
    if run_bytes < n * row {
        out.put_u8(LEAF_RUNS);
        out.put_vu32(runs as u32);
        let mut i = 0;
        while i < n {
            let mut j = i + 1;
            while j < n && values[j * plen..(j + 1) * plen] == values[i * plen..(i + 1) * plen] {
                j += 1;
            }
            out.put_vu32((j - i) as u32);
            for (d, &cp) in cps.iter().enumerate().take(dims) {
                out.put_bytes(&val(i, d)[cp..]);
            }
            i = j;
        }
    } else {
        out.put_u8(LEAF_ROWS);
        for i in 0..n {
            for (d, &cp) in cps.iter().enumerate().take(dims) {
                out.put_bytes(&val(i, d)[cp..]);
            }
        }
    }
}

/// Leaf doc ids `min_doc..min_doc + n` in leaf order (instead of a bit width).
const DOCS_CONSECUTIVE: u8 = 0xFF;

/// Leaf value layouts: one suffix row per point, or runs of equal values.
const LEAF_ROWS: u8 = 0;
const LEAF_RUNS: u8 = 1;

/// The build for packed values of at most 12 bytes (numbers, dates, geo points, small ranges):
/// every point becomes one `u128` key (packed value big-endian above the doc), so the median
/// selections, spans and leaf sorts run over a contiguous integer array instead of chasing point
/// indices. One-dimensional fields sort the keys once and cut the tree from the sorted run; the
/// splits and leaves are exactly the ones the general build makes.
struct KeyBuilder<'a, O: Out + ?Sized> {
    spec: &'a PointsSpec,
    leaves: usize,
    split_dims: &'a mut [u8],
    split_vals: &'a mut [u8],
    offsets: &'a mut [u64],
    leaf_docs: Vec<u32>,
    leaf_values: Vec<u8>,
    out: &'a mut O,
}

impl<O: Out + ?Sized> KeyBuilder<'_, O> {
    /// Value of dimension `d` of a key (dimensions of multi-dimensional keys are at most 6
    /// bytes, so every dimension fits a `u64`).
    #[inline]
    fn dim(&self, key: u128, d: usize) -> u64 {
        debug_assert!(self.spec.bytes_per_dim <= 8);
        let bits = self.spec.bytes_per_dim as u32 * 8;
        let shift = 32 + (self.spec.dims as u32 - 1 - d as u32) * bits;
        ((key >> shift) as u64) & (u64::MAX >> (64 - bits))
    }

    fn build(&mut self, node: usize, keys: &mut [u128], sorted: bool) {
        let bpd = self.spec.bytes_per_dim as usize;
        let plen = self.spec.packed_len();
        let len = keys.len();
        if node >= self.leaves {
            if !sorted {
                keys.sort_unstable();
            }
            self.leaf_docs.clear();
            self.leaf_values.clear();
            for &k in keys.iter() {
                self.leaf_docs.push(k as u32);
                self.leaf_values.extend_from_slice(&(k >> 32).to_be_bytes()[16 - plen..]);
            }
            self.offsets[node - self.leaves] = self.out.out_len();
            encode_leaf(self.spec, &self.leaf_docs, &self.leaf_values, self.out);
            return;
        }
        let ll = subtree_leaves(2 * node, self.leaves);
        let tl = subtree_leaves(node, self.leaves);
        let left = len * ll / tl;
        let split_at = left.min(len - 1);
        if sorted {
            let v = (keys[split_at] >> 32).to_be_bytes();
            self.split_vals[node * bpd..(node + 1) * bpd].copy_from_slice(&v[16 - bpd..]);
        } else {
            let mut best = 0usize;
            let mut best_span = 0u64;
            for d in 0..self.spec.index_dims as usize {
                let (lo, hi) = keys.iter().fold((u64::MAX, 0u64), |(lo, hi), &k| {
                    let v = self.dim(k, d);
                    (lo.min(v), hi.max(v))
                });
                if d == 0 || hi - lo > best_span {
                    best = d;
                    best_span = hi - lo;
                }
            }
            if left > 0 && left < len {
                let bits = bpd as u32 * 8;
                let shift = 32 + (self.spec.dims as u32 - 1 - best as u32) * bits;
                let mask = u64::MAX >> (64 - bits);
                keys.select_nth_unstable_by_key(left, |&k| ((k >> shift) as u64) & mask);
            }
            self.split_dims[node] = best as u8;
            let v = self.dim(keys[split_at], best).to_be_bytes();
            self.split_vals[node * bpd..(node + 1) * bpd].copy_from_slice(&v[8 - bpd..]);
        }
        let (l, r) = keys.split_at_mut(left);
        self.build(2 * node, l, sorted);
        self.build(2 * node + 1, r, sorted);
    }
}

/// Big-endian unsigned `a - b` (a >= b).
fn sub_be(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; a.len()];
    let mut borrow = 0i32;
    for i in (0..a.len()).rev() {
        let mut d = a[i] as i32 - b[i] as i32 - borrow;
        borrow = 0;
        if d < 0 {
            d += 256;
            borrow = 1;
        }
        out[i] = d as u8;
    }
    out
}

/// Reads one field's BKD tree. Cheap to clone; `Send + Sync`.
#[derive(Clone, Debug)]
pub struct PointsReader {
    spec: PointsSpec,
    num_points: u64,
    doc_count: u32,
    leaves: usize,
    min: Vec<u8>,
    max: Vec<u8>,
    split_dims: Slice,
    split_vals: Slice,
    offsets: Slice,
    data: Slice,
}

/// Decoded leaf scratch space.
struct Leaf {
    docs: Vec<u32>,
    /// The docs are consecutive (`docs[i] == docs[0] + i`).
    consecutive: bool,
    values: Vec<u8>,
    min: Vec<u8>,
    max: Vec<u8>,
}

impl PointsReader {
    pub(crate) fn open(data: Slice, spec: &PointsSpec) -> Result<PointsReader> {
        let mut r = In::new(&data);
        let s = PointsSpec {
            dims: r.u8()?,
            index_dims: r.u8()?,
            bytes_per_dim: r.u8()?,
        };
        if s != *spec {
            return Err(corrupt("points spec mismatch"));
        }
        s.validate()?;
        let num_points = r.u64()?;
        let doc_count = r.u32()?;
        let leaves = r.u32()? as usize;
        let plen = s.packed_len();
        let bpd = s.bytes_per_dim as usize;
        if leaves == 0 || num_points.div_ceil(MAX_POINTS_IN_LEAF as u64).max(1) != leaves as u64 {
            return Err(corrupt("points leaf count mismatch"));
        }
        let min = r.bytes(plen)?.to_vec();
        let max = r.bytes(plen)?.to_vec();
        let p = r.pos() as u64;
        let split_dims = data
            .try_slice(p, leaves as u64)
            .ok_or_else(|| corrupt("points truncated"))?;
        let split_vals = data
            .try_slice(p + leaves as u64, (leaves * bpd) as u64)
            .ok_or_else(|| corrupt("points truncated"))?;
        let offsets = data
            .try_slice(p + (leaves * (1 + bpd)) as u64, (leaves * 8) as u64)
            .ok_or_else(|| corrupt("points truncated"))?;
        let data_at = p + (leaves * (9 + bpd)) as u64;
        let leaf_data = data.slice(data_at.min(data.len() as u64) as usize..data.len());
        for i in 1..leaves {
            if split_dims[i] >= s.index_dims {
                return Err(corrupt("bad split dimension"));
            }
        }
        for l in 0..leaves {
            let o = u64::from_le_bytes(offsets[l * 8..l * 8 + 8].try_into().unwrap());
            if o >= leaf_data.len() as u64 {
                return Err(corrupt("leaf offset out of range"));
            }
        }
        Ok(PointsReader {
            spec: s,
            num_points,
            doc_count,
            leaves,
            min,
            max,
            split_dims,
            split_vals,
            offsets,
            data: leaf_data,
        })
    }

    pub fn spec(&self) -> &PointsSpec {
        &self.spec
    }

    /// Total number of points (a doc may have several).
    pub fn size(&self) -> u64 {
        self.num_points
    }

    /// Number of docs with at least one point.
    pub fn doc_count(&self) -> u32 {
        self.doc_count
    }

    /// Smallest value per data dimension (packed).
    pub fn min_packed_value(&self) -> &[u8] {
        &self.min
    }

    pub fn max_packed_value(&self) -> &[u8] {
        &self.max
    }

    fn index_bounds(&self) -> (Vec<u8>, Vec<u8>) {
        let il = self.spec.index_dims as usize * self.spec.bytes_per_dim as usize;
        (self.min[..il].to_vec(), self.max[..il].to_vec())
    }

    /// Visits every point matching the visitor's query.
    pub fn intersect(&self, visitor: &mut dyn PointVisitor) {
        if self.num_points == 0 {
            return;
        }
        let (mut min, mut max) = self.index_bounds();
        let plen = self.spec.packed_len();
        let mut leaf = Leaf {
            docs: Vec::with_capacity(MAX_POINTS_IN_LEAF),
            consecutive: false,
            values: Vec::with_capacity(MAX_POINTS_IN_LEAF * plen),
            min: vec![0; plen],
            max: vec![0; plen],
        };
        self.recurse(
            1,
            self.num_points as usize,
            &mut min,
            &mut max,
            visitor,
            &mut leaf,
        );
    }

    fn recurse(
        &self,
        node: usize,
        count: usize,
        min: &mut [u8],
        max: &mut [u8],
        v: &mut dyn PointVisitor,
        leaf: &mut Leaf,
    ) {
        let rel = v.compare(min, max);
        match rel {
            Relation::CellOutsideQuery => {}
            Relation::CellInsideQuery => {
                v.grow(count);
                self.visit_all(node, leaf, v);
            }
            Relation::CellCrossesQuery => {
                if node >= self.leaves {
                    self.visit_leaf_crossing(node, leaf, v);
                    return;
                }
                let bpd = self.spec.bytes_per_dim as usize;
                let d = self.split_dims[node] as usize;
                let split = &self.split_vals[node * bpd..(node + 1) * bpd];
                let ll = subtree_leaves(2 * node, self.leaves);
                let tl = subtree_leaves(node, self.leaves);
                let left = count * ll / tl;
                let r = d * bpd..(d + 1) * bpd;
                // Narrow the cell in place and restore it afterwards (no allocation per node).
                let mut saved = [0u8; 32];
                saved[..bpd].copy_from_slice(&max[r.clone()]);
                max[r.clone()].copy_from_slice(split);
                self.recurse(2 * node, left, min, max, v, leaf);
                max[r.clone()].copy_from_slice(&saved[..bpd]);
                saved[..bpd].copy_from_slice(&min[r.clone()]);
                min[r.clone()].copy_from_slice(split);
                self.recurse(2 * node + 1, count - left, min, max, v, leaf);
                min[r].copy_from_slice(&saved[..bpd]);
            }
        }
    }

    fn visit_all(&self, node: usize, leaf: &mut Leaf, v: &mut dyn PointVisitor) {
        if node >= self.leaves {
            self.read_leaf_docs(node, leaf);
            Self::visit_docs_of(leaf, &leaf.docs, v);
        } else {
            self.visit_all(2 * node, leaf, v);
            self.visit_all(2 * node + 1, leaf, v);
        }
    }

    fn leaf_offset(&self, node: usize) -> usize {
        let l = node - self.leaves;
        u64::from_le_bytes(self.offsets[l * 8..l * 8 + 8].try_into().unwrap()) as usize
    }

    /// Decodes a leaf's doc ids; returns the position after them.
    fn read_leaf_docs(&self, node: usize, leaf: &mut Leaf) -> usize {
        let mut r = In::at(&self.data, self.leaf_offset(node));
        leaf.docs.clear();
        let n = r.vlen(MAX_POINTS_IN_LEAF).unwrap_or(0);
        if n == 0 {
            return r.pos();
        }
        let min_doc = r.vu32().unwrap_or(0);
        let bits = r.u8().unwrap_or(0);
        if bits == DOCS_CONSECUTIVE {
            leaf.consecutive = true;
            leaf.docs.extend((0..n as u32).map(|i| min_doc.wrapping_add(i)));
            return r.pos();
        }
        leaf.consecutive = false;
        let bits = bits as u32;
        let packed = r.rest();
        let docs = &mut leaf.docs;
        direct::for_each_range(packed, bits, 0, n, |d| docs.push(min_doc.wrapping_add(d as u32)));
        r.pos() + direct::byte_len(n as u64, bits)
    }

    /// Hands `docs` (a leaf's, or a run of them) to the visitor, as a range when the leaf's docs
    /// are consecutive.
    #[inline]
    fn visit_docs_of(leaf: &Leaf, docs: &[u32], v: &mut dyn PointVisitor) {
        match (leaf.consecutive, docs.first()) {
            (true, Some(&d)) => v.visit_doc_range(d, d.saturating_add(docs.len() as u32)),
            _ => v.visit_docs(docs),
        }
    }

    /// Visits a leaf whose cell crosses the query. Values are sorted (by their first dimension
    /// first), so the leaf is split into runs of [`SUB_BLOCK`] points whose first-dimension
    /// bounds come from their first and last value: runs outside the query are skipped and runs
    /// inside it are visited without decoding or checking values, which makes narrow range
    /// queries check a few dozen values per boundary leaf instead of all of them.
    fn visit_leaf_crossing(&self, node: usize, leaf: &mut Leaf, v: &mut dyn PointVisitor) {
        let mut p = self.read_leaf_docs(node, leaf);
        let n = leaf.docs.len();
        if n == 0 {
            return;
        }
        let bpd = self.spec.bytes_per_dim as usize;
        let dims = self.spec.dims as usize;
        let idims = self.spec.index_dims as usize;
        let plen = self.spec.packed_len();
        let data = &self.data[..];
        let mut prefix = [[0u8; 32]; 16];
        let mut cps = [0usize; 16];
        for d in 0..dims {
            let cp = (data.get(p).copied().unwrap_or(0) as usize).min(bpd);
            p += 1;
            prefix[d][..cp].copy_from_slice(data.get(p..p + cp).unwrap_or(&[0u8; 32][..cp]));
            p += cp;
            cps[d] = cp;
        }
        // Leaf bounds.
        for d in 0..idims {
            let cp = cps[d];
            let sl = bpd - cp;
            leaf.min[d * bpd..d * bpd + cp].copy_from_slice(&prefix[d][..cp]);
            leaf.max[d * bpd..d * bpd + cp].copy_from_slice(&prefix[d][..cp]);
            leaf.min[d * bpd + cp..(d + 1) * bpd]
                .copy_from_slice(data.get(p..p + sl).unwrap_or(&[0u8; 32][..sl]));
            p += sl;
            leaf.max[d * bpd + cp..(d + 1) * bpd]
                .copy_from_slice(data.get(p..p + sl).unwrap_or(&[0u8; 32][..sl]));
            p += sl;
        }
        let il = idims * bpd;
        match v.compare(&leaf.min[..il], &leaf.max[..il]) {
            Relation::CellOutsideQuery => return,
            Relation::CellInsideQuery => {
                Self::visit_docs_of(leaf, &leaf.docs, v);
                return;
            }
            Relation::CellCrossesQuery => {}
        }
        let row: usize = (0..dims).map(|d| bpd - cps[d]).sum();
        let layout = data.get(p).copied().unwrap_or(LEAF_ROWS);
        p += 1;
        let fill = |at: usize, dst: &mut [u8]| {
            let mut at = at;
            for d in 0..dims {
                let cp = cps[d];
                let sl = bpd - cp;
                let out = &mut dst[d * bpd..(d + 1) * bpd];
                out[..cp].copy_from_slice(&prefix[d][..cp]);
                match data.get(at..at + sl) {
                    Some(src) => out[cp..].copy_from_slice(src),
                    None => out[cp..].fill(0),
                }
                at += sl;
            }
        };
        if layout == LEAF_RUNS {
            // Each run of equal values is checked once.
            let mut r = In::at(data, p);
            let runs = r.vlen(n).unwrap_or(0);
            let mut i = 0usize;
            leaf.values.clear();
            leaf.values.resize(plen, 0);
            for _ in 0..runs {
                let len = r.vlen(n - i).unwrap_or(0);
                if len == 0 {
                    break;
                }
                fill(r.pos(), &mut leaf.values[..plen]);
                if r.skip(row).is_err() {
                    break;
                }
                let docs = &leaf.docs[i..i + len];
                if len == 1 {
                    v.visit_value(docs[0], &leaf.values[..plen]);
                } else {
                    v.visit_value_docs(docs, &leaf.values[..plen]);
                }
                i += len;
            }
            return;
        }
        // Suffixes are fixed-width rows, so any value decodes on its own.
        let decode = |i: usize, dst: &mut [u8]| fill(p + i * row, dst);
        leaf.values.clear();
        leaf.values.resize(n * plen, 0);
        let (mut save_lo, mut save_hi, mut last) = ([0u8; 32], [0u8; 32], [0u8; 512]);
        save_lo[..bpd].copy_from_slice(&leaf.min[..bpd]);
        save_hi[..bpd].copy_from_slice(&leaf.max[..bpd]);
        let mut start = 0;
        while start < n {
            let end = (start + SUB_BLOCK).min(n);
            if n > SUB_BLOCK {
                // Bounds of this run: the leaf's, narrowed on the first dimension.
                let first = &mut leaf.values[start * plen..(start + 1) * plen];
                decode(start, first);
                decode(end - 1, &mut last[..plen]);
                leaf.min[..bpd].copy_from_slice(&first[..bpd]);
                leaf.max[..bpd].copy_from_slice(&last[..bpd]);
                let rel = v.compare(&leaf.min[..il], &leaf.max[..il]);
                leaf.min[..bpd].copy_from_slice(&save_lo[..bpd]);
                leaf.max[..bpd].copy_from_slice(&save_hi[..bpd]);
                match rel {
                    Relation::CellOutsideQuery => {
                        start = end;
                        continue;
                    }
                    Relation::CellInsideQuery => {
                        Self::visit_docs_of(leaf, &leaf.docs[start..end], v);
                        start = end;
                        continue;
                    }
                    Relation::CellCrossesQuery => {}
                }
            }
            for i in start..end {
                decode(i, &mut leaf.values[i * plen..(i + 1) * plen]);
            }
            // Equal values are adjacent: hand runs over at once so the visitor compares each
            // distinct value only once.
            let mut i = start;
            while i < end {
                let val = &leaf.values[i * plen..(i + 1) * plen];
                let mut j = i + 1;
                while j < end && &leaf.values[j * plen..(j + 1) * plen] == val {
                    j += 1;
                }
                if j - i == 1 {
                    v.visit_value(leaf.docs[i], val);
                } else {
                    v.visit_value_docs(&leaf.docs[i..j], val);
                }
                i = j;
            }
            start = end;
        }
    }

    /// Estimated number of points matching the visitor's query (no leaf is read): full counts
    /// of inside cells plus half of crossing leaves, like Lucene.
    pub fn estimate_point_count(&self, visitor: &mut dyn PointVisitor) -> u64 {
        if self.num_points == 0 {
            return 0;
        }
        let (mut min, mut max) = self.index_bounds();
        self.estimate(1, self.num_points as usize, &mut min, &mut max, visitor)
    }

    fn estimate(
        &self,
        node: usize,
        count: usize,
        min: &mut [u8],
        max: &mut [u8],
        v: &mut dyn PointVisitor,
    ) -> u64 {
        match v.compare(min, max) {
            Relation::CellOutsideQuery => 0,
            Relation::CellInsideQuery => count as u64,
            Relation::CellCrossesQuery => {
                if node >= self.leaves {
                    return (count as u64).div_ceil(2);
                }
                let bpd = self.spec.bytes_per_dim as usize;
                let d = self.split_dims[node] as usize;
                let split = &self.split_vals[node * bpd..(node + 1) * bpd];
                let ll = subtree_leaves(2 * node, self.leaves);
                let tl = subtree_leaves(node, self.leaves);
                let left = count * ll / tl;
                let r = d * bpd..(d + 1) * bpd;
                let mut saved = [0u8; 32];
                saved[..bpd].copy_from_slice(&max[r.clone()]);
                max[r.clone()].copy_from_slice(split);
                let a = self.estimate(2 * node, left, min, max, v);
                max[r.clone()].copy_from_slice(&saved[..bpd]);
                saved[..bpd].copy_from_slice(&min[r.clone()]);
                min[r.clone()].copy_from_slice(split);
                let b = self.estimate(2 * node + 1, count - left, min, max, v);
                min[r].copy_from_slice(&saved[..bpd]);
                a + b
            }
        }
    }

    /// Estimated number of docs matching (Lucene's `PointValues.estimateDocCount`): assumes
    /// points are spread uniformly over docs when fields are multi-valued.
    pub fn estimate_doc_count(&self, visitor: &mut dyn PointVisitor) -> u64 {
        let points = self.estimate_point_count(visitor);
        let size = self.num_points as f64;
        let docs = self.doc_count as f64;
        if points >= self.num_points {
            return self.doc_count as u64;
        }
        if self.num_points == self.doc_count as u64 || points == 0 {
            return points;
        }
        let exp = size / docs;
        let base = (size - points as f64) / size;
        let est = (docs * (1.0 - base.powf(exp))) as u64;
        est.max(1)
    }

    /// Visits all `(doc, packed value)` pairs (merges).
    pub fn visit_all_values(&self, f: &mut dyn FnMut(u32, &[u8])) {
        struct All<'a>(&'a mut dyn FnMut(u32, &[u8]));
        impl PointVisitor for All<'_> {
            fn compare(&mut self, _: &[u8], _: &[u8]) -> Relation {
                Relation::CellCrossesQuery
            }
            fn visit(&mut self, _: u32) {}
            fn visit_value(&mut self, doc: u32, packed: &[u8]) {
                (self.0)(doc, packed)
            }
        }
        // Crossing every cell decodes every value; inside cells never occur.
        let mut v = All(f);
        if self.num_points == 0 {
            return;
        }
        let plen = self.spec.packed_len();
        let mut leaf = Leaf {
            docs: Vec::with_capacity(MAX_POINTS_IN_LEAF),
            consecutive: false,
            values: Vec::with_capacity(MAX_POINTS_IN_LEAF * plen),
            min: vec![0; plen],
            max: vec![0; plen],
        };
        for node in self.leaves..2 * self.leaves {
            self.visit_leaf_crossing(node, &mut leaf, &mut v);
        }
    }
}

/// A pull iterator over every `(doc, packed value)` of a [`PointsReader`], one leaf decoded at a
/// time (merges stream points instead of collecting them).
pub struct PointValuesIter<'a> {
    reader: &'a PointsReader,
    node: usize,
    leaf: Leaf,
    docs: Vec<u32>,
    values: Vec<u8>,
    i: usize,
}

impl PointValuesIter<'_> {
    /// Loads leaves until one has a point left; false at the end.
    fn fill(&mut self) -> bool {
        struct Collect<'b>(&'b mut Vec<u32>, &'b mut Vec<u8>);
        impl PointVisitor for Collect<'_> {
            fn compare(&mut self, _: &[u8], _: &[u8]) -> Relation {
                Relation::CellCrossesQuery
            }
            fn visit(&mut self, _: u32) {}
            fn visit_value(&mut self, doc: u32, packed: &[u8]) {
                self.0.push(doc);
                self.1.extend_from_slice(packed);
            }
        }
        let r = self.reader;
        while self.i >= self.docs.len() {
            if r.num_points == 0 || self.node >= 2 * r.leaves {
                return false;
            }
            self.docs.clear();
            self.values.clear();
            self.i = 0;
            r.visit_leaf_crossing(self.node, &mut self.leaf, &mut Collect(&mut self.docs, &mut self.values));
            self.node += 1;
        }
        true
    }

    /// The next point, in leaf order.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<(u32, &[u8])> {
        if !self.fill() {
            return None;
        }
        let plen = self.reader.spec.packed_len();
        let i = self.i;
        self.i += 1;
        Some((self.docs[i], &self.values[i * plen..(i + 1) * plen]))
    }

    /// The doc of the next point, without consuming it.
    pub fn peek_doc(&mut self) -> Option<u32> {
        self.fill().then(|| self.docs[self.i])
    }

    /// Skips the next point.
    pub fn skip(&mut self) {
        if self.fill() {
            self.i += 1;
        }
    }
}

impl PointsReader {
    /// Iterates every point (merges).
    pub fn values_iter(&self) -> PointValuesIter<'_> {
        let plen = self.spec.packed_len();
        PointValuesIter {
            reader: self,
            node: self.leaves,
            leaf: Leaf {
                docs: Vec::with_capacity(MAX_POINTS_IN_LEAF),
                consecutive: false,
                values: Vec::with_capacity(MAX_POINTS_IN_LEAF * plen),
                min: vec![0; plen],
                max: vec![0; plen],
            },
            docs: Vec::with_capacity(MAX_POINTS_IN_LEAF),
            values: Vec::with_capacity(MAX_POINTS_IN_LEAF * plen),
            i: 0,
        }
    }
}

/// Lucene `NumericUtils`-compatible sortable encodings of point values.
pub mod encode {
    /// `IntPoint`: big-endian with the sign bit flipped.
    pub fn i32_bytes(v: i32) -> [u8; 4] {
        ((v as u32) ^ 0x8000_0000).to_be_bytes()
    }
    pub fn i32_from(b: &[u8]) -> i32 {
        (u32::from_be_bytes(b[..4].try_into().unwrap()) ^ 0x8000_0000) as i32
    }
    /// `LongPoint`.
    pub fn i64_bytes(v: i64) -> [u8; 8] {
        ((v as u64) ^ 0x8000_0000_0000_0000).to_be_bytes()
    }
    pub fn i64_from(b: &[u8]) -> i64 {
        (u64::from_be_bytes(b[..8].try_into().unwrap()) ^ 0x8000_0000_0000_0000) as i64
    }
    /// `NumericUtils.floatToSortableInt` (NaNs canonicalized like `Float.floatToIntBits`).
    pub fn f32_sortable(v: f32) -> i32 {
        let bits = if v.is_nan() {
            0x7fc0_0000
        } else {
            v.to_bits() as i32
        };
        bits ^ ((bits >> 31) & 0x7fff_ffff)
    }
    pub fn sortable_f32(i: i32) -> f32 {
        f32::from_bits((i ^ ((i >> 31) & 0x7fff_ffff)) as u32)
    }
    /// `NumericUtils.doubleToSortableLong`.
    pub fn f64_sortable(v: f64) -> i64 {
        let bits = if v.is_nan() {
            0x7ff8_0000_0000_0000
        } else {
            v.to_bits() as i64
        };
        bits ^ ((bits >> 63) & 0x7fff_ffff_ffff_ffff)
    }
    pub fn sortable_f64(i: i64) -> f64 {
        f64::from_bits((i ^ ((i >> 63) & 0x7fff_ffff_ffff_ffff)) as u64)
    }
    /// `FloatPoint`.
    pub fn f32_bytes(v: f32) -> [u8; 4] {
        i32_bytes(f32_sortable(v))
    }
    /// `DoublePoint`.
    pub fn f64_bytes(v: f64) -> [u8; 8] {
        i64_bytes(f64_sortable(v))
    }
    /// `HalfFloatPoint`: sortable short of IEEE half-precision bits, sign-flipped big-endian.
    pub fn f16_bits_bytes(bits: u16) -> [u8; 2] {
        ((f16_sortable_bits(bits) as u16) ^ 0x8000).to_be_bytes()
    }

    /// `HalfFloatPoint.encodeDimension` of a float.
    pub fn f16_bytes(v: f32) -> [u8; 2] {
        f16_bits_bytes(f16_bits(v))
    }

    fn f16_sortable_bits(bits: u16) -> i16 {
        let s = bits as i16;
        s ^ ((s >> 15) & 0x7fff)
    }

    /// `HalfFloatPoint.halfFloatToSortableShort`: the value `half_float` doc values hold.
    pub fn f16_sortable(v: f32) -> i16 {
        f16_sortable_bits(f16_bits(v))
    }

    /// Inverse of [`f16_sortable`].
    pub fn sortable_f16(s: i16) -> f32 {
        f16_to_f32(f16_sortable_bits(s as u16) as u16)
    }

    /// Rounds `i >> shift` to nearest, ties to even.
    fn round_shift(i: u32, shift: u32) -> u32 {
        let i = i + (1 << (shift - 1));
        let i = i - ((i >> shift) & 1);
        i >> shift
    }

    /// IEEE half-precision bits of `v` the way Lucene's `HalfFloatPoint` computes them: round
    /// to nearest even, overflow to infinity, float denormals to zero, NaN canonicalized.
    pub fn f16_bits(v: f32) -> u16 {
        let bits = if v.is_nan() { 0x7fc0_0000 } else { v.to_bits() };
        let sign = bits >> 31;
        let mut exp = ((bits >> 23) & 0xff) as i32;
        let mut mantissa = bits & 0x7f_ffff;
        if exp == 0xff {
            exp = 0x1f;
            mantissa >>= 23 - 10;
        } else if exp == 0 {
            mantissa = 0;
        } else {
            exp = exp - 127 + 15;
            if exp >= 0x1f {
                exp = 0x1f;
                mantissa = 0;
            } else if exp <= 0 {
                let shift = 23 - 10 - exp + 1;
                if shift >= 32 {
                    exp = 0;
                    mantissa = 0;
                } else {
                    let m = round_shift(mantissa | 0x80_0000, shift as u32);
                    exp = (m >> 10) as i32;
                    mantissa = m & 0x3ff;
                }
            } else {
                let m = round_shift(((exp as u32) << 23) | mantissa, 23 - 10);
                exp = (m >> 10) as i32;
                mantissa = m & 0x3ff;
            }
        }
        ((sign << 15) | ((exp as u32) << 10) | mantissa) as u16
    }

    /// The float a half-precision bit pattern stands for.
    pub fn f16_to_f32(h: u16) -> f32 {
        let sign = (h as u32) >> 15;
        let mut exp = ((h >> 10) & 0x1f) as i32;
        let mut mantissa = (h & 0x3ff) as u32;
        if exp == 0x1f {
            exp = 0xff;
            mantissa <<= 23 - 10;
        } else if mantissa == 0 && exp == 0 {
            // zero
        } else {
            if exp == 0 {
                // A half denormal is a normal float.
                let shift = mantissa.leading_zeros() as i32 - (32 - 11);
                mantissa = (mantissa << shift) & 0x3ff;
                exp = exp - shift + 1;
            }
            exp = exp + 127 - 15;
            mantissa <<= 23 - 10;
        }
        f32::from_bits((sign << 31) | ((exp as u32) << 23) | mantissa)
    }

    // Lucene divides by the reciprocal of the scale (and decodes by multiplying with it); the
    // rounding differs from multiplying by the scale for some inputs, so keep its exact form.
    const LAT_SCALE: f64 = (1u64 << 32) as f64 / 180.0;
    const LAT_DECODE: f64 = 1.0 / LAT_SCALE;
    const LON_SCALE: f64 = (1u64 << 32) as f64 / 360.0;
    const LON_DECODE: f64 = 1.0 / LON_SCALE;

    fn clamp_max(v: f64, max: f64) -> f64 {
        if v == max { f64::from_bits(max.to_bits() - 1) } else { v }
    }

    /// `GeoEncodingUtils.encodeLatitude` (rounds toward -90). The caller validates the range.
    pub fn encode_latitude(lat: f64) -> i32 {
        (clamp_max(lat, 90.0) / LAT_DECODE).floor() as i32
    }
    /// `GeoEncodingUtils.encodeLatitudeCeil` (rounds toward +90).
    pub fn encode_latitude_ceil(lat: f64) -> i32 {
        (clamp_max(lat, 90.0) / LAT_DECODE).ceil() as i32
    }
    /// `GeoEncodingUtils.encodeLongitude` (rounds toward -180).
    pub fn encode_longitude(lon: f64) -> i32 {
        (clamp_max(lon, 180.0) / LON_DECODE).floor() as i32
    }
    /// `GeoEncodingUtils.encodeLongitudeCeil` (rounds toward +180).
    pub fn encode_longitude_ceil(lon: f64) -> i32 {
        (clamp_max(lon, 180.0) / LON_DECODE).ceil() as i32
    }
    /// `GeoEncodingUtils.decodeLatitude`.
    pub fn decode_latitude(enc: i32) -> f64 {
        enc as f64 * LAT_DECODE
    }
    /// `GeoEncodingUtils.decodeLongitude`.
    pub fn decode_longitude(enc: i32) -> f64 {
        enc as f64 * LON_DECODE
    }
    /// `LatLonPoint` packed value: latitude then longitude.
    pub fn lat_lon_bytes(lat: f64, lon: f64) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&i32_bytes(encode_latitude(lat)));
        b[4..].copy_from_slice(&i32_bytes(encode_longitude(lon)));
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    struct Range {
        lo: Vec<u8>,
        hi: Vec<u8>,
        bpd: usize,
        dims: usize,
        hits: Vec<u32>,
    }

    impl PointVisitor for Range {
        fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
            let mut inside = true;
            for d in 0..self.dims {
                let r = d * self.bpd..(d + 1) * self.bpd;
                if max[r.clone()] < self.lo[r.clone()] || min[r.clone()] > self.hi[r.clone()] {
                    return Relation::CellOutsideQuery;
                }
                if min[r.clone()] < self.lo[r.clone()] || max[r.clone()] > self.hi[r.clone()] {
                    inside = false;
                }
            }
            if inside {
                Relation::CellInsideQuery
            } else {
                Relation::CellCrossesQuery
            }
        }
        fn visit(&mut self, doc: u32) {
            self.hits.push(doc);
        }
        fn visit_value(&mut self, doc: u32, packed: &[u8]) {
            for d in 0..self.dims {
                let r = d * self.bpd..(d + 1) * self.bpd;
                if packed[r.clone()] < self.lo[r.clone()] || packed[r.clone()] > self.hi[r.clone()]
                {
                    return;
                }
            }
            self.hits.push(doc);
        }
    }

    fn enc(v: i32, bpd: u8) -> Vec<u8> {
        match bpd {
            2 => ((v + 32768) as u16).to_be_bytes().to_vec(),
            4 => encode::i32_bytes(v).to_vec(),
            _ => encode::i64_bytes(v as i64).to_vec(),
        }
    }

    #[test]
    fn subtree_leaf_counts() {
        for leaves in 1..200usize {
            assert_eq!(subtree_leaves(1, leaves), leaves);
            for node in 1..leaves {
                assert_eq!(
                    subtree_leaves(node, leaves),
                    subtree_leaves(2 * node, leaves) + subtree_leaves(2 * node + 1, leaves)
                );
            }
        }
    }

    #[test]
    fn range_queries_match_brute_force() {
        let mut rng = StdRng::seed_from_u64(9);
        for &(n, dims, bpd) in &[
            (0usize, 1u8, 4u8),
            (1, 1, 4),
            (511, 1, 8),
            (512, 1, 8),
            (513, 2, 4),
            (5000, 1, 8),
            (20_000, 2, 4),
            (3000, 3, 2),
        ] {
            let spec = PointsSpec::new(dims, bpd);
            let plen = spec.packed_len();
            let mut docs = Vec::new();
            let mut values = Vec::new();
            for i in 0..n {
                docs.push((i / 2) as u32); // multi-valued
                for _ in 0..dims {
                    let v: i32 = rng.gen_range(-1000..1000);
                    values.extend_from_slice(&enc(v, bpd));
                }
            }
            let mut out = Vec::new();
            write_points(&spec, &docs, &values, &SpillOptions::default(), &mut out).unwrap();
            let r = PointsReader::open(Slice::from(out), &spec).unwrap();
            assert_eq!(r.size(), n as u64);
            assert_eq!(r.doc_count() as usize, n.div_ceil(2));
            for _ in 0..30 {
                let mut lo = Vec::new();
                let mut hi = Vec::new();
                for _ in 0..dims {
                    let a: i32 = rng.gen_range(-1100..1100);
                    let b: i32 = a + rng.gen_range(0..800);
                    lo.extend_from_slice(&enc(a, bpd));
                    hi.extend_from_slice(&enc(b, bpd));
                }
                let mut want: Vec<u32> = (0..n)
                    .filter(|&i| {
                        let v = &values[i * plen..(i + 1) * plen];
                        (0..dims as usize).all(|d| {
                            let r = d * bpd as usize..(d + 1) * bpd as usize;
                            v[r.clone()] >= lo[r.clone()] && v[r.clone()] <= hi[r.clone()]
                        })
                    })
                    .map(|i| docs[i])
                    .collect();
                want.sort();
                let mut q = Range {
                    lo: lo.clone(),
                    hi: hi.clone(),
                    bpd: bpd as usize,
                    dims: dims as usize,
                    hits: Vec::new(),
                };
                r.intersect(&mut q);
                q.hits.sort();
                assert_eq!(q.hits, want);
                let est = r.estimate_point_count(&mut Range {
                    lo,
                    hi,
                    bpd: bpd as usize,
                    dims: dims as usize,
                    hits: Vec::new(),
                });
                assert!(
                    est as usize >= want.len() / 2 || want.len() < 1000,
                    "estimate {est} vs {}",
                    want.len()
                );
            }
            let mut all = Vec::new();
            r.visit_all_values(&mut |d, v| all.push((d, v.to_vec())));
            assert_eq!(all.len(), n);
            // The pull iterator yields the same points in the same order, and peek/skip agree.
            let mut it = r.values_iter();
            let mut k = 0;
            while let Some(d) = it.peek_doc() {
                assert_eq!(d, all[k].0);
                if k % 3 == 0 {
                    it.skip();
                } else {
                    let (d2, v) = it.next().unwrap();
                    assert_eq!((d2, v), (all[k].0, &all[k].1[..]));
                }
                k += 1;
            }
            assert_eq!(k, n);
            assert!(it.next().is_none());
        }
    }

    #[test]
    fn high_dimensional_and_data_only_dims() {
        let mut rng = StdRng::seed_from_u64(10);
        for &(n, dims, index_dims, bpd) in &[
            (3000usize, 4u8, 4u8, 4u8),
            (2500, 5, 5, 2),
            (4000, 7, 4, 4),
            (1500, 8, 8, 1),
            (1200, 16, 8, 1),
            (900, 1, 1, 32),
        ] {
            let spec = PointsSpec { dims, index_dims, bytes_per_dim: bpd };
            let plen = spec.packed_len();
            let bpd = bpd as usize;
            let docs: Vec<u32> = (0..n as u32).map(|i| i / 3).collect();
            // Low cardinality per dim so that runs of equal values occur.
            let values: Vec<u8> = (0..n * plen).map(|_| rng.gen_range(0..4u8) * 60).collect();
            let mut out = Vec::new();
            write_points(&spec, &docs, &values, &SpillOptions::default(), &mut out).unwrap();
            let r = PointsReader::open(Slice::from(out), &spec).unwrap();
            for _ in 0..20 {
                let mut lo = vec![0u8; index_dims as usize * bpd];
                let mut hi = vec![0u8; index_dims as usize * bpd];
                for d in 0..index_dims as usize {
                    let a: Vec<u8> = (0..bpd).map(|_| rng.gen_range(0..4u8) * 60).collect();
                    let b: Vec<u8> = (0..bpd).map(|_| rng.gen_range(0..4u8) * 60).collect();
                    let (a, b) = if a <= b { (a, b) } else { (b, a) };
                    lo[d * bpd..(d + 1) * bpd].copy_from_slice(&a);
                    hi[d * bpd..(d + 1) * bpd].copy_from_slice(&b);
                }
                let mut want: Vec<u32> = (0..n)
                    .filter(|&i| {
                        let v = &values[i * plen..(i + 1) * plen];
                        (0..index_dims as usize).all(|d| {
                            let r = d * bpd..(d + 1) * bpd;
                            v[r.clone()] >= lo[r.clone()] && v[r.clone()] <= hi[r.clone()]
                        })
                    })
                    .map(|i| docs[i])
                    .collect();
                want.sort();
                let mut q = Range { lo, hi, bpd, dims: index_dims as usize, hits: Vec::new() };
                r.intersect(&mut q);
                q.hits.sort();
                assert_eq!(q.hits, want, "dims {dims}/{index_dims} bpd {bpd}");
            }
            // Every data dimension (indexed or not) round trips.
            let mut all = Vec::new();
            r.visit_all_values(&mut |d, v| all.push((d, v.to_vec())));
            let mut expect: Vec<(u32, Vec<u8>)> =
                (0..n).map(|i| (docs[i], values[i * plen..(i + 1) * plen].to_vec())).collect();
            all.sort();
            expect.sort();
            assert_eq!(all, expect);
        }
    }

    /// `cargo test --release -p search-codec --lib bench_points -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_points() {
        let mut rng = StdRng::seed_from_u64(4);
        let n = 4_000_000usize;
        let spec = PointsSpec::new(1, 8);
        let docs: Vec<u32> = (0..n as u32).collect();
        let values: Vec<u8> = (0..n).flat_map(|_| encode::i64_bytes(rng.gen_range(0..1_000_000_000))).collect();
        let t = std::time::Instant::now();
        let mut out = Vec::new();
        write_points(&spec, &docs, &values, &SpillOptions::default(), &mut out).unwrap();
        println!("build: {:.0} ns/point, {:.2} bytes/point", t.elapsed().as_nanos() as f64 / n as f64, out.len() as f64 / n as f64);
        let spec2 = PointsSpec::new(2, 4);
        let values2: Vec<u8> = (0..n).flat_map(|_| encode::lat_lon_bytes(rng.gen_range(-90.0..90.0), rng.gen_range(-180.0..180.0))).collect();
        let t = std::time::Instant::now();
        let mut out2 = Vec::new();
        write_points(&spec2, &docs, &values2, &SpillOptions::default(), &mut out2).unwrap();
        println!("build 2-d: {:.0} ns/point, {:.2} bytes/point", t.elapsed().as_nanos() as f64 / n as f64, out2.len() as f64 / n as f64);
        let r2 = PointsReader::open(Slice::from(out2), &spec2).unwrap();
        let t = std::time::Instant::now();
        let mut hits = 0;
        for i in 0..50 {
            let lat = -80.0 + i as f64 * 3.0;
            let mut q = Range {
                lo: [encode::i32_bytes(encode::encode_latitude(lat)), encode::i32_bytes(encode::encode_longitude(-10.0))].concat(),
                hi: [encode::i32_bytes(encode::encode_latitude(lat + 1.0)), encode::i32_bytes(encode::encode_longitude(10.0))].concat(),
                bpd: 4,
                dims: 2,
                hits: Vec::new(),
            };
            r2.intersect(&mut q);
            hits += q.hits.len();
        }
        println!("2-d box 1x20 degrees: {:?}/query, {} hits/query", t.elapsed() / 50, hits / 50);
        let r = PointsReader::open(Slice::from(out), &spec).unwrap();
        for width in [1_000u64, 1_000_000, 100_000_000] {
            let t = std::time::Instant::now();
            let mut hits = 0usize;
            let rounds = 50;
            for i in 0..rounds {
                let lo = (i as i64 * 15_485_863) % 900_000_000;
                let mut q = Range {
                    lo: encode::i64_bytes(lo).to_vec(),
                    hi: encode::i64_bytes(lo + width as i64).to_vec(),
                    bpd: 8,
                    dims: 1,
                    hits: Vec::new(),
                };
                r.intersect(&mut q);
                hits += q.hits.len();
            }
            let e = t.elapsed() / rounds;
            println!("range width {width}: {e:?}/query, {} hits/query", hits / rounds as usize);
        }
    }

    #[test]
    fn half_float_matches_ieee_and_lucene_rounding() {
        use encode::*;
        assert_eq!(f16_bits(1.0), 0x3C00);
        assert_eq!(f16_bits(-2.0), 0xC000);
        assert_eq!(f16_bits(65504.0), 0x7BFF);
        assert_eq!(f16_bits(65520.0), 0x7C00); // rounds up to infinity
        assert_eq!(f16_bits(65519.0), 0x7BFF);
        assert_eq!(f16_bits(0.1), 0x2E66);
        assert_eq!(f16_bits(5.960_464_5e-8), 0x0001); // smallest denormal
        assert_eq!(f16_bits(2.980_232_2e-8), 0x0000); // exactly half of it: tie to even
        assert_eq!(f16_bits(f32::INFINITY), 0x7C00);
        assert_eq!(f16_bits(f32::NEG_INFINITY), 0xFC00);
        assert_eq!(f16_bits(f32::NAN), 0x7E00);
        assert_eq!(f16_bits(1e-40), 0); // float denormal
        assert_eq!(f16_bits(-0.0), 0x8000);
        // Every finite half round-trips, and sortable order follows numeric order.
        let mut prev: Option<(f32, i16)> = None;
        let mut halves: Vec<u16> = (0..=0xFFFFu16).filter(|h| h & 0x7C00 != 0x7C00 || h & 0x3FF == 0).collect();
        halves.sort_by(|a, b| f16_to_f32(*a).total_cmp(&f16_to_f32(*b)));
        for h in halves {
            let f = f16_to_f32(h);
            assert_eq!(f16_bits(f), h, "{h:#x}");
            let s = f16_sortable(f);
            assert_eq!(sortable_f16(s).to_bits(), f.to_bits());
            if let Some((pf, ps)) = prev {
                assert!(ps < s || (pf == f && ps <= s), "{pf} {f}");
                assert!(f16_bytes(pf) <= f16_bytes(f));
            }
            prev = Some((f, s));
        }
        // Ties round to even.
        assert_eq!(f16_bits(f16_to_f32(0x3C00) + f16_to_f32(0x1400) / 2.0), 0x3C00);
    }

    #[test]
    fn sortable_encodings() {
        let vals = [
            f64::NEG_INFINITY,
            -1e300,
            -1.0,
            -0.0,
            0.0,
            1e-300,
            1.0,
            f64::INFINITY,
        ];
        for w in vals.windows(2) {
            assert!(encode::f64_bytes(w[0]) < encode::f64_bytes(w[1]));
            assert_eq!(
                encode::sortable_f64(encode::f64_sortable(w[0])).to_bits(),
                w[0].to_bits()
            );
        }
        let ints = [i64::MIN, -5, 0, 5, i64::MAX];
        for w in ints.windows(2) {
            assert!(encode::i64_bytes(w[0]) < encode::i64_bytes(w[1]));
            assert_eq!(encode::i64_from(&encode::i64_bytes(w[0])), w[0]);
        }
        assert_eq!(encode::encode_latitude(90.0), i32::MAX);
        assert_eq!(encode::encode_longitude(-180.0), i32::MIN);
        // f16: -1.0 (0xBC00) < 0 (0x0000) < 1.0 (0x3C00)
        assert!(encode::f16_bits_bytes(0xBC00) < encode::f16_bits_bytes(0x0000));
        assert!(encode::f16_bits_bytes(0x0000) < encode::f16_bits_bytes(0x3C00));
    }

    /// The sorted one-dimensional build writes exactly what the general median-selection build
    /// writes (duplicates, multi-valued docs, every value width it handles).
    #[test]
    fn sorted_build_equals_general_build() {
        let mut rng = StdRng::seed_from_u64(17);
        for &(n, bpd, distinct) in &[
            (1usize, 4u8, 10u64),
            (511, 8, 1_000_000),
            (512, 8, 3),
            (513, 2, 50),
            (5000, 8, 1_000_000_000),
            (5000, 4, 1),
            (20_000, 12, u64::MAX),
            (3001, 1, 200),
            (70_000, 8, 5000),
        ] {
            let spec = PointsSpec::new(1, bpd);
            let mut docs = Vec::new();
            let mut values = Vec::new();
            for i in 0..n {
                docs.push((i as u32 / 3) ^ 0x55);
                let v: u64 = rng.gen_range(0..distinct.max(1));
                let mut be = [0u8; 16];
                be[8..].copy_from_slice(&v.to_be_bytes());
                if bpd > 8 {
                    rng.fill(&mut be[..8]);
                }
                values.extend_from_slice(&be[16 - bpd as usize..]);
            }
            let (mut a, mut b) = (Vec::new(), Vec::new());
            write_points_impl(&spec, &docs, &values, &SpillOptions::default(), &mut a, true).unwrap();
            write_points_impl(&spec, &docs, &values, &SpillOptions::default(), &mut b, false).unwrap();
            assert!(a == b, "n {n} bpd {bpd}");
        }
    }

    /// Values increasing with doc ids (time series in time order) store no doc ids, and inside
    /// cells arrive as doc ranges.
    #[test]
    fn consecutive_leaf_docs_visit_as_ranges() {
        struct Ranges {
            q: Range,
            ranges: usize,
        }
        impl PointVisitor for Ranges {
            fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
                self.q.compare(min, max)
            }
            fn visit(&mut self, doc: u32) {
                self.q.visit(doc)
            }
            fn visit_value(&mut self, doc: u32, packed: &[u8]) {
                self.q.visit_value(doc, packed)
            }
            fn visit_doc_range(&mut self, start: u32, end: u32) {
                self.ranges += 1;
                for d in start..end {
                    self.q.visit(d);
                }
            }
        }
        let n = 100_000usize;
        let spec = PointsSpec::new(1, 8);
        let docs: Vec<u32> = (0..n as u32).collect();
        let ts: Vec<i64> = (0..n as i64).map(|i| 1_700_000_000_000 + i * 1000 + (i % 7)).collect();
        let values: Vec<u8> = ts.iter().flat_map(|&v| encode::i64_bytes(v)).collect();
        let mut out = Vec::new();
        write_points(&spec, &docs, &values, &SpillOptions::default(), &mut out).unwrap();
        assert!(out.len() < n * 4, "{} bytes", out.len());
        let r = PointsReader::open(Slice::from(out), &spec).unwrap();
        let (lo, hi) = (ts[1234], ts[77_777]);
        let mut v = Ranges {
            q: Range { lo: encode::i64_bytes(lo).to_vec(), hi: encode::i64_bytes(hi).to_vec(), bpd: 8, dims: 1, hits: Vec::new() },
            ranges: 0,
        };
        r.intersect(&mut v);
        v.q.hits.sort_unstable();
        assert_eq!(v.q.hits, (1234..=77_777).collect::<Vec<u32>>());
        assert!(v.ranges > 100, "{} ranges", v.ranges);
    }

    /// Low-cardinality fields store each leaf's runs of equal values once, and queries still
    /// match exactly.
    #[test]
    fn low_cardinality_leaves_use_runs() {
        let mut rng = StdRng::seed_from_u64(23);
        let codes = [200i32, 201, 301, 404, 500, 503];
        let n = 50_000usize;
        let spec = PointsSpec::new(1, 4);
        let docs: Vec<u32> = (0..n as u32).collect();
        let vals: Vec<i32> = (0..n).map(|_| codes[rng.gen_range(0..codes.len())]).collect();
        let values: Vec<u8> = vals.iter().flat_map(|&v| encode::i32_bytes(v)).collect();
        let mut out = Vec::new();
        write_points(&spec, &docs, &values, &SpillOptions::default(), &mut out).unwrap();
        // Doc ids dominate: well under the 4 value bytes per point of the raw rows.
        assert!(out.len() < n * 3, "{} bytes for {n} points", out.len());
        let r = PointsReader::open(Slice::from(out), &spec).unwrap();
        for (lo, hi) in [(200, 200), (201, 404), (0, 199), (300, 600), (404, 404)] {
            let mut q = Range { lo: enc(lo, 4), hi: enc(hi, 4), bpd: 4, dims: 1, hits: Vec::new() };
            r.intersect(&mut q);
            q.hits.sort_unstable();
            let want: Vec<u32> = (0..n as u32).filter(|&d| (lo..=hi).contains(&vals[d as usize])).collect();
            assert_eq!(q.hits, want, "[{lo}, {hi}]");
        }
        let mut all = 0;
        r.visit_all_values(&mut |_, _| all += 1);
        assert_eq!(all, n);
    }

    #[test]
    fn corrupt_is_error() {
        let spec = PointsSpec::new(1, 4);
        let docs: Vec<u32> = (0..2000).collect();
        let values: Vec<u8> = (0..2000i32).flat_map(encode::i32_bytes).collect();
        let mut out = Vec::new();
        write_points(&spec, &docs, &values, &SpillOptions::default(), &mut out).unwrap();
        for n in [0usize, 10, 40] {
            assert!(PointsReader::open(Slice::from(out[..n].to_vec()), &spec).is_err());
        }
        assert!(PointsReader::open(Slice::from(out.clone()), &PointsSpec::new(2, 4)).is_err());
    }
}
