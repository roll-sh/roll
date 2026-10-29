//! Parallel plans as EXPLAIN shows them: a node's partial plan (its rows spread over the
//! workers) under a Gather when that is cheaper than running it serially
//! (generate_gather_paths), and aggregation split into a Partial Aggregate in the workers
//! and a Finalize Aggregate above the Gather (create_partial_grouping_paths). The
//! executor runs the plan in one process: only the plan's shape and estimates are
//! parallel.

use super::*;

/// `par` (with a Gather) beats `ser`: by add_path's parallel-safety rule when `ser` has no
/// Gather, else by cost.
pub(super) fn par_beats(par: &ExNode, ser: &ExNode) -> bool {
    if has_gather(ser) {
        cheaper(par, ser)
    } else {
        parallel_wins(par, ser)
    }
}

/// A plan a worker can run: parallel-safe scans under joins, hashes, sorts and the like.
pub(super) fn safe_tree(n: &ExNode) -> bool {
    if n.is_scan() {
        return n.par_safe && n.subplans.is_empty();
    }
    match n.node_type {
        "Hash" | "Materialize" | "Sort" | "Incremental Sort" | "Memoize" | "Result" | "Limit" => {
            n.subplans.is_empty() && n.children.iter().all(safe_tree)
        }
        "Hash Join" | "Nested Loop" | "Merge Join" | "Append" => {
            n.par_safe && n.subplans.is_empty()
        }
        _ => false,
    }
}

/// `a` beats `b` as add_path keeps paths: fewer disabled nodes; else clearly (1%) cheaper in
/// total; else, fuzzily as cheap, ordered where `b` isn't, or cheaper.
pub(super) fn fuzzy_better(a: &ExNode, b: &ExNode) -> bool {
    const FUZZ: f64 = 1.01;
    let (da, db) = (disabled_nodes(a), disabled_nodes(b));
    if da != db {
        return da < db;
    }
    if b.total_cost > a.total_cost * FUZZ {
        return true;
    }
    if a.total_cost > b.total_cost * FUZZ {
        return false;
    }
    match (a.order.is_empty(), b.order.is_empty()) {
        (false, true) => true,
        (true, false) => false,
        _ => cheaper(a, b),
    }
}

/// A plan with a Gather (or Gather Merge) in it.
pub(super) fn has_gather(n: &ExNode) -> bool {
    matches!(n.node_type, "Gather" | "Gather Merge") || n.children.iter().any(has_gather)
}

/// The disabled nodes of a plan (a node whose enable_* setting is off).
pub(super) fn disabled_nodes(n: &ExNode) -> usize {
    usize::from(n.disabled)
        + n.children.iter().map(disabled_nodes).sum::<usize>()
}

/// `a` is cheaper than `b` (set_cheapest: disabled nodes, then total, then startup cost).
/// add_path between two paths with the same order: `a` replaces `b` when fewer nodes are
/// disabled, or clearly (1%) cheaper in total, or fuzzily as cheap in total but clearly
/// cheaper to start, or else (costs fuzzily equal) cheaper in total at all.
pub(super) fn dominates(a: &ExNode, b: &ExNode) -> bool {
    const FUZZ: f64 = 1.01;
    let (da, db) = (disabled_nodes(a), disabled_nodes(b));
    if da != db {
        return da < db;
    }
    if b.total_cost > a.total_cost * FUZZ {
        return true;
    }
    if a.total_cost > b.total_cost * FUZZ {
        return false;
    }
    if b.startup_cost > a.startup_cost * FUZZ {
        return true;
    }
    if a.startup_cost > b.startup_cost * FUZZ {
        return false;
    }
    b.total_cost > a.total_cost * 1.000_000_000_1
}

/// add_path with an unordered path `h` and one with pathkeys `s`: whether `h` stays
/// (fewer disabled nodes, or fuzzily cheaper in total cost, or fuzzily the same total but
/// cheaper to start; fuzzily the same, the ordered one dominates it).
pub(super) fn unsorted_kept(h: &ExNode, s: &ExNode) -> bool {
    const FUZZ: f64 = 1.01;
    let (dh, ds) = (disabled_nodes(h), disabled_nodes(s));
    if dh != ds {
        return dh < ds;
    }
    if s.total_cost > h.total_cost * FUZZ {
        return true;
    }
    if h.total_cost > s.total_cost * FUZZ {
        return false;
    }
    s.startup_cost > h.startup_cost * FUZZ
}

/// add_partial_path between two partial paths of the same pathkeys: `new` replaces `old`
/// with fewer disabled nodes, or when `old` costs more in total at all (startup costs
/// don't count; a tie keeps the one built first).
pub(super) fn partial_better(new: &ExNode, old: &ExNode) -> bool {
    let (dn, d_old) = (disabled_nodes(new), disabled_nodes(old));
    if dn != d_old {
        return dn < d_old;
    }
    old.total_cost > new.total_cost * 1.000_000_000_1
}

pub(super) fn cheaper(a: &ExNode, b: &ExNode) -> bool {
    let (da, db) = (disabled_nodes(a), disabled_nodes(b));
    if da != db {
        return da < db;
    }
    if a.total_cost != b.total_cost {
        return a.total_cost < b.total_cost;
    }
    a.startup_cost < b.startup_cost
}

/// A parallel plan `par` (its Gather isn't parallel safe) beats serial plan `ser`
/// (add_path): fewer disabled nodes, or fuzzily cheaper (by more than 1%) in total cost, or
/// fuzzily the same total but cheaper to start. Fuzzily equal, the parallel-safe serial
/// plan stays.
pub(super) fn parallel_wins(par: &ExNode, ser: &ExNode) -> bool {
    const FUZZ: f64 = 1.01;
    let (dp, ds) = (disabled_nodes(par), disabled_nodes(ser));
    if dp != ds {
        return dp < ds;
    }
    if ser.total_cost > par.total_cost * FUZZ {
        return true;
    }
    if par.total_cost > ser.total_cost * FUZZ {
        return false;
    }
    ser.startup_cost > par.startup_cost * FUZZ && par.total_cost < ser.total_cost
}

/// compare_pathkeys over two plans' orders: 0 the same, 1 `a`'s is better (`b`'s is a
/// leading part of it), 2 `b`'s is, 3 they differ.
fn compare_orders(a: &[String], b: &[String]) -> u8 {
    if a.iter().zip(b).any(|(x, y)| x != y) {
        return 3;
    }
    match a.len().cmp(&b.len()) {
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Less => 2,
    }
}

/// add_partial_path over partial plans: `new` kept in `list` unless one in a compatible
/// order dominates it (fewer disabled nodes, or fuzzily cheaper in total with an order at
/// least as good), dropping the ones it dominates; the list stays in total-cost order.
pub(super) fn add_partial_path(list: &mut Vec<ExNode>, new: ExNode) {
    const FUZZ: f64 = 1.01;
    let mut insert_at = 0;
    let mut i = 0;
    while i < list.len() {
        let old = &list[i];
        let mut remove_old = false;
        let mut accept = true;
        let kc = compare_orders(&new.order, &old.order);
        if kc != 3 {
            let (dn, d_old) = (disabled_nodes(&new), disabled_nodes(old));
            if dn != d_old {
                if dn > d_old {
                    accept = false;
                } else {
                    remove_old = true;
                }
            } else if new.total_cost > old.total_cost * FUZZ {
                accept = kc == 1;
            } else if old.total_cost > new.total_cost * FUZZ {
                remove_old = kc != 2;
            } else if kc == 1 {
                remove_old = true;
            } else if kc == 2 {
                accept = false;
            } else if old.total_cost > new.total_cost * 1.000_000_000_1 {
                remove_old = true;
            } else {
                accept = false;
            }
        }
        if !accept {
            return;
        }
        if remove_old {
            list.remove(i);
        } else {
            if new.total_cost >= list[i].total_cost {
                insert_at = i + 1;
            }
            i += 1;
        }
    }
    list.insert(insert_at, new);
}

/// add_path over `paths` in turn (compare_path_costs_fuzzily, compare_pathkeys over their
/// orders, then rows and parallel safety: a plan with a Gather in it isn't parallel safe,
/// so it never drops a serial one): the ones left, in the order add_path keeps them.
pub(super) fn add_paths(paths: Vec<ExNode>) -> Vec<ExNode> {
    const FUZZ: f64 = 1.01;
    // (1: `a` better, 2: `b` better, 0: the same, 3: each better in one cost.)
    let costs = |a: &ExNode, b: &ExNode, f: f64| -> u8 {
        let (da, db) = (disabled_nodes(a), disabled_nodes(b));
        if da != db {
            return if da < db { 1 } else { 2 };
        }
        if a.total_cost > b.total_cost * f {
            return if b.startup_cost > a.startup_cost * f { 3 } else { 2 };
        }
        if b.total_cost > a.total_cost * f {
            return if a.startup_cost > b.startup_cost * f { 3 } else { 1 };
        }
        if a.startup_cost > b.startup_cost * f {
            return 2;
        }
        if b.startup_cost > a.startup_cost * f {
            return 1;
        }
        0
    };
    let safe = |n: &ExNode| !has_gather(n);
    let mut kept: Vec<ExNode> = Vec::new();
    for new in paths {
        let mut accept = true;
        let mut i = 0;
        while i < kept.len() {
            let old = &kept[i];
            let mut remove_old = false;
            let cc = costs(&new, old, FUZZ);
            let kc = compare_orders(&new.order, &old.order);
            if cc != 3 && kc != 3 {
                let new_dominates = new.rows <= old.rows && safe(&new) >= safe(old);
                let old_dominates = new.rows >= old.rows && safe(&new) <= safe(old);
                match cc {
                    0 => {
                        if kc == 1 {
                            remove_old = new_dominates;
                        } else if kc == 2 {
                            accept = !old_dominates;
                        } else if safe(&new) != safe(old) {
                            if safe(&new) {
                                remove_old = true;
                            } else {
                                accept = false;
                            }
                        } else if new.rows < old.rows {
                            remove_old = true;
                        } else if new.rows > old.rows {
                            accept = false;
                        } else if costs(&new, old, 1.000_000_000_1) == 1 {
                            remove_old = true;
                        } else {
                            accept = false;
                        }
                    }
                    1 => remove_old = kc != 2 && new_dominates,
                    _ => accept = !(kc != 1 && old_dominates),
                }
            }
            if remove_old {
                kept.remove(i);
            } else {
                i += 1;
            }
            if !accept {
                break;
            }
        }
        if accept {
            kept.push(new);
        }
    }
    kept
}

impl Builder<'_, '_> {
    /// A Gather over partial plan `part` (cost_gather).
    pub(super) fn gather_of(&self, part: &ExNode) -> ExNode {
        let cfg = self.par.as_ref().expect("parallel settings");
        let rows = costs::clamp_rows(part.rows * cfg.divisor(part.workers));
        let mut g = ExNode::new("Gather");
        g.rows = rows;
        g.width = part.width;
        g.cols = part.cols.clone();
        g.startup_cost = part.startup_cost + cfg.setup_cost;
        g.total_cost = part.total_cost + cfg.setup_cost + cfg.tuple_cost * rows;
        g.workers = part.workers;
        g.details.push(Detail::Workers(part.workers));
        g.key = part.key;
        let mut child = part.clone();
        self.exact_tlist(&mut child);
        if self.o.verbose {
            g.output = child.output.iter().map(|o| refer_to(o)).collect();
        }
        g.children.push(child.child("Outer"));
        g
    }

    /// A Gather Merge over sorted partial plan `part` (cost_gather_merge).
    pub(super) fn gather_merge_of(&self, part: &ExNode) -> ExNode {
        let cfg = self.par.as_ref().expect("parallel settings");
        let rows = costs::clamp_rows(part.rows * cfg.divisor(part.workers));
        self.gather_merge_rows(part, rows)
    }

    /// `gather_merge_of` returning `rows` (create_gather_merge_path's own row count).
    pub(super) fn gather_merge_rows(&self, part: &ExNode, rows: f64) -> ExNode {
        let cfg = self.par.as_ref().expect("parallel settings");
        let mut g = ExNode::new("Gather Merge");
        g.rows = rows;
        g.width = part.width;
        g.cols = part.cols.clone();
        g.order = part.order.clone();
        let n = part.workers as f64 + 1.0;
        let log_n = n.log2();
        let comparison = 2.0 * costs::CPU_OPERATOR_COST;
        let startup = comparison * n * log_n + cfg.setup_cost;
        let run = rows * comparison * log_n
            + costs::CPU_OPERATOR_COST * rows
            + cfg.tuple_cost * rows * 1.05;
        g.startup_cost = startup + part.startup_cost;
        g.total_cost = startup + run + part.total_cost;
        g.workers = part.workers;
        g.details.push(Detail::Workers(part.workers));
        g.key = part.key;
        let mut child = part.clone();
        self.exact_tlist(&mut child);
        if self.o.verbose {
            g.output = child.output.iter().map(|o| refer_to(o)).collect();
        }
        g.children.push(child.child("Outer"));
        g
    }

    /// A scan under a Gather returns just the columns used above it (no physical target
    /// list: create_gather_plan asks for an exact one).
    fn exact_tlist(&self, n: &mut ExNode) {
        if n.is_scan()
            && let Some(m) = n.key.and_then(|k| self.needed.get(&k))
            && n.output.len() == m.len()
        {
            n.output = n
                .output
                .iter()
                .zip(m)
                .filter(|(_, keep)| **keep)
                .map(|(o, _)| o.clone())
                .collect();
        }
    }

    /// `n`, or a Gather over its partial plan when that is cheaper; the partial plan goes
    /// either way (its consumer can't use it).
    pub(super) fn settle(&self, n: &mut ExNode) {
        if !n.wanted_order.is_empty() && n.order.len() >= n.wanted_order.len() && self.par.is_some() {
            self.settle_ordered(n);
            return;
        }
        n.wanted_order.clear();
        n.partials.clear();
        let Some(part) = n.partial.take() else { return };
        if self.par.is_none() {
            return;
        }
        // (create_gather_path takes the relation's rows here, which is the same up to the
        // partial plan's rounding, except for an Append that run-time pruning cuts down: its
        // costs here are the members left's only, where Postgres's are all of them.)
        let mut g = self.gather_of(&part);
        // (create_gather_path returns the relation's rows. An Append run-time pruning cuts
        // down keeps the partial plan's: its costs here are the members left's only, where
        // Postgres's are all of them.)
        if n.node_type != "Append"
            && n.rows > 0.0
            && let Some(cfg) = self.par.as_ref()
        {
            g.rows = n.rows;
            g.total_cost = part.total_cost + cfg.setup_cost + cfg.tuple_cost * g.rows;
        }
        if parallel_wins(&g, n) {
            // (The relation's other paths, in an order, stay beside it.)
            g.alt = n.alt.take();
            *n = g;
        }
    }

    /// `part` (a partial plan) sorted on `list` for a Gather Merge, fully or (when its
    /// order already has `list`'s first keys and `incremental`) incrementally: key i
    /// reads column `cols[i]` of its rows when a plain column. `sort_key`: the counters.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn sorted_partial(
        &self,
        part: &ExNode,
        list: &[String],
        cols: &[Option<usize>],
        incremental: bool,
        sort_key: Option<usize>,
    ) -> ExNode {
        let pcommon = list.iter().zip(&part.order).take_while(|(a, b)| a == b).count();
        let incremental = incremental && pcommon > 0 && pcommon < list.len();
        let mut sn = ExNode::new(if incremental { "Incremental Sort" } else { "Sort" });
        sn.details.push(Detail::List("Sort Key", list.to_vec()));
        if incremental {
            sn.details.push(Detail::List(
                "Presorted Key",
                list[..pcommon].iter().map(|k| presorted_name(k)).collect(),
            ));
            sn.details.push(Detail::IncSortInfo);
            let pre: Vec<ColInfo> = cols[..pcommon.min(cols.len())]
                .iter()
                .map(|c| c.and_then(|j| part.cols.get(j).cloned()).unwrap_or_default())
                .collect();
            let groups = if pre.len() < pcommon || pre.iter().any(|x| x.var.is_none()) {
                part.rows.clamp(1.0, costs::DEFAULT_NUM_DISTINCT)
            } else {
                self.num_groups(&pre, part.rows)
            };
            (sn.startup_cost, sn.total_cost) = costs::incremental_sort_cost(
                part.startup_cost,
                part.total_cost,
                part.rows,
                part.width,
                groups,
                None,
            );
            sn.rows = part.rows;
            sn.width = part.width;
            sn.cols = part.cols.clone();
            sn.presorted_groups = Some(groups);
        } else {
            sn.details.push(Detail::SortInfo);
            self.sort_costs(&mut sn, part);
        }
        sn.order = list.to_vec();
        sn.workers = part.workers;
        sn.key = sort_key;
        sn.disabled = !incremental && !crate::sql::paths::Gucs::read(self.ctx).sort;
        if self.o.verbose {
            sn.output = part.output.clone();
        }
        sn.children.push(part.clone().child("Outer"));
        sn
    }

    /// create_ordered_paths' own Gather Merge under a LIMIT: the workers' sort of `gm`
    /// (a Gather Merge over a sorted partial plan) costed for the rows the LIMIT reads, the
    /// Gather Merge returning the partial rows times the parallel divisor.
    pub(super) fn bounded_gather_merge(&self, gm: &ExNode, limit: f64) -> Option<ExNode> {
        let cfg = self.par.as_ref()?;
        if gm.node_type != "Gather Merge" || limit <= 0.0 {
            return None;
        }
        let sn = gm.children.first()?;
        let input = sn.children.first()?;
        if input.workers == 0 {
            return None;
        }
        let mut sn = sn.clone();
        match sn.node_type {
            "Sort" => {
                (sn.startup_cost, sn.total_cost) =
                    costs::sort_cost(input.total_cost, input.rows, input.width, Some(limit));
            }
            "Incremental Sort" => {
                let groups = sn.presorted_groups?;
                (sn.startup_cost, sn.total_cost) = costs::incremental_sort_cost(
                    input.startup_cost,
                    input.total_cost,
                    input.rows,
                    input.width,
                    groups,
                    Some(limit),
                );
            }
            _ => return None,
        }
        sn.relationship = None;
        let rows = costs::clamp_rows(sn.rows * cfg.divisor(sn.workers));
        let mut g = self.gather_merge_rows(&sn, rows);
        g.output = gm.output.clone();
        g.children[0].output = gm.children[0].output.clone();
        Some(g)
    }

    /// generate_useful_gather_paths for the order `list` over a relation's partial plans
    /// (`part` the cheapest, `others` the ordered ones): a Gather Merge over each one
    /// already in that order, over the cheapest sorted, and over each other one in a
    /// leading part of the order sorted incrementally. `rows`: the relation's rows (None:
    /// the partial plan's times the parallel divisor).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn useful_gathers(
        &self,
        part: &ExNode,
        others: &[ExNode],
        list: &[String],
        cols: &[Option<usize>],
        rows: Option<f64>,
        sort_key: Option<usize>,
        inc_ok: bool,
    ) -> Vec<ExNode> {
        let mut out = Vec::new();
        if list.is_empty() || !self.par.as_ref().is_some_and(|p| p.gather_merge) {
            return out;
        }
        let incremental = inc_ok && crate::sql::paths::Gucs::read(self.ctx).incremental_sort;
        let gm = |sn: &ExNode| match rows {
            Some(r) => self.gather_merge_rows(sn, r),
            None => self.gather_merge_of(sn),
        };
        for (i, p) in std::iter::once(part).chain(others).enumerate() {
            let pcommon = list.iter().zip(&p.order).take_while(|(a, b)| a == b).count();
            if pcommon == list.len() {
                out.push(gm(p));
                continue;
            }
            if i > 0 && (pcommon == 0 || !incremental) {
                continue;
            }
            out.push(gm(&self.sorted_partial(p, list, cols, incremental, sort_key)));
        }
        out
    }

    /// A scan reading an index in the order the query's ORDER BY wants (the planner took
    /// it for the Sort): a plain Gather would lose that order, so its partial plans go
    /// only under a Gather Merge in it (generate_gather_paths / generate_useful_gather_paths
    /// at its relation), which it becomes when that is cheaper, the serial scan its
    /// alternative (a LIMIT above weighs both).
    pub(super) fn ordered_scan_gathers(&self, s: &ScanPlan, n: &mut ExNode) {
        let Some(u) = &s.useful.0 else { return };
        let want = &u.0;
        if want.is_empty() || n.order.len() < want.len() {
            return;
        }
        let ord = crate::sql::paths::index_order(self.ctx, s);
        let covered = ord.len() >= want.len()
            && want.iter().zip(&ord).all(|(w, k)| {
                w.desc == k.desc && w.nulls_first == k.nulls_first && w.expr.same_as(&k.expr)
            });
        if covered {
            n.wanted_order = want
                .iter()
                .map(|k| match &k.expr {
                    Expr::Col(j, _) => Some(*j),
                    _ => None,
                })
                .collect();
        }
    }

    /// `settle` of a scan in the order the query wants (`ExNode::wanted_order`).
    fn settle_ordered(&self, n: &mut ExNode) {
        let Some(part) = n.partial.take() else { return };
        let others = std::mem::take(&mut n.partials);
        let cols = std::mem::take(&mut n.wanted_order);
        let list: Vec<String> = n.order[..cols.len()].to_vec();
        let gms = self.useful_gathers(&part, &others, &list, &cols, Some(n.rows), n.key, true);
        let Some(mut gm) = gms.into_iter().reduce(|a, b| if cheaper(&b, &a) { b } else { a }) else {
            return;
        };
        if par_beats(&gm, n) {
            let serial = std::mem::take(n);
            gm.alt = Some(Box::new(serial));
            *n = gm;
        } else if gm.startup_cost < n.startup_cost {
            n.alt = Some(Box::new(gm));
        }
    }

    /// A parameterized scan a worker can run: besides parallel-safe values its quals read
    /// only the nested loop's outer row of this query level (nestloop parameters, which the
    /// workers have), not an enclosing query's values.
    pub(super) fn param_scan_safe(&self, s: &ScanPlan) -> bool {
        if self.par.is_none()
            || s.table.temporary
            || s.sample.is_some()
            || s.current_of.is_some()
            || s.mark.is_some()
            || s.table.is_foreign()
            || self.outer_depth.first() != Some(&self.depth)
        {
            return false;
        }
        let plan = Plan::Scan(Box::new(s.clone()));
        let mut exprs = Vec::new();
        node_exprs(&plan, &mut exprs);
        let mut reads_row = false;
        let cat = self.ctx.catalog();
        let safe = exprs.iter().all(|e| {
            let e = (*e)
                .clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Outer(1, _, t) => {
                            reads_row = true;
                            Expr::Const(crate::types::Value::Null, t)
                        }
                        x => x,
                    })
                })
                .unwrap_or_else(|_| (*e).clone());
            crate::sql::paths::parallel::hazard(&e, &cat) == 's'
        });
        safe && reads_row
    }

    /// A scan's partial plan (the cheapest partial path of its relation), as the scan
    /// node `serial` shows it but parallel-aware.
    pub(super) fn partial_scan(
        &mut self,
        p: &Arc<Plan>,
        s: &ScanPlan,
        names: &[Name],
        serial: &ExNode,
    ) -> Option<ExNode> {
        self.par.as_ref()?;
        let needed = self.needed.get(&addr(p)).cloned().unwrap_or_default();
        let ps = crate::sql::paths::parallel::partial_scan(self.ctx, s, &needed, self.member_scan)?;
        // (Parallel safe, though no worker to scan it.)
        let Some(ps) = ps else {
            return Some(ExNode::default());
        };
        Some(self.partial_scan_node(p, s, ps, names, serial))
    }

    /// The scan's other partial paths, kept for their order (`ExNode::partials`).
    pub(super) fn ordered_partial_scans(
        &mut self,
        p: &Arc<Plan>,
        s: &ScanPlan,
        names: &[Name],
        serial: &ExNode,
    ) -> Vec<ExNode> {
        if self.par.is_none() {
            return Vec::new();
        }
        let needed = self.needed.get(&addr(p)).cloned().unwrap_or_default();
        let list = crate::sql::paths::parallel::ordered_partial_scans(self.ctx, s, &needed, self.member_scan);
        list.into_iter().map(|ps| self.partial_scan_node(p, s, ps, names, serial)).collect()
    }

    fn partial_scan_node(
        &mut self,
        p: &Arc<Plan>,
        s: &ScanPlan,
        ps: crate::sql::paths::parallel::PartialScan,
        names: &[Name],
        serial: &ExNode,
    ) -> ExNode {
        let saved = std::mem::take(&mut self.pending);
        let mut n = self.planned_scan(p, &ps.scan, ps.desc, names);
        self.pending = saved;
        // (Its counters are the scan's own.)
        let (from, to) = (scan_addr(&ps.scan), scan_addr(s));
        for d in &mut n.details {
            match d {
                Detail::Removed(_, list) => {
                    for r in list.iter_mut() {
                        if let Removed::Scan(k) = r
                            && *k == from
                        {
                            *k = to;
                        }
                    }
                }
                Detail::IndexSearches(k) | Detail::HeapFetches(k) | Detail::HeapBlocks(k)
                    if *k == from =>
                {
                    *k = to
                }
                _ => {}
            }
        }
        n.title = format!("Parallel {}", n.title);
        n.parallel_aware = true;
        n.workers = ps.workers;
        n.width = serial.width;
        n.cols = serial.cols.clone();
        for c in &mut n.cols {
            if let Some(v) = &mut c.var {
                v.rows = n.rows;
            }
        }
        n.key = serial.key;
        n.subplans = serial.subplans.clone();
        n
    }

    /// The aggregation `serial` (over the serial input) as a Finalize over a Gather of a
    /// Partial aggregation of `part` (its input's partial plan), when the aggregates can
    /// be split and that is cheaper. `gnames` / `anames`: the group keys' and aggregates'
    /// names.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn parallel_agg(
        &mut self,
        serial: &ExNode,
        part: ExNode,
        others: &[ExNode],
        groups: &[Expr],
        aggs: &[AggCall],
        gnames: &[Name],
        anames: &[Name],
    ) -> Option<ExNode> {
        let cfg = self.par.clone()?;
        // (A user aggregate: with a combine function, a state it can pass (not internal,
        // or serialized), and parallel safe.)
        let cat0 = self.ctx.catalog();
        let partial_ok = |a: &AggCall| {
            // (agg_args_support_sendreceive: an internal state serialized with an anonymous
            // record argument can't be read back, record_recv needing its typmod.)
            let record_arg = a.args.iter().any(|e| e.ty().oid() == crate::types::oid::RECORD);
            if a.user.is_none() {
                return crate::sql::paths::parallel::agg_partial_ok(a.def.sig.oid)
                    && !(record_arg && crate::sql::paths::parallel::agg_internal_state(a.def.sig.oid));
            }
            if record_arg
                && cat0
                    .functions
                    .get(&a.def.sig.oid)
                    .and_then(|f| f.agg.as_ref())
                    .is_some_and(|g| g.transtype.oid() == 2281)
            {
                return false;
            }
            cat0.functions.get(&a.def.sig.oid).is_some_and(|f| {
                f.parallel == 's'
                    && f.agg.as_ref().is_some_and(|g| {
                        g.combinefn != 0 && (g.transtype.oid() != 2281 || g.serialfn != 0)
                    })
            })
        };
        // Only plain or grouped aggregation of aggregates that combine, over safe values.
        let strategy = serial.strategy.unwrap_or("Sorted");
        if !matches!(strategy, "Plain" | "Hashed" | "Sorted")
            || serial.children.len() != 1
            || aggs.iter().any(|a| {
                a.distinct
                    || !a.order_by.is_empty()
                    || !a.direct.is_empty()
                    || !partial_ok(a)
            })
        {
            return None;
        }
        let cat = self.ctx.catalog();
        let safe = |e: &Expr| crate::sql::paths::parallel::hazard(e, &cat) == 's';
        if !groups.iter().all(safe)
            || !aggs
                .iter()
                .all(|a| a.args.iter().chain(&a.filter).all(safe))
        {
            return None;
        }
        let trans = costs::CPU_OPERATOR_COST
            * aggs
                .iter()
                .map(|a| 1 + a.args.iter().map(costs::ops).sum::<usize>())
                .sum::<usize>() as f64;
        let combine = costs::CPU_OPERATOR_COST * aggs.len() as f64;
        let group_ops = costs::CPU_OPERATOR_COST * groups.len() as f64;
        // (The group expressions the grouping targets compute, per group: create_agg_path.)
        let gexpr: f64 = groups
            .iter()
            .filter(|g| !matches!(g, Expr::Col(..)))
            .map(costs::qual_cost)
            .sum();
        let in_rows = part.rows;
        let workers = part.workers;
        let g = crate::sql::paths::Gucs::read(self.ctx);
        let partial_out: Vec<String> = if self.o.verbose {
            gnames
                .iter()
                .map(|n| self.pick(n, Cx::Output))
                .chain(
                    anames
                        .iter()
                        .map(|n| format!("PARTIAL {}", self.pick(n, Cx::Output))),
                )
                .collect()
        } else {
            Vec::new()
        };
        let serial_keys: Vec<String> = serial
            .details
            .iter()
            .find_map(|d| match d {
                Detail::List("Group Key", k) => Some(k.clone()),
                _ => None,
            })
            .unwrap_or_default();
        // The keys (those the serial grouping shows) as a hashed partial grouping computes
        // them, and as the nodes over a computed one refer to it.
        let mut hkeys: Vec<String> = Vec::new();
        let mut keys: Vec<String> = Vec::new();
        let mut key_cols: Vec<Option<usize>> = Vec::new();
        for (g, n) in groups.iter().zip(gnames) {
            let bare = self.pick(n, Cx::Upper);
            let computed = !matches!(g, Expr::Col(..)) && !g.columns().is_empty();
            let wrapped = if computed { self.pick(&n.wrapped(), Cx::Upper) } else { bare.clone() };
            if serial_keys.contains(&bare) || serial_keys.contains(&wrapped) {
                hkeys.push(bare);
                keys.push(wrapped);
                key_cols.push(match g {
                    Expr::Col(j, _) => Some(*j),
                    _ => None,
                });
            }
        }
        if hkeys.len() != serial_keys.len() {
            hkeys = serial_keys.clone();
            keys = serial_keys.clone();
            key_cols = vec![None; keys.len()];
        }
        // An aggregation node like `serial`, of `kind` ("Plain", "Hashed", "Sorted") in
        // `mode` ("Partial", "Finalize"), over `input`.
        let make = |kind: &'static str, mode: &'static str, input: ExNode| -> ExNode {
            let mut a = serial.clone();
            a.children.clear();
            a.subplans.clear();
            a.partial = None;
            a.alt = None;
            a.node_type = "Aggregate";
            a.strategy = Some(kind);
            a.partial_mode = Some(mode);
            // (Sorted on the keys in their order: its groups come out so.)
            let sort_keys = if kind == "Sorted" { (0..groups.len()).map(|i| (i, false, false)).collect() } else { Vec::new() };
            super::put_group_sort(&mut a.details, sort_keys);
            let base = match kind {
                "Plain" => "Aggregate",
                "Hashed" => "HashAggregate",
                _ => "GroupAggregate",
            };
            a.title = format!("{mode} {base}");
            a.details.retain(|d| {
                !matches!(d, Detail::HashAggInfo(_) | Detail::List("Group Key", _))
            });
            if !keys.is_empty() {
                let shown = if mode == "Finalize" {
                    keys.iter().map(|k| refer_to(k)).collect()
                } else if kind == "Hashed" {
                    hkeys.clone()
                } else {
                    keys.clone()
                };
                a.details.insert(0, Detail::List("Group Key", shown));
            }
            if kind == "Hashed" {
                a.details.push(Detail::HashAggInfo(0));
            }
            a.disabled = kind == "Hashed" && !g.hashagg;
            a.order = if kind == "Sorted" { keys.clone() } else { Vec::new() };
            // (The aggregation runs once: its counters are its partial form's.)
            if a.key.is_none() {
                a.key = serial.key;
            }
            a.workers = input.workers;
            a.children.push(input.child("Outer"));
            a
        };
        let sorted = |b: &Self, input: ExNode| -> ExNode {
            let mut sn = ExNode::new("Sort");
            sn.details.push(Detail::List("Sort Key", keys.clone()));
            sn.details.push(Detail::SortInfo);
            b.sort_costs(&mut sn, &input);
            sn.order = keys.clone();
            sn.workers = input.workers;
            sn.disabled = !g.sort;
            sn.key = input.key;
            if b.o.verbose {
                sn.output = input.output.clone();
            }
            sn.children.push(input.child("Outer"));
            sn
        };
        let mut finals: Vec<ExNode> = Vec::new();
        if keys.is_empty() {
            // Plain aggregation.
            let mut pa = make("Plain", "Partial", part);
            pa.rows = 1.0;
            pa.startup_cost = pa.children[0].total_cost + in_rows * trans;
            pa.total_cost = pa.startup_cost + costs::CPU_TUPLE_COST;
            pa.output = partial_out.clone();
            let mut gt = self.gather_of(&pa);
            gt.rows = costs::clamp_rows(pa.rows * cfg.divisor(workers));
            gt.total_cost = pa.total_cost + cfg.setup_cost + cfg.tuple_cost * gt.rows;
            let mut f = make("Plain", "Finalize", gt);
            let gi = &f.children[0];
            f.startup_cost = gi.total_cost + gi.rows * combine;
            f.total_cost = f.startup_cost + costs::CPU_TUPLE_COST;
            f.rows = 1.0;
            finals.push(f);
        } else {
            // (estimate_num_groups over the keys, expressions by their columns.)
            let pgroups = self.group_num_groups(groups, &part.cols, in_rows);
            let ngroups = serial.rows;
            // The partial groups by hashing, and by sorting the input.
            let mut ph = make("Hashed", "Partial", part.clone());
            ph.rows = pgroups;
            ph.startup_cost = part.total_cost + in_rows * (trans + group_ops);
            ph.total_cost = ph.startup_cost + pgroups * (costs::CPU_TUPLE_COST + gexpr);
            // (cost_agg's disk costs when the groups won't fit in hash_mem, partial and
            // finalized alike: the transition states of all the aggregates.)
            let (num_trans, trans_space) = crate::sql::paths::parallel::agg_transitions(aggs);
            let (s, t, parts) = g.hashagg_spill(in_rows, part.width, pgroups, num_trans, trans_space);
            ph.startup_cost += s;
            ph.total_cost += t;
            super::plan_partitions(&mut ph.details, parts);
            ph.output = partial_out.clone();
            let si = sorted(self, part.clone());
            let mut ps = make("Sorted", "Partial", si);
            let sic = &ps.children[0];
            ps.rows = pgroups;
            ps.startup_cost = sic.startup_cost;
            ps.total_cost =
                sic.total_cost + in_rows * (trans + group_ops) + pgroups * (costs::CPU_TUPLE_COST + gexpr);
            ps.output = partial_out.clone();
            let gathered = |b: &Self, pa: &ExNode, merge: bool| -> ExNode {
                let mut gt = if merge { b.gather_merge_of(pa) } else { b.gather_of(pa) };
                let rows = costs::clamp_rows(pa.rows * cfg.divisor(workers));
                if merge {
                    let n = workers as f64 + 1.0;
                    let comparison = 2.0 * costs::CPU_OPERATOR_COST;
                    let startup = comparison * n * n.log2() + cfg.setup_cost;
                    let run = rows * comparison * n.log2()
                        + costs::CPU_OPERATOR_COST * rows
                        + cfg.tuple_cost * rows * 1.05;
                    gt.startup_cost = startup + pa.startup_cost;
                    gt.total_cost = startup + run + pa.total_cost;
                } else {
                    gt.total_cost = pa.total_cost + cfg.setup_cost + cfg.tuple_cost * rows;
                }
                gt.rows = rows;
                gt
            };
            let fin_hashed = |gt: ExNode| -> ExNode {
                let mut f = make("Hashed", "Finalize", gt);
                let gi = &f.children[0];
                f.startup_cost = gi.total_cost + gi.rows * (combine + group_ops);
                f.total_cost = f.startup_cost + ngroups * (costs::CPU_TUPLE_COST + gexpr);
                let (s, t, parts) = g.hashagg_spill(gi.rows, gi.width, ngroups, num_trans, trans_space);
                f.startup_cost += s;
                f.total_cost += t;
                super::plan_partitions(&mut f.details, parts);
                f.rows = ngroups;
                f
            };
            let fin_sorted = |gt: ExNode| -> ExNode {
                let mut f = make("Sorted", "Finalize", gt);
                let gi = &f.children[0];
                f.startup_cost = gi.startup_cost;
                f.total_cost = gi.total_cost
                    + gi.rows * (combine + group_ops)
                    + ngroups * (costs::CPU_TUPLE_COST + gexpr);
                f.rows = ngroups;
                f
            };
            // The partially grouped relation's partial paths (create_partial_grouping_paths
            // through add_partial_path: the sorted ones first, then the hashed one, which
            // goes when it is fuzzily as cheap as a sorted one, whose order is better).
            let mut partials: Vec<ExNode> = Vec::new();
            add_partial_path(&mut partials, ps);
            // Or grouping the other partial plans already in (a leading part of) the keys'
            // order (create_partial_grouping_paths' make_ordered_path).
            for po in others {
                let pc = keys.iter().zip(&po.order).take_while(|(a, b)| a == b).count();
                let si = if pc == keys.len() {
                    let mut si = po.clone();
                    si.relationship = None;
                    si
                } else if pc > 0 && g.incremental_sort {
                    self.sorted_partial(po, &keys, &key_cols, true, po.key)
                } else {
                    continue;
                };
                let mut ps2 = make("Sorted", "Partial", si);
                let sic = &ps2.children[0];
                ps2.rows = pgroups;
                ps2.startup_cost = sic.startup_cost;
                ps2.total_cost = sic.total_cost
                    + in_rows * (trans + group_ops)
                    + pgroups * (costs::CPU_TUPLE_COST + gexpr);
                ps2.output = partial_out.clone();
                add_partial_path(&mut partials, ps2);
            }
            add_partial_path(&mut partials, ph);
            // gather_grouping_paths: a Gather over the cheapest, finalized by hashing
            // (its cheapest total path); a Gather Merge over each sorted one, and over the
            // cheapest sorted when it isn't, finalized in order.
            if let Some(first) = partials.first() {
                finals.push(fin_hashed(gathered(self, first, false)));
                if first.order.is_empty() {
                    let sph = sorted(self, first.clone());
                    finals.push(fin_sorted(gathered(self, &sph, true)));
                }
            }
            for pa in partials.iter().filter(|pa| !pa.order.is_empty()) {
                finals.push(fin_sorted(gathered(self, pa, true)));
            }
        }
        // (add_path: fuzzily the same cost, the one in order stays.)
        let mut best: Option<ExNode> = None;
        for f in finals {
            if best.as_ref().is_none_or(|b| fuzzy_better(&f, b)) {
                best = Some(f);
            }
        }
        let f = best?;
        // (Over a Gather already, the serial plan isn't parallel safe either: plain cost
        // decides.)
        par_beats(&f, serial).then_some(f)
    }
}

impl Builder<'_, '_> {
    /// An Append's partial plans (add_paths_to_append_rel): an Append of its members'
    /// partial plans, parallel-aware when enable_parallel_append (members spread over the
    /// workers), and a Parallel Append that runs some members whole, each in one process,
    /// where that is cheaper than splitting them. The cheaper of them.
    pub(super) fn partial_append(&self, serial: &ExNode, members: &[ExNode]) -> Option<ExNode> {
        let cfg = self.par.clone()?;
        if members.is_empty() || members.iter().any(|m| !m.par_safe) {
            return None;
        }
        let fls = |n: usize| (usize::BITS - n.leading_zeros()) as usize;
        let mut best: Option<ExNode> = None;
        let mut partial_rows: Option<f64> = None;
        // Every member partial.
        if members.iter().all(|m| m.partial.is_some()) {
            let subs: Vec<ExNode> = members
                .iter()
                .map(|m| (**m.partial.as_ref().unwrap()).clone())
                .collect();
            let mut workers = subs.iter().map(|x| x.workers).max().unwrap_or(1);
            if cfg.append {
                workers = workers.max(fls(members.len())).min(cfg.max_workers);
            }
            let n = self.append_node(serial, subs, Vec::new(), workers, cfg.append, None, &cfg);
            partial_rows = Some(n.rows);
            best = Some(n);
        }
        // Members run whole where a worker of their own does it faster.
        if cfg.append {
            let mut partial = Vec::new();
            let mut whole = Vec::new();
            for m in members {
                match &m.partial {
                    Some(pm) if pm.total_cost < m.total_cost => partial.push((**pm).clone()),
                    _ => {
                        let mut w = m.clone();
                        w.partial = None;
                        whole.push(w);
                    }
                }
            }
            if !whole.is_empty() {
                let workers = partial
                    .iter()
                    .map(|x| x.workers)
                    .max()
                    .unwrap_or(0)
                    .max(fls(members.len()))
                    .min(cfg.max_workers);
                if workers > 0 {
                    let n = self.append_node(serial, partial, whole, workers, true, partial_rows, &cfg);
                    if best.as_ref().is_none_or(|b| n.total_cost < b.total_cost) {
                        best = Some(n);
                    }
                }
            }
        }
        best
    }

    /// create_append_path + cost_append of a partial Append.
    #[allow(clippy::too_many_arguments)]
    fn append_node(
        &self,
        serial: &ExNode,
        mut partial: Vec<ExNode>,
        mut whole: Vec<ExNode>,
        workers: usize,
        aware: bool,
        rows: Option<f64>,
        cfg: &crate::sql::paths::parallel::ParCfg,
    ) -> ExNode {
        let mut n = serial.clone();
        n.children.clear();
        n.partial = None;
        n.workers = workers;
        n.parallel_aware = aware;
        if aware {
            n.title = "Parallel Append".into();
            // Members run whole by descending total cost, then the partial ones by
            // descending startup cost (in their relids' order when equal:
            // append_total_cost_compare, append_startup_cost_compare).
            whole.sort_by(|a, b| {
                b.total_cost
                    .total_cmp(&a.total_cost)
                    .then(b.startup_cost.total_cmp(&a.startup_cost))
                    .then(a.rank.cmp(&b.rank))
            });
            partial.sort_by(|a, b| {
                b.startup_cost
                    .total_cmp(&a.startup_cost)
                    .then(b.total_cost.total_cmp(&a.total_cost))
                    .then(a.rank.cmp(&b.rank))
            });
            let div = cfg.divisor(workers);
            let first_partial = whole.len();
            let subs: Vec<ExNode> = whole.into_iter().chain(partial).collect();
            let mut r = 0.0;
            let mut total = 0.0;
            let mut startup = 0.0;
            for (i, sp) in subs.iter().enumerate() {
                if i == 0 {
                    startup = sp.startup_cost;
                } else if i < workers {
                    startup = f64::min(startup, sp.startup_cost);
                }
                if i < first_partial {
                    r += sp.rows / div;
                } else {
                    r += sp.rows * (cfg.divisor(sp.workers) / div);
                    total += sp.total_cost;
                }
                r = costs::clamp_rows(r);
            }
            total += append_nonpartial_cost(&subs, first_partial, workers);
            let rows = rows.unwrap_or(r);
            n.rows = rows;
            n.startup_cost = startup;
            n.total_cost = total + costs::CPU_TUPLE_COST * 0.5 * rows;
            n.children = subs.into_iter().map(|c| c.child("Member")).collect();
        } else {
            n.title = "Append".into();
            n.rows = partial.iter().map(|s| s.rows).sum();
            n.startup_cost = partial.first().map_or(0.0, |s| s.startup_cost);
            n.total_cost = partial.iter().map(|s| s.total_cost).sum::<f64>()
                + costs::CPU_TUPLE_COST * 0.5 * n.rows;
            n.children = partial.into_iter().map(|c| c.child("Member")).collect();
        }
        n
    }
}

/// append_nonpartial_cost: the members run whole share out the workers.
fn append_nonpartial_cost(subs: &[ExNode], numpaths: usize, workers: usize) -> f64 {
    if numpaths == 0 {
        return 0.0;
    }
    let arrlen = workers.min(numpaths);
    let mut costs: Vec<f64> = subs.iter().take(arrlen).map(|s| s.total_cost).collect();
    let mut min_index = arrlen - 1;
    for sp in subs.iter().take(numpaths).skip(arrlen) {
        costs[min_index] += sp.total_cost;
        min_index = 0;
        for i in 0..arrlen {
            if costs[i] < costs[min_index] {
                min_index = i;
            }
        }
    }
    costs.into_iter().fold(0.0, f64::max)
}

/// The relation a scan reads, as one join group's trees both have it (`JoinMeta::par_alt`).
fn scan_ident(s: &ScanPlan) -> (usize, u32, u32, Option<String>) {
    (s.rti.0, s.rtindex.0, s.table.oid, s.alias.clone())
}

impl Builder<'_, '_> {
    /// The scans of a join group's other tree (`JoinMeta::par_alt`) named as
    /// the same relations' scans in the group's plan `main`.
    fn alt_refnames(&mut self, main: &Plan, alt: &Plan) {
        fn scans<'p>(p: &'p Plan, out: &mut Vec<&'p Plan>) {
            if let Plan::Scan(_) = p {
                out.push(p);
            }
            for c in plan_inputs(p) {
                scans(c, out);
            }
        }
        let (mut ms, mut alts) = (Vec::new(), Vec::new());
        scans(main, &mut ms);
        scans(alt, &mut alts);
        for a in alts {
            let Plan::Scan(sa) = a else { continue };
            let id = scan_ident(sa);
            let name = ms.iter().find_map(|m| match m {
                Plan::Scan(sm) if scan_ident(sm) == id => self.refnames.get(&addr(m)).cloned(),
                _ => None,
            });
            if let Some(n) = name {
                self.refnames.insert(addr(a), n);
            }
        }
    }
}

impl Builder<'_, '_> {
    /// A join group's partial plans along its cheapest partial path's tree
    /// (`JoinMeta::par_alt`) beside those along its plan's (add_partial_path over both).
    /// Not under ANALYZE: the executor runs the plan's tree, whose counts the nodes show.
    #[inline(never)]
    /// (`cost`: the search's estimate of the tree's partial path. Clearly dearer than the
    /// plan's own partial plan, the tree isn't built: add_partial_path would drop it.)
    pub(super) fn par_alt_partials(&mut self, b: &mut Built, main: &Plan, alt: &Arc<Plan>, cost: f64) {
        if self.o.analyze || self.par.is_none() {
            return;
        }
        if b.node.partial.as_ref().is_some_and(|own| cost > own.total_cost * 1.05) {
            return;
        }
        self.alt_refnames(main, alt);
        let saved = std::mem::take(&mut self.pending);
        let a = self.node(alt);
        self.pending = saved;
        let same_names = a.exported.len() == b.exported.len()
            && a.exported.iter().zip(&b.exported).all(|(x, y)| x.full == y.full);
        let Some(ap) = a.node.partial.filter(|_| same_names) else { return };
        let mut list: Vec<ExNode> = b.node.partial.take().map(|p| *p).into_iter().collect();
        list.append(&mut b.node.partials);
        for p in std::iter::once(*ap).chain(a.node.partials) {
            add_partial_path(&mut list, p);
        }
        if !list.is_empty() {
            b.node.partial = Some(Box::new(list.remove(0)));
        }
        b.node.partials = list;
    }
}

thread_local! {
    /// While `analyzed_plan` asks: the plan nodes the explained plan shows.
    pub(super) static SHOWN_KEYS: std::cell::RefCell<Option<std::collections::HashSet<usize>>> =
        const { std::cell::RefCell::new(None) };
}

/// Notes the plan nodes `n` shows, when `analyzed_plan` asks.
pub(super) fn note_shown(n: &ExNode) {
    fn walk(n: &ExNode, set: &mut std::collections::HashSet<usize>) {
        set.extend(n.key);
        for c in n.children.iter().chain(&n.subplans) {
            walk(c, set);
        }
    }
    SHOWN_KEYS.with(|s| {
        if let Some(set) = s.borrow_mut().as_mut() {
            walk(n, set);
        }
    });
}

/// The plan EXPLAIN ANALYZE runs: the statement's plan with each join group whose
/// parallel plan EXPLAIN shows along its partial path's tree (`JoinMeta::par_alt`) running
/// that tree, so the nodes shown are the nodes run. None: the plan as it is.
pub fn analyzed_plan(
    plan: &Arc<Plan>,
    hidden_rtes: usize,
    o: &Options,
    ctx: &ExecCtx,
    params: &[crate::types::Value],
) -> Option<Arc<Plan>> {
    fn has_alt(p: &Plan) -> bool {
        matches!(p, Plan::Join { meta, .. } if meta.par_alt.0.is_some()) || plan_inputs(p).into_iter().any(|c| has_alt(c))
    }
    fn keys(p: &Plan, out: &mut Vec<usize>) {
        out.push(addr(p));
        for c in plan_inputs(p) {
            keys(c, out);
        }
    }
    fn swap(p: &mut Plan, shown: &std::collections::HashSet<usize>) -> bool {
        if let Plan::Join { meta, .. } = p
            && let Some(alt) = meta.par_alt.0.clone()
        {
            let mut ks = Vec::new();
            keys(&alt, &mut ks);
            if ks.iter().any(|k| shown.contains(k)) {
                *p = (*alt).clone();
                return true;
            }
        }
        let mut any = false;
        for c in plan_inputs_mut(p) {
            if has_alt(c) {
                any |= swap(Arc::make_mut(c), shown);
            }
        }
        any
    }
    if !has_alt(plan) {
        return None;
    }
    let mut o = o.clone();
    o.analyze = false;
    o.format = Format::Text;
    SHOWN_KEYS.with(|s| *s.borrow_mut() = Some(Default::default()));
    let _ = explain(plan, hidden_rtes, &o, ctx, params, None, 0.0, 0.0);
    let shown = SHOWN_KEYS.with(|s| s.borrow_mut().take())?;
    let mut p = (**plan).clone();
    swap(&mut p, &shown).then(|| Arc::new(p))
}
