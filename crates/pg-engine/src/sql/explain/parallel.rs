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
        if self.o.verbose {
            g.output = part.output.iter().map(|o| refer_to(o)).collect();
        }
        g.key = part.key;
        let mut child = part.clone();
        self.exact_tlist(&mut child);
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
        if self.o.verbose {
            g.output = part.output.iter().map(|o| refer_to(o)).collect();
        }
        g.key = part.key;
        let mut child = part.clone();
        self.exact_tlist(&mut child);
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
        let Some(part) = n.partial.take() else { return };
        if self.par.is_none() {
            return;
        }
        // (create_gather_path takes the relation's rows here, which is the same up to the
        // partial plan's rounding, except for an Append that run-time pruning cuts down: its
        // costs here are the members left's only, where Postgres's are all of them.)
        let g = self.gather_of(&part);
        if parallel_wins(&g, n) {
            *n = g;
        }
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
        Some(n)
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
            if a.user.is_none() {
                return crate::sql::paths::parallel::agg_partial_ok(a.def.sig.oid);
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
        for (g, n) in groups.iter().zip(gnames) {
            let bare = self.pick(n, Cx::Upper);
            let computed = !matches!(g, Expr::Col(..)) && !g.columns().is_empty();
            let wrapped = if computed { self.pick(&n.wrapped(), Cx::Upper) } else { bare.clone() };
            if serial_keys.contains(&bare) || serial_keys.contains(&wrapped) {
                hkeys.push(bare);
                keys.push(wrapped);
            }
        }
        if hkeys.len() != serial_keys.len() {
            hkeys = serial_keys.clone();
            keys = serial_keys.clone();
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
            let base = match kind {
                "Plain" => "Aggregate",
                "Hashed" => "HashAggregate",
                _ => "GroupAggregate",
            };
            a.title = format!("{mode} {base}");
            a.details.retain(|d| {
                !matches!(d, Detail::HashAggInfo | Detail::List("Group Key", _))
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
                a.details.push(Detail::HashAggInfo);
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
            // Hashed partial groups gathered, finalized by hashing.
            finals.push(fin_hashed(gathered(self, &ph, false)));
            // Sorted partial groups merged, finalized in order.
            finals.push(fin_sorted(gathered(self, &ps, true)));
            // Hashed partial groups each sorted, then merged.
            let sph = sorted(self, ph.clone());
            finals.push(fin_sorted(gathered(self, &sph, true)));
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
            // descending startup cost (members in order when equal).
            whole.sort_by(|a, b| {
                b.total_cost
                    .total_cmp(&a.total_cost)
                    .then(b.startup_cost.total_cmp(&a.startup_cost))
            });
            partial.sort_by(|a, b| {
                b.startup_cost
                    .total_cmp(&a.startup_cost)
                    .then(b.total_cost.total_cmp(&a.total_cost))
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
