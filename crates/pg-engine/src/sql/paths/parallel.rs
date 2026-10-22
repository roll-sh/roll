//! Parallel query, as far as plans show it (allpaths.c's partial paths, costsize.c's
//! parallel divisor, clauses.c's parallel safety): how many workers a scan of a relation
//! gets (compute_parallel_worker), the cheapest partial path of a table scan
//! (create_plain_partial_paths, partial index and bitmap paths) and what may run in a
//! worker. The rows still come from one process: a Gather above runs its plan itself.

use super::*;
use crate::catalog::Catalog;

/// The parallel query settings.
#[derive(Clone, Debug)]
pub struct ParCfg {
    pub setup_cost: f64,
    pub tuple_cost: f64,
    /// min_parallel_table_scan_size / min_parallel_index_scan_size, in pages.
    pub min_table_pages: f64,
    pub min_index_pages: f64,
    /// max_parallel_workers_per_gather.
    pub max_workers: usize,
    pub leader: bool,
    pub append: bool,
    pub hash: bool,
    pub gather_merge: bool,
}

impl ParCfg {
    pub fn read(ctx: &ExecCtx) -> ParCfg {
        let s = ctx.sess.settings.borrow();
        let on = |n: &str| s.get_raw(n).is_none_or(|v| v != "off" && v != "false");
        let num = |n: &str, d: f64| {
            s.get_raw(n)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(d)
        };
        ParCfg {
            setup_cost: num("parallel_setup_cost", 1000.0),
            tuple_cost: num("parallel_tuple_cost", 0.1),
            min_table_pages: num("min_parallel_table_scan_size", 1024.0),
            min_index_pages: num("min_parallel_index_scan_size", 64.0),
            max_workers: num("max_parallel_workers_per_gather", 2.0).max(0.0) as usize,
            leader: on("parallel_leader_participation"),
            append: on("enable_parallel_append"),
            hash: on("enable_parallel_hash"),
            gather_merge: on("enable_gathermerge"),
        }
    }

    /// The settings, when plans can have workers at all.
    pub fn enabled(self) -> Option<ParCfg> {
        (self.max_workers > 0).then_some(self)
    }

    /// get_parallel_divisor: the processes a partial path's rows are spread over.
    pub fn divisor(&self, workers: usize) -> f64 {
        let w = workers as f64;
        let mut d = w;
        if self.leader {
            let lead = 1.0 - 0.3 * w;
            if lead > 0.0 {
                d += lead;
            }
        }
        d
    }
}

/// A table's `parallel_workers` storage parameter.
pub fn rel_parallel_workers(t: &Table) -> Option<usize> {
    t.options.iter().find_map(|o| {
        let (k, v) = o.split_once('=')?;
        (k.trim().eq_ignore_ascii_case("parallel_workers"))
            .then(|| v.trim().trim_matches('\'').parse::<usize>().ok())
            .flatten()
    })
}

/// compute_parallel_worker: the workers a scan reading `heap_pages` of the table and
/// `index_pages` of an index (-1: none) gets. `member`: an appendrel member (an
/// inheritance child or partition), which no minimum size keeps serial.
pub fn compute_workers(
    t: &Table,
    heap_pages: f64,
    index_pages: f64,
    cfg: &ParCfg,
    member: bool,
) -> usize {
    let mut workers = 0usize;
    if let Some(n) = rel_parallel_workers(t) {
        workers = n;
    } else {
        // (Pages are BlockNumbers.)
        let heap = (heap_pages >= 0.0).then(|| heap_pages.floor());
        let index = (index_pages >= 0.0).then(|| index_pages.floor());
        if !member
            && (heap.is_some_and(|h| h < cfg.min_table_pages)
                || index.is_some_and(|i| i < cfg.min_index_pages))
        {
            return 0;
        }
        let grow = |pages: f64, min: f64| {
            let mut threshold = min.max(1.0);
            let mut n = 1usize;
            while pages >= threshold * 3.0 {
                n += 1;
                threshold *= 3.0;
                if threshold > (i32::MAX / 3) as f64 {
                    break;
                }
            }
            n
        };
        if let Some(h) = heap {
            workers = grow(h, cfg.min_table_pages);
        }
        if let Some(i) = index {
            let n = grow(i, cfg.min_index_pages);
            workers = if workers > 0 { workers.min(n) } else { n };
        }
    }
    workers.min(cfg.max_workers)
}

/// pg_proc.proparallel of the built-in functions.
fn builtin_parallel(oid: u32) -> char {
    static MAP: std::sync::OnceLock<std::collections::HashMap<u32, char>> =
        std::sync::OnceLock::new();
    let m = MAP.get_or_init(|| {
        include_str!("../../catalog/pg_proc.tsv")
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let oid = f.first()?.parse::<u32>().ok()?;
                let p = f.get(14)?.chars().next()?;
                Some((oid, p))
            })
            .collect()
    });
    m.get(&oid).copied().unwrap_or('s')
}

/// A function's parallel safety ('s' safe, 'r' restricted, 'u' unsafe).
pub fn func_parallel(cat: &Catalog, oid: u32) -> char {
    match cat.functions.get(&oid) {
        Some(f) => f.parallel,
        None => builtin_parallel(oid),
    }
}

/// max_parallel_hazard of an expression: the worst function in it ('s', 'r' or 'u').
pub fn hazard(e: &Expr, cat: &Catalog) -> char {
    let mut worst = 's';
    let mut note = |c: char| {
        worst = match (worst, c) {
            (_, 'u') | ('u', _) => 'u',
            (_, 'r') | ('r', _) => 'r',
            _ => 's',
        }
    };
    e.walk(&mut |x| match x {
        Expr::Call(d, ..) => note(func_parallel(cat, d.oid)),
        Expr::Subquery(s) => {
            if let Some(c) = &s.call {
                note(func_parallel(cat, c.def.oid));
            } else if !s.params.is_empty() {
                // A correlated SubPlan isn't parallel safe: its plan reads the outer
                // query's values as Params, which workers don't get.
                note('r');
            } else {
                note(plan_hazard(&s.plan, cat));
            }
        }
        // (So does a reference to an enclosing query's value.)
        Expr::Outer(..) | Expr::Window(..) => note('r'),
        _ => {}
    });
    worst
}

/// The worst parallel hazard in a plan's expressions and nodes.
pub fn plan_hazard(p: &Plan, cat: &Catalog) -> char {
    let mut exprs = Vec::new();
    crate::sql::explain::node_exprs(p, &mut exprs);
    let mut worst = 's';
    let mut note = |c: char| {
        worst = match (worst, c) {
            (_, 'u') | ('u', _) => 'u',
            (_, 'r') | ('r', _) => 'r',
            _ => 's',
        }
    };
    for e in exprs {
        note(hazard(e, cat));
    }
    match p {
        Plan::Scan(s) if s.table.temporary => note('r'),
        Plan::PlCall { func, .. } => note(func.parallel),
        Plan::CteScan { .. } | Plan::WorkScan { .. } | Plan::With { .. } => note('r'),
        Plan::Insert(_) | Plan::Update(_) | Plan::Delete(_) | Plan::Merge(_) => note('u'),
        Plan::LockRows { .. } => note('u'),
        _ => {}
    }
    for c in crate::sql::explain::plan_inputs(p) {
        note(plan_hazard(c, cat));
    }
    worst
}

/// A table scan's cheapest partial path: the scan with that access path, its EXPLAIN
/// view (the partial path's estimates), and the workers it gets.
pub struct PartialScan {
    pub scan: ScanPlan,
    pub desc: ScanDesc,
    pub workers: usize,
}

/// set_plain_rel_pathlist's partial paths for a scan whose output columns `needed` are
/// used above (empty: all). `member`: an appendrel member. None when the relation can't
/// be scanned in parallel or no scan of it gets workers.
/// None: the scan isn't parallel safe; Some(None): it is, but gets no workers.
pub fn partial_scan(
    ctx: &ExecCtx,
    s: &ScanPlan,
    needed: &[bool],
    member: bool,
) -> Option<Option<PartialScan>> {
    let cfg = ParCfg::read(ctx);
    if cfg.max_workers == 0
        || s.table.temporary
        || s.sample.is_some()
        || s.current_of.is_some()
        || s.mark.is_some()
        || s.table.is_foreign()
    {
        return None;
    }
    let p = Planner::new(ctx);
    if let Some(f) = &s.filter
        && hazard(f, &p.cat) != 's'
    {
        return None;
    }
    Some(partial_scan_safe(ctx, &p, &cfg, s, needed, member))
}

fn partial_scan_safe(
    ctx: &ExecCtx,
    p: &Planner,
    cfg: &ParCfg,
    s: &ScanPlan,
    needed: &[bool],
    member: bool,
) -> Option<PartialScan> {
    let cfg = cfg.clone();
    let rel = p.rel(s, needed)?;
    let mut best: Option<(Path, usize)> = None;
    for (path, workers) in p.partial_paths(&rel, &cfg, member, &s.useful) {
        let better = match &best {
            None => true,
            Some((b, _)) => {
                (path.disabled, path.total) < (b.disabled, b.total)
                    || (path.disabled == b.disabled
                        && path.total == b.total
                        && path.startup < b.startup)
            }
        };
        if better {
            best = Some((path, workers));
        }
    }
    let (path, workers) = best?;
    partial_of(ctx, &rel, &cfg, s, &path, workers)
}

fn partial_of(
    ctx: &ExecCtx,
    rel: &Rel,
    cfg: &ParCfg,
    s: &ScanPlan,
    path: &Path,
    workers: usize,
) -> Option<PartialScan> {
    let mut scan = s.clone();
    apply(rel, &mut scan, path);
    let desc = describe_with(ctx, &scan, Some((workers, cfg.divisor(workers))))?;
    Some(PartialScan {
        scan,
        desc,
        workers,
    })
}

/// add_partial_path: keep `new` in `list` unless a path of compatible pathkeys
/// dominates it (fewer disabled nodes, or fuzzily cheaper in total cost with keys at
/// least as good), dropping the ones it dominates; the list stays in total-cost order.
fn add_partial_path(list: &mut Vec<(Path, usize)>, new: (Path, usize)) {
    let mut accept = true;
    let mut insert_at = 0;
    let mut i = 0;
    while i < list.len() {
        let old = &list[i].0;
        let mut remove_old = false;
        let kc = cmp_keys(&new.0.keys, &old.keys);
        if kc != KeysCmp::Different {
            if new.0.disabled != old.disabled {
                if new.0.disabled > old.disabled {
                    accept = false;
                } else {
                    remove_old = true;
                }
            } else if new.0.total > old.total * FUZZ {
                if kc != KeysCmp::Better1 {
                    accept = false;
                }
            } else if old.total > new.0.total * FUZZ {
                if kc != KeysCmp::Better2 {
                    remove_old = true;
                }
            } else if kc == KeysCmp::Better1 {
                remove_old = true;
            } else if kc == KeysCmp::Better2 {
                accept = false;
            } else if old.total > new.0.total * 1.000_000_000_1 {
                remove_old = true;
            } else {
                accept = false;
            }
        }
        if remove_old {
            list.remove(i);
        } else {
            if new.0.total >= list[i].0.total {
                insert_at = i + 1;
            }
            i += 1;
        }
        if !accept {
            return;
        }
    }
    list.insert(insert_at, new);
}

/// The partial paths of a scan's relation other than its cheapest one that add_partial_path
/// keeps because they come in an order (parallel index scans), each as `partial_scan`
/// gives the cheapest: in total-cost order.
pub fn ordered_partial_scans(
    ctx: &ExecCtx,
    s: &ScanPlan,
    needed: &[bool],
    member: bool,
) -> Vec<PartialScan> {
    let cfg = ParCfg::read(ctx);
    if cfg.max_workers == 0
        || s.table.temporary
        || s.sample.is_some()
        || s.current_of.is_some()
        || s.mark.is_some()
        || s.table.is_foreign()
    {
        return Vec::new();
    }
    let p = Planner::new(ctx);
    if let Some(f) = &s.filter
        && hazard(f, &p.cat) != 's'
    {
        return Vec::new();
    }
    let Some(rel) = p.rel(s, needed) else {
        return Vec::new();
    };
    let mut list: Vec<(Path, usize)> = Vec::new();
    for path in p.partial_paths(&rel, &cfg, member, &s.useful) {
        add_partial_path(&mut list, path);
    }
    list.iter()
        .filter(|(path, _)| !path.keys.is_empty())
        .filter_map(|(path, w)| partial_of(ctx, &rel, &cfg, s, path, *w))
        .collect()
}

impl Planner<'_, '_> {
    /// The partial (parallel-aware) paths of a base relation: a parallel sequential scan,
    /// parallel index scans and a parallel bitmap heap scan, each with its workers.
    pub(super) fn partial_paths(
        &self,
        rel: &Rel,
        cfg: &ParCfg,
        member: bool,
        useful: &crate::sql::plan::ScanUseful,
    ) -> Vec<(Path, usize)> {
        let mut out = Vec::new();
        let t = &rel.table;
        // create_plain_partial_paths.
        let w = compute_workers(t, rel.size.pages, -1.0, cfg, member);
        if w > 0 {
            let div = cfg.divisor(w);
            let cpu = (CPU_TUPLE_COST + rel.qual_cost) * rel.size.tuples;
            out.push((
                Path {
                    kind: PathKind::Seq,
                    startup: rel.qual_startup,
                    total: rel.qual_startup + self.g.seq_page_cost * rel.size.pages + cpu / div,
                    rows: costs::clamp_rows(rel.rows / div),
                    keys: Vec::new(),
                    disabled: (!self.g.seqscan) as u32,
                },
                w,
            ));
        }
        // Partial index scans (btree), and the bitmap heap scan: build_index_paths makes
        // them along with the plain ones, for the orderings found useful.
        let want = match &useful.0 {
            Some(u) => Want {
                keys: u.0.clone(),
                limit: None,
                merge: u.1.clone(),
            },
            None => Want::default(),
        };
        for (path, _) in self.paths(rel, &want).into_iter().map(|p| (p, 0)) {
            match &path.kind {
                PathKind::Index(ip) if rel.indexes[ip.ix].am == Am::Btree => {
                    let w = compute_workers(t, ip.rand_pages, ip.index_pages, cfg, member);
                    if w == 0 {
                        continue;
                    }
                    let div = cfg.divisor(w);
                    let mut p = path.clone();
                    p.total -= ip.cpu_run - ip.cpu_run / div;
                    p.rows = costs::clamp_rows(rel.rows / div);
                    out.push((p, w));
                }
                PathKind::Bitmap(b) => {
                    let (st, tot, pages, tuples) = self.bitmap_heap(rel, b);
                    let w = compute_workers(t, pages, -1.0, cfg, member);
                    if w == 0 {
                        continue;
                    }
                    let div = cfg.divisor(w);
                    let cpu = (CPU_TUPLE_COST + rel.qual_cost) * tuples;
                    let mut p = path.clone();
                    p.startup = st;
                    p.total = tot - cpu + cpu / div;
                    p.rows = costs::clamp_rows(rel.rows / div);
                    out.push((p, w));
                }
                _ => {}
            }
        }
        out
    }
}

/// Whether an aggregate can run partially in workers and be combined (it has a combine
/// function, and a serialization function for an internal state): pg_aggregate.
pub fn agg_partial_ok(oid: u32) -> bool {
    static MAP: std::sync::OnceLock<std::collections::HashMap<u32, bool>> =
        std::sync::OnceLock::new();
    let m = MAP.get_or_init(|| {
        include_str!("../../catalog/data/pg_aggregate.tsv")
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let oid = f.first()?.parse::<u32>().ok()?;
                let combine = f.get(5)?.parse::<u32>().unwrap_or(0);
                let serial = f.get(6)?.parse::<u32>().unwrap_or(0);
                let trans_type = f.get(16)?.parse::<u32>().unwrap_or(0);
                Some((oid, combine != 0 && (trans_type != 2281 || serial != 0)))
            })
            .collect()
    });
    m.get(&oid).copied().unwrap_or(false)
}

/// Whether an aggregate's transition state is `internal` (pg_aggregate.aggtranstype).
pub fn agg_internal_state(oid: u32) -> bool {
    static MAP: std::sync::OnceLock<std::collections::HashMap<u32, bool>> =
        std::sync::OnceLock::new();
    let m = MAP.get_or_init(|| {
        include_str!("../../catalog/data/pg_aggregate.tsv")
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let oid = f.first()?.parse::<u32>().ok()?;
                let trans_type = f.get(16)?.parse::<u32>().unwrap_or(0);
                Some((oid, trans_type == 2281))
            })
            .collect()
    });
    m.get(&oid).copied().unwrap_or(false)
}

/// get_agg_clause_costs' transition space of an aggregate: its state's average width when
/// the state type is passed by reference (MAXALIGNed; an internal state without a declared
/// size: ALLOCSET_DEFAULT_INITSIZE), else 0.
pub fn agg_trans_space(oid: u32) -> f64 {
    static MAP: std::sync::OnceLock<std::collections::HashMap<u32, f64>> =
        std::sync::OnceLock::new();
    let m = MAP.get_or_init(|| {
        include_str!("../../catalog/data/pg_aggregate.tsv")
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let oid = f.first()?.parse::<u32>().ok()?;
                let trans_type = f.get(16)?.parse::<u32>().unwrap_or(0);
                let space = f.get(17)?.parse::<f64>().unwrap_or(0.0);
                // Pass-by-value state types: bool, "char", int8, int2, int4, oid, float4,
                // float8, money, date, time, timestamp, timestamptz.
                let byval = matches!(
                    trans_type,
                    16 | 18 | 20 | 21 | 23 | 26 | 700 | 701 | 790 | 1082 | 1083 | 1114 | 1184
                );
                let width = if byval {
                    0.0
                } else if space > 0.0 {
                    space
                } else if trans_type == 2281 {
                    8192.0
                } else {
                    match trans_type {
                        // Fixed-size by-reference types: name, interval, timetz, uuid…
                        19 => 64.0,
                        1186 => 16.0,
                        1266 => 12.0,
                        2950 => 16.0,
                        600 => 16.0,
                        _ => 32.0,
                    }
                };
                Some((oid, (width / 8.0_f64).ceil() * 8.0))
            })
            .collect()
    });
    m.get(&oid).copied().unwrap_or(0.0)
}
