//! Join estimates as Postgres's planner makes them: join sizes (calc_joinrel_size_estimate
//! with eqjoinsel's MCV matching), the semi/anti-join factors, and the costs of nested
//! loop, hash and merge joins (costsize.c's initial/final_cost_* functions, cost_rescan,
//! cost_material, ExecChooseHashTableSize). The planner compares paths with them and EXPLAIN
//! shows the same numbers.

use crate::catalog::Catalog;
use crate::sql::costs::{self, CPU_OPERATOR_COST, CPU_TUPLE_COST, VarRef};
use crate::types::Value;
use crate::types::text::FormatCtx;

const BLCKSZ: f64 = 8192.0;

/// The planner's settings for joins.
#[derive(Clone, Copy, Debug)]
pub struct JoinGucs {
    pub nestloop: bool,
    pub hashjoin: bool,
    pub mergejoin: bool,
    pub material: bool,
    pub incremental_sort: bool,
    pub memoize: bool,
    pub seq_page_cost: f64,
    /// work_mem, in kB.
    pub work_mem: f64,
    pub hash_mem_multiplier: f64,
}

impl JoinGucs {
    pub fn read(ctx: &crate::exec::ExecCtx) -> JoinGucs {
        let s = ctx.sess.settings.borrow();
        let on = |n: &str| s.get_raw(n).is_none_or(|v| v != "off" && v != "false");
        let num = |n: &str, d: f64| {
            s.get_raw(n)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(d)
        };
        JoinGucs {
            nestloop: on("enable_nestloop"),
            hashjoin: on("enable_hashjoin"),
            mergejoin: on("enable_mergejoin"),
            material: on("enable_material"),
            incremental_sort: on("enable_incremental_sort"),
            memoize: on("enable_memoize"),
            seq_page_cost: num("seq_page_cost", 1.0),
            work_mem: num("work_mem", 4096.0),
            hash_mem_multiplier: num("hash_mem_multiplier", 2.0),
        }
    }

    fn work_mem_bytes(&self) -> f64 {
        self.work_mem * 1024.0
    }
}

/// What a join input is, as far as rescanning it goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InKind {
    Plain,
    FunctionScan,
    CteScan,
    WorkTableScan,
    Material,
    Sort,
    /// A hash join (its batch count).
    HashJoin(u32),
    /// A parameterized index scan using every join clause as an index condition.
    IndexedJoin,
    /// A nested loop or merge join (order-preserving, no mark/restore).
    Join,
    /// Some other node without mark/restore (a subquery's scan: ExecSupportsMarkRestore).
    NoMark,
    /// A Memoize: its rescan costs (cost_memoize_rescan's startup and total).
    Memoize(f64, f64),
}

impl InKind {
    /// ExecMaterializesOutput.
    pub fn materializes(self) -> bool {
        matches!(
            self,
            InKind::Material
                | InKind::FunctionScan
                | InKind::CteScan
                | InKind::WorkTableScan
                | InKind::Sort
        )
    }
}

/// A join input's estimates.
#[derive(Clone, Copy, Debug)]
pub struct In {
    pub rows: f64,
    pub startup: f64,
    pub total: f64,
    pub width: u32,
    pub disabled: u32,
    pub kind: InKind,
}

/// The cost of evaluating some quals: (startup, per tuple).
#[derive(Clone, Copy, Debug, Default)]
pub struct QualCost {
    pub startup: f64,
    pub per_tuple: f64,
}

/// SemiAntiJoinFactors.
#[derive(Clone, Copy, Debug)]
pub struct Semi {
    pub outer_match_frac: f64,
    pub match_count: f64,
}

/// The join type as costing sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Jt {
    Inner,
    Left,
    Right,
    Full,
    Semi,
    Anti,
    /// The semi / anti join with the sides swapped (the inner side is the one whose rows
    /// come out: Hash Right Semi Join, Hash / Merge Right Anti Join).
    RightSemi,
    RightAnti,
}

/// relation_byte_size.
pub fn relation_byte_size(rows: f64, width: u32) -> f64 {
    costs::relation_byte_size(rows, width)
}

fn page_size(rows: f64, width: u32) -> f64 {
    (relation_byte_size(rows, width) / BLCKSZ).ceil()
}

/// cost_rescan: (startup, total) of scanning `p` again.
pub fn rescan(p: &In, g: &JoinGucs) -> (f64, f64) {
    match p.kind {
        InKind::Memoize(startup, total) => (startup, total),
        InKind::FunctionScan => (0.0, p.total - p.startup),
        InKind::HashJoin(1) => (0.0, p.total - p.startup),
        InKind::CteScan | InKind::WorkTableScan => {
            let mut run = CPU_TUPLE_COST * p.rows;
            let nbytes = relation_byte_size(p.rows, p.width);
            if nbytes > g.work_mem_bytes() {
                run += g.seq_page_cost * (nbytes / BLCKSZ).ceil();
            }
            (0.0, run)
        }
        InKind::Material | InKind::Sort => {
            let mut run = CPU_OPERATOR_COST * p.rows;
            let nbytes = relation_byte_size(p.rows, p.width);
            if nbytes > g.work_mem_bytes() {
                run += g.seq_page_cost * (nbytes / BLCKSZ).ceil();
            }
            (0.0, run)
        }
        _ => (p.startup, p.total),
    }
}

/// cost_memoize_rescan: (startup, total) of looking a parameterized input up in a
/// Memoize's cache. `calls`: the outer rows; `ndistinct`: the distinct parameter values
/// estimate_num_groups finds (None: it fell back on a default); `key_width`: the cache
/// keys' widths.
pub fn memoize_rescan(
    sub: &In,
    calls: f64,
    ndistinct: Option<f64>,
    key_width: f64,
    g: &JoinGucs,
) -> (f64, f64) {
    let tuples = sub.rows;
    let hash_mem = g.work_mem * g.hash_mem_multiplier * 1024.0;
    // relation_byte_size + ExecEstimateCacheEntryOverheadBytes + the keys.
    let entry = relation_byte_size(tuples, sub.width) + 24.0 + 24.0 + 16.0 * tuples + key_width;
    let cache_entries = (hash_mem / entry).floor();
    let ndistinct = ndistinct.unwrap_or(calls);
    let evict = 1.0 - cache_entries.min(ndistinct) / ndistinct;
    let hit = ((calls - ndistinct) / calls) * (cache_entries / ndistinct.max(cache_entries));
    let mut total = sub.total * (1.0 - hit) + CPU_OPERATOR_COST;
    total += CPU_TUPLE_COST * evict;
    total += CPU_OPERATOR_COST / 10.0 * evict * tuples;
    total += CPU_TUPLE_COST + CPU_OPERATOR_COST * tuples;
    let startup = sub.startup * (1.0 - hit) + CPU_TUPLE_COST;
    (startup, total)
}

/// estimate_num_groups over a Memoize's key variables (each: its statistics variable, a
/// distinct count known otherwise, the rows of its relation) for `rows` calls; None when
/// it falls back on a default (then every call counts as distinct).
pub fn memo_ndistinct(
    cat: &Catalog,
    vars: &[(Option<VarRef>, Option<f64>, f64)],
    rows: f64,
) -> Option<f64> {
    let mut total = 1.0;
    // Per relation: (table, product of distinct counts, tuples, rows, columns, max).
    let mut rels: Vec<(u32, f64, f64, f64, usize, f64)> = Vec::new();
    for (var, distinct, leaf_rows) in vars {
        match var {
            Some(v) => {
                let (nd, def) = v.ndistinct(cat);
                if def {
                    return None;
                }
                let rel_rows = if v.rows > 0.0 { v.rows } else { *leaf_rows };
                match rels.iter_mut().find(|r| r.0 == v.table.oid) {
                    Some(r) => {
                        r.1 *= nd;
                        r.4 += 1;
                        r.5 = r.5.max(nd);
                    }
                    None => rels.push((v.table.oid, nd, v.tuples, rel_rows, 1, nd)),
                }
            }
            None => total *= (*distinct)?,
        }
    }
    for (_, mut nd, tuples, rel_rows, nvars, maxnd) in rels {
        let mut clamp = tuples;
        if nvars > 1 {
            clamp *= 0.1;
            if clamp < maxnd {
                clamp = maxnd.min(tuples);
            }
        }
        if nd > clamp && clamp > 0.0 {
            nd = clamp;
        }
        if nd > 0.0 && rel_rows < tuples {
            nd *= 1.0 - ((tuples - rel_rows) / tuples).powf(tuples / nd);
        }
        total *= costs::clamp_rows(nd);
    }
    Some(total.ceil().min(rows).max(1.0))
}

/// cost_material: a Materialize over `p`.
pub fn material(p: &In, g: &JoinGucs) -> In {
    let mut run = p.total - p.startup;
    run = (2.0 * CPU_OPERATOR_COST).mul_add(p.rows, run);
    let nbytes = relation_byte_size(p.rows, p.width);
    if nbytes > g.work_mem_bytes() {
        run += g.seq_page_cost * (nbytes / BLCKSZ).ceil();
    }
    In {
        rows: p.rows,
        startup: p.startup,
        total: p.startup + run,
        width: p.width,
        disabled: p.disabled + (!g.material) as u32,
        kind: InKind::Material,
    }
}

/// A join's cost: (disabled nodes, startup, total).
#[derive(Clone, Copy, Debug)]
pub struct Cost {
    pub disabled: u32,
    pub startup: f64,
    pub total: f64,
}

/// initial_cost_nestloop + final_cost_nestloop. `rows` is the join's size; `quals` all
/// the join's clauses (evaluated per pair).
#[allow(clippy::too_many_arguments)]
pub fn nestloop(
    jt: Jt,
    outer: &In,
    inner: &In,
    inner_unique: bool,
    semi: Option<Semi>,
    rows: f64,
    quals: QualCost,
    g: &JoinGucs,
) -> Cost {
    let disabled = (!g.nestloop) as u32 + inner.disabled + outer.disabled;
    let mut outer_rows = outer.rows;
    let (rescan_start, rescan_total) = rescan(inner, g);
    let mut startup = outer.startup + inner.startup;
    let mut run = outer.total - outer.startup;
    if outer_rows > 1.0 {
        run = (outer_rows - 1.0).mul_add(rescan_start, run);
    }
    let inner_run = inner.total - inner.startup;
    let inner_rescan_run = rescan_total - rescan_start;
    let stops = matches!(jt, Jt::Semi | Jt::Anti) || inner_unique;
    if !stops {
        run += inner_run;
        if outer_rows > 1.0 {
            run = (outer_rows - 1.0).mul_add(inner_rescan_run, run);
        }
    }
    if outer_rows <= 0.0 {
        outer_rows = 1.0;
    }
    let inner_rows = if inner.rows <= 0.0 { 1.0 } else { inner.rows };
    let ntuples = if stops {
        let sf = semi.unwrap_or(Semi {
            outer_match_frac: 1.0,
            match_count: 1.0,
        });
        let mut matched = (outer_rows * sf.outer_match_frac).round_ties_even();
        let mut unmatched = outer_rows - matched;
        let frac = 2.0 / (sf.match_count + 1.0);
        let mut nt = matched * inner_rows * frac;
        if inner.kind == InKind::IndexedJoin {
            run = inner_run.mul_add(frac, run);
            if matched > 1.0 {
                run = ((matched - 1.0) * inner_rescan_run).mul_add(frac, run);
            }
            run += unmatched * inner_rescan_run / inner_rows;
        } else {
            nt = unmatched.mul_add(inner_rows, nt);
            run += inner_run;
            if unmatched >= 1.0 {
                unmatched -= 1.0;
            } else {
                matched -= 1.0;
            }
            if matched > 0.0 {
                run = (matched * inner_rescan_run).mul_add(frac, run);
            }
            if unmatched > 0.0 {
                run = unmatched.mul_add(inner_rescan_run, run);
            }
        }
        nt
    } else {
        outer_rows * inner_rows
    };
    startup += quals.startup;
    let cpu_per_tuple = CPU_TUPLE_COST + quals.per_tuple;
    run = cpu_per_tuple.mul_add(ntuples, run);
    let _ = rows;
    Cost {
        disabled,
        startup,
        total: startup + run,
    }
}

/// ExecChooseHashTableSize for a Parallel Hash (try_combined_hash_mem): the combined
/// hash_mem of `workers` workers and the leader when the whole table fits in it, else the
/// plain sizing with one process's hash_mem. `ntuples`: all the participants' rows.
pub fn parallel_hash_table_size(ntuples: f64, width: u32, g: &JoinGucs, workers: usize) -> (u32, u32) {
    let (nb, nbatch, _) = parallel_hash_table_space(ntuples, width, g, workers);
    (nb, nbatch)
}

/// `parallel_hash_table_size`, with the memory the table may use (space_allowed).
pub fn parallel_hash_table_space(ntuples: f64, width: u32, g: &JoinGucs, workers: usize) -> (u32, u32, f64) {
    let mut combined = *g;
    combined.work_mem *= workers as f64 + 1.0;
    let (nb, nbatch, space) = hash_table_space(ntuples, width, &combined);
    if nbatch > 1 {
        return hash_table_space(ntuples, width, g);
    }
    (nb, nbatch, space)
}

/// ExecChooseHashTableSize: (buckets, batches).
pub fn hash_table_size(ntuples: f64, width: u32, g: &JoinGucs) -> (u32, u32) {
    let (nb, nbatch, _) = hash_table_space(ntuples, width, g);
    (nb, nbatch)
}

/// ExecChooseHashTableSize: (buckets, batches, space_allowed: the bytes the table may use,
/// doubled with each halving of the batches).
pub fn hash_table_space(ntuples: f64, width: u32, g: &JoinGucs) -> (u32, u32, f64) {
    let ntuples = if ntuples <= 0.0 { 1000.0 } else { ntuples };
    let maxalign = |n: usize| n.div_ceil(8) * 8;
    // HJTUPLE_OVERHEAD + MAXALIGN(SizeofMinimalTupleHeader) + MAXALIGN(width).
    let tupsize = (16 + 16 + maxalign(width as usize)) as f64;
    let inner_bytes = ntuples * tupsize;
    let mut hash_bytes = (g.work_mem * g.hash_mem_multiplier * 1024.0).floor();
    let mut space_allowed = hash_bytes;
    // The skew table's share (SKEW_HASH_MEM_PERCENT = 2).
    let bytes_per_mcv = tupsize + 8.0 * 8.0 + 4.0 + 16.0;
    let mut skew_mcvs = (hash_bytes / bytes_per_mcv).floor();
    skew_mcvs = (skew_mcvs * 2.0 / 100.0).floor();
    if skew_mcvs > 0.0 {
        hash_bytes -= skew_mcvs * bytes_per_mcv;
    }
    let prevpow2 = |n: f64| {
        let n = n.max(1.0) as u64;
        1u64 << (63 - n.leading_zeros())
    };
    let nextpow2 = |n: u64| n.max(1).next_power_of_two();
    // (MaxAllocSize / sizeof(HashJoinTuple), rounded down to a power of 2.)
    let max_alloc_pointers = (((1u64 << 30) - 1) / 8) as f64;
    let max_pointers = (hash_bytes / 8.0).floor().min(max_alloc_pointers);
    let max_pointers = (prevpow2(max_pointers) as f64).min((i32::MAX / 2 + 1) as f64);
    let dbuckets = ntuples.ceil().min(max_pointers);
    let mut nbuckets = nextpow2((dbuckets as u64).max(1024));
    let mut nbatch: u64 = 1;
    let bucket_bytes = 8.0 * nbuckets as f64;
    if inner_bytes + bucket_bytes > hash_bytes {
        let bucket_size = tupsize + 8.0;
        let sbuckets = if hash_bytes <= bucket_size {
            1
        } else {
            nextpow2((hash_bytes / bucket_size).floor() as u64)
        };
        let sbuckets = (sbuckets as f64).min(max_pointers) as u64;
        nbuckets = nextpow2(sbuckets);
        let bucket_bytes = nbuckets as f64 * 8.0;
        let dbatch = (inner_bytes / (hash_bytes - bucket_bytes))
            .ceil()
            .min(max_pointers);
        nbatch = nextpow2((dbatch as u64).max(2));
    }
    // Halving the batches (doubling the table) while that uses less memory, the batch
    // files' buffers counted: (S + 2 * nbatch * BLCKSZ) < (2 * S + nbatch * BLCKSZ) stops it.
    while nbatch > 1 {
        if nbuckets as f64 > max_alloc_pointers / 2.0 {
            break;
        }
        if (nbatch as f64) < (space_allowed / BLCKSZ).floor() {
            break;
        }
        nbuckets *= 2;
        space_allowed *= 2.0;
        nbatch /= 2;
    }
    (nbuckets as u32, nbatch as u32, space_allowed)
}

/// estimate_hash_bucket_stats: (mcv frequency, bucket size fraction) of the inner key.
/// `rel_rows` / `rel_tuples`: the key's relation's rows after its restrictions, and in all.
pub fn bucket_stats(
    cat: &Catalog,
    var: Option<&VarRef>,
    rel_rows: f64,
    nbuckets: f64,
) -> (f64, f64) {
    let Some(v) = var else {
        return (0.0, 0.1_f64.max(0.0));
    };
    let s = costs::col_stats_of(cat, v);
    let mcv_freq = s
        .and_then(|s| s.slots.iter().find(|x| x.kind == 1))
        .and_then(|x| x.numbers.as_ref()?.first().copied())
        .map_or(0.0, |f| f as f64);
    let (mut nd, default) = v.ndistinct(cat);
    if default {
        return (mcv_freq, 0.1_f64.max(mcv_freq));
    }
    let nullfrac = s.map_or(0.0, |s| s.nullfrac as f64);
    let avgfreq = (1.0 - nullfrac) / nd;
    if v.tuples > 0.0 {
        nd *= rel_rows / v.tuples;
        nd = costs::clamp_rows(nd);
    }
    let mut est = if nd > nbuckets { 1.0 / nbuckets } else { 1.0 / nd };
    if avgfreq > 0.0 && mcv_freq > avgfreq {
        est *= mcv_freq / avgfreq;
    }
    (mcv_freq, est.clamp(1.0e-6, 1.0))
}

/// What a hash join's costing needs to know beyond its inputs.
#[derive(Clone, Copy, Debug)]
pub struct HashInfo {
    pub nclauses: usize,
    /// The hash clauses' evaluation cost, and the other join quals'.
    pub hash_quals: QualCost,
    pub other_quals: QualCost,
    /// The inner side's bucket size fraction and MCV frequency (the smallest over the
    /// clauses), None for a unique-ified inner side.
    pub bucket: Option<(f64, f64)>,
    /// approx_tuple_count of the hash clauses.
    pub hash_tuples: f64,
    /// A Parallel Hash: the inner side's rows over all the participants (its partial
    /// rows times its parallel divisor) and the outer side's workers, which size the
    /// shared table (initial_cost_hashjoin's inner_path_rows_total).
    pub parallel: Option<(f64, usize)>,
}

/// initial_cost_hashjoin + final_cost_hashjoin: the cost and the batch count.
pub fn hashjoin(
    jt: Jt,
    outer: &In,
    inner: &In,
    inner_unique: bool,
    semi: Option<Semi>,
    h: &HashInfo,
    g: &JoinGucs,
) -> (Cost, u32) {
    let disabled = (!g.hashjoin) as u32 + inner.disabled + outer.disabled;
    let outer_rows = outer.rows;
    let inner_rows = inner.rows;
    let n = h.nclauses as f64;
    let mut startup = outer.startup;
    let mut run = outer.total - outer.startup;
    startup += inner.total;
    startup = CPU_OPERATOR_COST.mul_add(n, CPU_TUPLE_COST).mul_add(inner_rows, startup);
    run = (CPU_OPERATOR_COST * n).mul_add(outer_rows, run);
    let (nbuckets, nbatch) = match h.parallel {
        Some((rows_total, workers)) => parallel_hash_table_size(rows_total, inner.width, g, workers),
        None => hash_table_size(inner_rows, inner.width, g),
    };
    if nbatch > 1 {
        let outerpages = page_size(outer_rows, outer.width);
        let innerpages = page_size(inner_rows, inner.width);
        startup = g.seq_page_cost.mul_add(innerpages, startup);
        run = g.seq_page_cost.mul_add(innerpages + 2.0 * outerpages, run);
    }
    let virtualbuckets = nbuckets as f64 * nbatch as f64;
    let (bucketsize, mcvfreq) = match h.bucket {
        Some((b, m)) => (b, m),
        None => (1.0 / virtualbuckets, 0.0),
    };
    // A bucket holding the most common value beyond hash_mem: hash only as a last
    // resort (disable_cost).
    if relation_byte_size(costs::clamp_rows(inner_rows * mcvfreq), inner.width)
        > g.work_mem * g.hash_mem_multiplier * 1024.0
    {
        startup += 1.0e10;
    }
    let hashjointuples;
    if matches!(jt, Jt::Semi | Jt::Anti) || inner_unique {
        let sf = semi.unwrap_or(Semi {
            outer_match_frac: 1.0,
            match_count: 1.0,
        });
        let matched = (outer_rows * sf.outer_match_frac).round_ties_even();
        let frac = 2.0 / (sf.match_count + 1.0);
        startup += h.hash_quals.startup;
        run = (h.hash_quals.per_tuple
            * matched
            * costs::clamp_rows(inner_rows * bucketsize * frac))
        .mul_add(0.5, run);
        run = (h.hash_quals.per_tuple
            * (outer_rows - matched)
            * costs::clamp_rows(inner_rows / virtualbuckets))
        .mul_add(0.05, run);
        hashjointuples = if jt == Jt::Anti {
            outer_rows - matched
        } else {
            matched
        };
    } else {
        startup += h.hash_quals.startup;
        run = (h.hash_quals.per_tuple * outer_rows * costs::clamp_rows(inner_rows * bucketsize))
            .mul_add(0.5, run);
        hashjointuples = h.hash_tuples;
    }
    startup += h.other_quals.startup;
    let cpu_per_tuple = CPU_TUPLE_COST + h.other_quals.per_tuple;
    run = cpu_per_tuple.mul_add(hashjointuples, run);
    (
        Cost {
            disabled,
            startup,
            total: startup + run,
        },
        nbatch,
    )
}

/// What a merge join's costing needs to know beyond its inputs.
#[derive(Clone, Copy, Debug)]
pub struct MergeInfo {
    pub merge_quals: QualCost,
    pub other_quals: QualCost,
    /// mergejoinscansel of the first clause: (outer start, outer end, inner start, inner
    /// end) fractions.
    pub scansel: (f64, f64, f64, f64),
    /// approx_tuple_count of the merge clauses.
    pub merge_tuples: f64,
    /// Every join clause is a merge clause.
    pub all_merge: bool,
    /// The inputs are sorted for the join (None: already in order); the inner one's sort
    /// can't be incremental.
    pub outer_sort: Option<SortIn>,
    pub inner_sort: bool,
    /// The outer side is unique-ified.
    pub outer_unique: bool,
    /// The materialization decision already made (EXPLAIN of a planned join).
    pub material: Option<bool>,
}

/// An explicit sort of a merge join's outer input: incremental over `presorted` keys
/// (with `groups` groups) when there are some.
#[derive(Clone, Copy, Debug)]
pub struct SortIn {
    pub presorted: usize,
    pub groups: f64,
}

/// initial_cost_mergejoin + final_cost_mergejoin: the cost, whether the inner side gets
/// a Materialize, and the sorted inputs' costs (startup, total).
#[allow(clippy::type_complexity)]
pub fn mergejoin(
    jt: Jt,
    outer: &In,
    inner: &In,
    inner_unique: bool,
    m: &MergeInfo,
    g: &JoinGucs,
) -> (Cost, bool, Option<(f64, f64)>, Option<(f64, f64)>) {
    let outer_rows_in = if outer.rows <= 0.0 { 1.0 } else { outer.rows };
    let inner_rows_in = if inner.rows <= 0.0 { 1.0 } else { inner.rows };
    let (mut os, mut oe, mut is, mut ie) = m.scansel;
    match jt {
        Jt::Left | Jt::Anti => {
            os = 0.0;
            oe = 1.0;
        }
        Jt::Right | Jt::RightAnti => {
            is = 0.0;
            ie = 1.0;
        }
        Jt::Full => {
            os = 0.0;
            is = 0.0;
            oe = 1.0;
            ie = 1.0;
        }
        _ => {}
    }
    let outer_skip = (outer_rows_in * os).round_ties_even();
    let inner_skip = (inner_rows_in * is).round_ties_even();
    let outer_rows = costs::clamp_rows(outer_rows_in * oe);
    let inner_rows = costs::clamp_rows(inner_rows_in * ie);
    let os = outer_skip / outer_rows_in;
    let is = inner_skip / inner_rows_in;
    let oe = outer_rows / outer_rows_in;
    let ie = inner_rows / inner_rows_in;
    let mut disabled = (!g.mergejoin) as u32;
    let mut startup = 0.0;
    let mut run = 0.0;
    let mut outer_sorted = None;
    let mut inner_sorted = None;
    let (ost, otot) = match m.outer_sort {
        Some(sk) => {
            let c = if g.incremental_sort && sk.presorted > 0 {
                costs::incremental_sort_cost(
                    outer.startup,
                    outer.total,
                    outer.rows,
                    outer.width,
                    sk.groups,
                    None,
                )
            } else {
                costs::sort_cost(outer.total, outer.rows, outer.width, None)
            };
            outer_sorted = Some(c);
            disabled += outer.disabled;
            c
        }
        None => {
            disabled += outer.disabled;
            (outer.startup, outer.total)
        }
    };
    startup += ost;
    startup = (otot - ost).mul_add(os, startup);
    run = (otot - ost).mul_add(oe - os, run);
    let (ist, itot) = if m.inner_sort {
        let c = costs::sort_cost(inner.total, inner.rows, inner.width, None);
        inner_sorted = Some(c);
        c
    } else {
        (inner.startup, inner.total)
    };
    disabled += inner.disabled;
    startup += ist;
    startup = (itot - ist).mul_add(is, startup);
    let inner_run = (itot - ist) * (ie - is);
    // final_cost_mergejoin.
    let skip_mark = (matches!(jt, Jt::Semi | Jt::Anti) || inner_unique) && m.all_merge;
    let rescanned = if m.outer_unique || skip_mark {
        0.0
    } else {
        (m.merge_tuples - inner_rows_in).max(0.0)
    };
    let ratio = 1.0 + rescanned / inner_rows;
    let bare = inner_run * ratio;
    let mat = (CPU_OPERATOR_COST * inner_rows).mul_add(ratio, inner_run);
    let materialize = if let Some(m) = m.material {
        m
    } else if skip_mark {
        false
    } else if g.material && mat < bare {
        true
    } else if !m.inner_sort && matches!(inner.kind, InKind::Join | InKind::HashJoin(_) | InKind::NoMark) {
        // (Only index scans, sorts and materializations can mark and restore.)
        true
    } else {
        g.material
            && m.inner_sort
            && relation_byte_size(inner_rows_in, inner.width) > g.work_mem * 1024.0
    };
    run += if materialize { mat } else { bare };
    startup += m.merge_quals.startup;
    startup = m
        .merge_quals
        .per_tuple
        .mul_add(inner_skip.mul_add(ratio, outer_skip), startup);
    run = m.merge_quals.per_tuple.mul_add(
        (inner_rows - inner_skip).mul_add(ratio, outer_rows - outer_skip),
        run,
    );
    startup += m.other_quals.startup;
    let cpu_per_tuple = CPU_TUPLE_COST + m.other_quals.per_tuple;
    run = cpu_per_tuple.mul_add(m.merge_tuples, run);
    (
        Cost {
            disabled,
            startup,
            total: startup + run,
        },
        materialize,
        outer_sorted,
        inner_sorted,
    )
}

// ── selectivity ──

/// A statistics slot's values and numbers (MCV: kind 1).
fn mcvs(cat: &Catalog, v: &VarRef, fmt: &FormatCtx) -> Option<(Vec<Value>, Vec<f64>)> {
    let s = costs::col_stats_of(cat, v)?;
    let sl = s.slots.iter().find(|x| x.kind == 1)?;
    let numbers: Vec<f64> = sl.numbers.as_ref()?.iter().map(|x| *x as f64).collect();
    let (ty, vals) = sl.values.as_ref()?;
    let values = vals
        .iter()
        .map(|x| crate::types::text::parse_value(x, ty, fmt))
        .collect::<crate::error::Result<Vec<_>>>()
        .ok()?;
    Some((values, numbers))
}

fn eq(a: &Value, b: &Value) -> bool {
    !a.is_null()
        && !b.is_null()
        && crate::exec::agg::cmp_rows(std::slice::from_ref(a), std::slice::from_ref(b)).is_eq()
}

/// One side of an equality join clause: its variable (if a column with statistics can
/// stand for it), or a known distinct count.
#[derive(Clone)]
pub struct JoinVar {
    pub var: Option<VarRef>,
    pub distinct: Option<f64>,
    /// The rows of the relation the variable is from (after its restrictions).
    pub rel_rows: f64,
}

impl JoinVar {
    fn nd(&self, cat: &Catalog) -> (f64, bool) {
        match &self.var {
            Some(v) => v.ndistinct(cat),
            None => match self.distinct {
                Some(d) => (d, false),
                None => (costs::DEFAULT_NUM_DISTINCT, true),
            },
        }
    }

    fn nullfrac(&self, cat: &Catalog) -> Option<f64> {
        let v = self.var.as_ref()?;
        costs::col_stats_of(cat, v).map(|s| s.nullfrac as f64)
    }
}

/// eqjoinsel for an inner join.
pub fn eqjoinsel_inner(cat: &Catalog, fmt: &FormatCtx, a: &JoinVar, b: &JoinVar) -> f64 {
    let (nd1, _) = a.nd(cat);
    let (nd2, _) = b.nd(cat);
    let m1 = a.var.as_ref().and_then(|v| mcvs(cat, v, fmt));
    let m2 = b.var.as_ref().and_then(|v| mcvs(cat, v, fmt));
    let sel = match (m1, m2) {
        (Some((v1, f1)), Some((v2, f2))) => {
            let nf1 = a.nullfrac(cat).unwrap_or(0.0);
            let nf2 = b.nullfrac(cat).unwrap_or(0.0);
            let mut has1 = vec![false; v1.len()];
            let mut has2 = vec![false; v2.len()];
            let mut matchprodfreq = 0.0;
            let mut nmatches = 0.0;
            for i in 0..v1.len() {
                for j in 0..v2.len() {
                    if has2[j] {
                        continue;
                    }
                    if eq(&v1[i], &v2[j]) {
                        has1[i] = true;
                        has2[j] = true;
                        matchprodfreq = f1[i].mul_add(f2[j], matchprodfreq);
                        nmatches += 1.0;
                        break;
                    }
                }
            }
            let matchprodfreq: f64 = matchprodfreq.clamp(0.0, 1.0);
            let (mut mf1, mut uf1) = (0.0, 0.0);
            for i in 0..v1.len() {
                if has1[i] {
                    mf1 += f1[i];
                } else {
                    uf1 += f1[i];
                }
            }
            let (mf1, uf1): (f64, f64) = (mf1.clamp(0.0, 1.0), uf1.clamp(0.0, 1.0));
            let (mut mf2, mut uf2) = (0.0, 0.0);
            for j in 0..v2.len() {
                if has2[j] {
                    mf2 += f2[j];
                } else {
                    uf2 += f2[j];
                }
            }
            let (mf2, uf2): (f64, f64) = (mf2.clamp(0.0, 1.0), uf2.clamp(0.0, 1.0));
            let of1 = (1.0 - nf1 - mf1 - uf1).clamp(0.0, 1.0);
            let of2 = (1.0 - nf2 - mf2 - uf2).clamp(0.0, 1.0);
            let n1 = v1.len() as f64;
            let n2 = v2.len() as f64;
            let mut t1 = matchprodfreq;
            if nd2 > n2 {
                t1 += uf1 * of2 / (nd2 - n2);
            }
            if nd2 > nmatches {
                t1 += of1 * (of2 + uf2) / (nd2 - nmatches);
            }
            let mut t2 = matchprodfreq;
            if nd1 > n1 {
                t2 += uf2 * of1 / (nd1 - n1);
            }
            if nd1 > nmatches {
                t2 += of2 * (of1 + uf1) / (nd1 - nmatches);
            }
            if t1 < t2 { t1 } else { t2 }
        }
        _ => {
            let nf1 = a.nullfrac(cat).unwrap_or(0.0);
            let nf2 = b.nullfrac(cat).unwrap_or(0.0);
            let mut s = (1.0 - nf1) * (1.0 - nf2);
            if nd1 > nd2 {
                s /= nd1;
            } else {
                s /= nd2;
            }
            s
        }
    };
    sel.clamp(0.0, 1.0)
}

/// eqjoinsel for a semi / anti join: the fraction of the outer side's rows (`a`) with a
/// match in the inner side (`b`, whose relation has `inner_rows` rows).
pub fn eqjoinsel_semi(
    cat: &Catalog,
    fmt: &FormatCtx,
    a: &JoinVar,
    b: &JoinVar,
    inner_rows: f64,
) -> f64 {
    let (mut nd1, default1) = a.nd(cat);
    let (mut nd2, mut default2) = b.nd(cat);
    if b.var.is_some() && nd2 >= b.rel_rows {
        nd2 = b.rel_rows;
        default2 = false;
    }
    if nd2 >= inner_rows {
        nd2 = inner_rows;
        default2 = false;
    }
    let nf1 = a.nullfrac(cat).unwrap_or(0.0);
    let m1 = a.var.as_ref().and_then(|v| mcvs(cat, v, fmt));
    let m2 = b.var.as_ref().and_then(|v| mcvs(cat, v, fmt));
    let sel = match (m1, m2) {
        (Some((v1, f1)), Some((v2, _))) => {
            let clamped = (v2.len() as f64).min(nd2) as usize;
            let mut has1 = vec![false; v1.len()];
            let mut has2 = vec![false; clamped];
            let mut nmatches = 0.0;
            for i in 0..v1.len() {
                for j in 0..clamped {
                    if has2[j] {
                        continue;
                    }
                    if eq(&v1[i], &v2[j]) {
                        has1[i] = true;
                        has2[j] = true;
                        nmatches += 1.0;
                        break;
                    }
                }
            }
            let mut mf1: f64 = 0.0;
            for i in 0..v1.len() {
                if has1[i] {
                    mf1 += f1[i];
                }
            }
            let mf1 = mf1.clamp(0.0, 1.0);
            let uncertainfrac = if !default1 && !default2 {
                nd1 -= nmatches;
                nd2 -= nmatches;
                if nd1 <= nd2 || nd2 < 0.0 {
                    1.0
                } else {
                    nd2 / nd1
                }
            } else {
                0.5
            };
            let uncertain = (1.0 - mf1 - nf1).clamp(0.0, 1.0);
            uncertainfrac.mul_add(uncertain, mf1)
        }
        _ => {
            if !default1 && !default2 {
                if nd1 <= nd2 || nd2 < 0.0 {
                    1.0 - nf1
                } else {
                    (nd2 / nd1) * (1.0 - nf1)
                }
            } else {
                0.5 * (1.0 - nf1)
            }
        }
    };
    sel.clamp(0.0, 1.0)
}

/// calc_joinrel_size_estimate.
pub fn join_rows(jt: Jt, outer_rows: f64, inner_rows: f64, jsel: f64, psel: f64) -> f64 {
    let n = match jt {
        Jt::Inner => outer_rows * inner_rows * jsel,
        Jt::Left => (outer_rows * inner_rows * jsel).max(outer_rows) * psel,
        Jt::Right => (outer_rows * inner_rows * jsel).max(inner_rows) * psel,
        Jt::Full => (outer_rows * inner_rows * jsel)
            .max(outer_rows)
            .max(inner_rows)
            * psel,
        Jt::Semi => outer_rows * jsel,
        Jt::Anti => outer_rows * (1.0 - jsel) * psel,
        Jt::RightSemi => inner_rows * jsel,
        Jt::RightAnti => inner_rows * (1.0 - jsel) * psel,
    };
    costs::clamp_rows(n)
}
