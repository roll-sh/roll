//! Recursive graph bisection ("BP") doc reordering, as in Lucene's `BPIndexReorderer`
//! (Dhulipala et al., *Compressing Graphs and Indexes with Recursive Graph Bisection*, with the
//! simulated-annealing stop rule of Mackenzie et al., *Tradeoff Options for Bipartite Graph
//! Partitioning*).
//!
//! Docs that share terms are moved next to each other, which shrinks doc-id gaps (smaller
//! postings) and clusters matches into fewer blocks (better block-max pruning) — the biggest
//! single win measured by search benchmarks after block-max scoring. It runs when merging
//! segments without an index sort:
//!
//! 1. Build a forward index (unit → ids of its terms) over the inverted fields, keeping terms
//!    whose doc freq is in `[min_doc_freq, max_doc_freq_ratio * max_doc]` (rare terms do not
//!    help and frequent ones cost the most). It is built term-major from the postings and then
//!    transposed, so peak memory is two `u32`s per kept posting; when that exceeds
//!    `ram_budget_bytes` the segment is left as is.
//! 2. Split the units in two halves. Each iteration computes every unit's bias
//!    `Σ log2(df_right(t)) - log2(df_left(t))` over its terms (negative: drawn to the left), and
//!    if the most right-leaning unit on the left and the most left-leaning unit on the right
//!    differ by more than the iteration number, moves the lowest-bias half to the left (a
//!    linear-time selection) and updates the two doc-freq tables for the units that changed side.
//! 3. Recurse into both halves (each sorted by doc id first, keeping locality for ties) until
//!    partitions are smaller than `min_partition_size`. The top levels run on separate threads,
//!    each with its own doc-freq tables, which are cleared by touching only the terms of the
//!    partition.
//!
//! A unit is one doc, or with `parents` (nested documents) one block: the child docs followed by
//! their parent. Blocks move as a whole, so block joins keep working after reordering.
//!
//! The result is a [`DocMap`] to apply with [`RemappedSource`](crate::RemappedSource).

use crate::NO_MORE_DOCS;
use crate::error::{Result, invalid};
use crate::source::{DocMap, SegmentSource};

/// BP parameters (Lucene's defaults).
#[derive(Clone, Debug)]
pub struct BpOptions {
    /// Ignore terms in fewer docs than this.
    pub min_doc_freq: u32,
    /// Ignore terms in more than this fraction of docs.
    pub max_doc_freq_ratio: f32,
    /// Stop recursing below this many units.
    pub min_partition_size: usize,
    /// Swap iterations per bisection.
    pub max_iters: usize,
    /// Fields to use (`None`: every inverted field).
    pub fields: Option<Vec<u32>>,
    /// Threads for the top levels of the recursion.
    pub threads: usize,
    /// Memory the forward index may use; larger segments are not reordered.
    pub ram_budget_bytes: usize,
    /// Nested documents: a bitset (`max_doc` bits, little-endian words) of parent docs. Each
    /// parent and the non-parent docs right before it form a block that is never split. Docs
    /// after the last parent form a final block.
    pub parents: Option<Vec<u64>>,
}

impl Default for BpOptions {
    fn default() -> BpOptions {
        BpOptions {
            min_doc_freq: 4096,
            max_doc_freq_ratio: 1.0,
            min_partition_size: 32,
            max_iters: 20,
            fields: None,
            threads: std::thread::available_parallelism().map_or(1, |n| n.get()).min(8),
            ram_budget_bytes: 512 << 20,
            parents: None,
        }
    }
}

/// Unit → term ids, in compressed sparse row form, plus the doc range of every unit.
struct Forward {
    offsets: Vec<u64>,
    terms: Vec<u32>,
    num_terms: usize,
    /// `unit_start[u]..unit_start[u + 1]` are the docs of unit `u`.
    unit_start: Vec<u32>,
}

impl Forward {
    #[inline]
    fn terms(&self, unit: u32) -> &[u32] {
        let u = unit as usize;
        &self.terms[self.offsets[u] as usize..self.offsets[u + 1] as usize]
    }

    fn num_units(&self) -> usize {
        self.unit_start.len() - 1
    }
}

/// Unit boundaries: one unit per doc, or one per parent block.
fn unit_starts(max_doc: u32, parents: Option<&[u64]>) -> Result<Vec<u32>> {
    let Some(bits) = parents else {
        return Ok((0..=max_doc).collect());
    };
    if bits.len() < (max_doc as usize).div_ceil(64) {
        return Err(invalid("parents bitset is shorter than max_doc"));
    }
    let mut starts = vec![0u32];
    for d in 0..max_doc {
        if bits[(d >> 6) as usize] >> (d & 63) & 1 != 0 {
            starts.push(d + 1);
        }
    }
    if *starts.last().unwrap() != max_doc {
        starts.push(max_doc);
    }
    Ok(starts)
}

/// `None` when the forward index would exceed the RAM budget.
fn build_forward(src: &dyn SegmentSource, opts: &BpOptions) -> Result<Option<Forward>> {
    let max_doc = src.max_doc();
    let unit_start = unit_starts(max_doc, opts.parents.as_deref())?;
    let units = unit_start.len() - 1;
    let mut unit_of = Vec::new();
    if units != max_doc as usize {
        unit_of = vec![0u32; max_doc as usize];
        for u in 0..units {
            unit_of[unit_start[u] as usize..unit_start[u + 1] as usize].fill(u as u32);
        }
    }
    let max_df = ((opts.max_doc_freq_ratio as f64 * max_doc as f64) as u64).max(1);
    // Term-major lists of units.
    let mut flat: Vec<u32> = Vec::new();
    let mut term_ends: Vec<u64> = Vec::new();
    let budget_postings = opts.ram_budget_bytes / 8;
    for f in src.field_infos() {
        if !f.index.is_indexed() || opts.fields.as_ref().is_some_and(|fs| !fs.contains(&f.number)) {
            continue;
        }
        let Some(mut terms) = src.terms(f.number)? else { continue };
        while terms.next_term()?.is_some() {
            let p = terms.postings()?;
            let start = flat.len();
            loop {
                let d = p.next_doc();
                if d == NO_MORE_DOCS {
                    break;
                }
                if d >= max_doc {
                    return Err(invalid(format!("postings doc {d} >= max_doc {max_doc}")));
                }
                flat.push(if unit_of.is_empty() { d } else { unit_of[d as usize] });
            }
            let df = (flat.len() - start) as u64;
            if df >= opts.min_doc_freq as u64 && df <= max_df {
                term_ends.push(flat.len() as u64);
                if flat.len() > budget_postings {
                    return Ok(None);
                }
            } else {
                flat.truncate(start);
            }
        }
    }
    let num_terms = term_ends.len();
    let mut offsets = vec![0u64; units + 1];
    for &u in &flat {
        offsets[u as usize + 1] += 1;
    }
    for i in 0..units {
        offsets[i + 1] += offsets[i];
    }
    let mut fill = offsets.clone();
    let mut terms = vec![0u32; flat.len()];
    let mut start = 0usize;
    for (t, &end) in term_ends.iter().enumerate() {
        for &u in &flat[start..end as usize] {
            let at = &mut fill[u as usize];
            terms[*at as usize] = t as u32;
            *at += 1;
        }
        start = end as usize;
    }
    Ok(Some(Forward { offsets, terms, num_terms, unit_start }))
}

/// `log2` with 8 bits of mantissa precision (Lucene's `fastLog2`); `log2(0)` is taken as 0.
struct Log2 {
    table: [f32; 256],
}

impl Log2 {
    fn new() -> Log2 {
        let mut table = [0f32; 256];
        for (i, t) in table.iter_mut().enumerate() {
            *t = (1.0 + i as f64 / 256.0).log2() as f32;
        }
        Log2 { table }
    }

    #[inline]
    fn get(&self, i: u32) -> f32 {
        if i == 0 {
            return 0.0;
        }
        let floor = 31 - i.leading_zeros();
        let idx = ((i << (31 - floor)) << 1 >> 24) as usize;
        floor as f32 + self.table[idx]
    }
}

/// Computes a BP reordering of `src`'s docs; `None` when there is nothing to gain (too few docs,
/// no qualifying terms, or a forward index over the RAM budget).
pub fn bp_reorder(src: &dyn SegmentSource, opts: &BpOptions) -> Result<Option<DocMap>> {
    let max_doc = src.max_doc();
    let min_part = opts.min_partition_size.max(1);
    if (max_doc as usize) < min_part * 2 {
        return Ok(None);
    }
    let Some(fwd) = build_forward(src, opts)? else {
        return Ok(None);
    };
    if fwd.num_terms == 0 || fwd.num_units() < min_part * 2 {
        return Ok(None);
    }
    let mut units: Vec<u32> = (0..fwd.num_units() as u32).collect();
    let depth_threads = opts.threads.max(1).next_power_of_two().trailing_zeros() as usize;
    let ctx = Ctx { fwd: &fwd, opts, min_part, log2: Log2::new() };
    let mut bufs = Bufs::new(fwd.num_terms);
    ctx.bisect(&mut units, &mut bufs, depth_threads);
    if units.iter().enumerate().all(|(i, &u)| i as u32 == u) {
        return Ok(None);
    }
    let mut new_to_old = Vec::with_capacity(max_doc as usize);
    for &u in &units {
        new_to_old.extend(fwd.unit_start[u as usize]..fwd.unit_start[u as usize + 1]);
    }
    Ok(Some(DocMap::from_new_to_old(new_to_old, max_doc)?))
}

/// Per-thread scratch: doc freqs of both halves and the bias array.
struct Bufs {
    left: Vec<u32>,
    right: Vec<u32>,
    /// (bias, unit, position before the move)
    biases: Vec<(f32, u32, u32)>,
}

impl Bufs {
    fn new(num_terms: usize) -> Bufs {
        Bufs { left: vec![0; num_terms], right: vec![0; num_terms], biases: Vec::new() }
    }
}

struct Ctx<'a> {
    fwd: &'a Forward,
    opts: &'a BpOptions,
    min_part: usize,
    log2: Log2,
}

impl Ctx<'_> {
    fn bisect(&self, units: &mut [u32], bufs: &mut Bufs, parallel_levels: usize) {
        if units.len() < self.min_part * 2 {
            return;
        }
        self.partition(units, bufs);
        let mid = units.len() / 2;
        let (l, r) = units.split_at_mut(mid);
        l.sort_unstable();
        r.sort_unstable();
        if parallel_levels > 0 && l.len() > 4096 {
            std::thread::scope(|s| {
                s.spawn(|| {
                    let mut own = Bufs::new(self.fwd.num_terms);
                    self.bisect(l, &mut own, parallel_levels - 1);
                });
                self.bisect(r, bufs, parallel_levels - 1);
            });
        } else {
            self.bisect(l, bufs, 0);
            self.bisect(r, bufs, 0);
        }
    }

    /// Moves units between the two halves of `units` to minimize the log-gap cost.
    fn partition(&self, units: &mut [u32], bufs: &mut Bufs) {
        let fwd = self.fwd;
        let mid = units.len() / 2;
        let Bufs { left, right, biases } = bufs;
        for &u in &units[..mid] {
            for &t in fwd.terms(u) {
                left[t as usize] += 1;
            }
        }
        for &u in &units[mid..] {
            for &t in fwd.terms(u) {
                right[t as usize] += 1;
            }
        }
        for iter in 0..self.opts.max_iters {
            biases.clear();
            let (mut max_left, mut min_right) = (f32::NEG_INFINITY, f32::INFINITY);
            for (i, &u) in units.iter().enumerate() {
                let mut b = 0f32;
                for &t in fwd.terms(u) {
                    b += self.log2.get(right[t as usize]) - self.log2.get(left[t as usize]);
                }
                if i < mid {
                    max_left = max_left.max(b);
                } else {
                    min_right = min_right.min(b);
                }
                biases.push((b, u, i as u32));
            }
            if max_left - min_right <= iter as f32 {
                break;
            }
            biases.select_nth_unstable_by(mid, |a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            for (i, &(_, u, was)) in biases.iter().enumerate() {
                let (now_left, was_left) = (i < mid, (was as usize) < mid);
                if now_left != was_left {
                    let (from, to) = if was_left { (&mut *left, &mut *right) } else { (&mut *right, &mut *left) };
                    for &t in fwd.terms(u) {
                        from[t as usize] -= 1;
                        to[t as usize] += 1;
                    }
                }
                units[i] = u;
            }
        }
        // Clear only what this partition touched.
        for &u in units.iter() {
            for &t in fwd.terms(u) {
                left[t as usize] = 0;
                right[t as usize] = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_log2_is_close() {
        let l = Log2::new();
        assert_eq!(l.get(0), 0.0);
        assert_eq!(l.get(1), 0.0);
        for i in [2u32, 3, 7, 100, 4097, 1 << 20, u32::MAX] {
            let want = (i as f64).log2() as f32;
            assert!((l.get(i) - want).abs() < 0.01, "{i}: {} vs {want}", l.get(i));
        }
        for i in 1..5000u32 {
            assert!(l.get(i + 1) >= l.get(i));
        }
    }

    #[test]
    fn units_follow_parent_blocks() {
        let mut bits = vec![0u64; 1];
        for p in [2u32, 3, 7] {
            bits[0] |= 1 << p;
        }
        assert_eq!(unit_starts(10, Some(&bits)).unwrap(), vec![0, 3, 4, 8, 10]);
        assert_eq!(unit_starts(8, Some(&bits)).unwrap(), vec![0, 3, 4, 8]);
        assert_eq!(unit_starts(3, None).unwrap(), vec![0, 1, 2, 3]);
        assert!(unit_starts(100, Some(&bits)).is_err());
    }
}
