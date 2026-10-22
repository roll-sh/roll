//! Partitionwise aggregation (create_partitionwise_grouping_paths): a grouping over a
//! partitioned table's Append done in each partition, fully when the GROUP BY holds the
//! partition key (each group lives in one partition), else partially, a Finalize
//! aggregation combining the partitions' groups. It competes with grouping the Append by
//! cost. EXPLAIN shows it; the executor groups the Append (the same groups).

use super::*;

/// For an inheritance Append's members, the partition level whose parent is the
/// Append's table (by member: its index among the member's levels), and the members
/// grouped by the partition of that table they are in (a sub-partitioned partition's
/// several leaves, in order).
pub(super) fn own_levels(members: &[&Arc<Plan>]) -> Option<(Vec<usize>, Vec<Vec<usize>>)> {
    let levels: Vec<&Vec<crate::sql::plan::PartLevel>> = members
        .iter()
        .map(|m| member_scan(m).and_then(|s| s.part.as_deref()))
        .collect::<Option<_>>()?;
    // The deepest parent every member's levels have.
    let parent = levels[0]
        .iter()
        .map(|l| l.parent.oid)
        .find(|p| levels.iter().all(|ls| ls.iter().any(|l| l.parent.oid == *p)))?;
    let idx: Vec<usize> = levels
        .iter()
        .map(|ls| ls.iter().position(|l| l.parent.oid == parent).unwrap())
        .collect();
    let mut groups: Vec<(u32, Vec<usize>)> = Vec::new();
    for (i, ls) in levels.iter().enumerate() {
        // (The partition directly under the Append's table.)
        let part = if idx[i] == 0 {
            member_scan(members[i]).map_or(0, |s| s.table.oid)
        } else {
            ls[idx[i] - 1].parent.oid
        };
        match groups.iter_mut().find(|g| g.0 == part) {
            Some(g) => g.1.push(i),
            None => groups.push((part, vec![i])),
        }
    }
    Some((idx, groups.into_iter().map(|g| g.1).collect()))
}

/// A join (through projections and filters).
fn is_join(p: &Plan) -> bool {
    match p {
        Plan::Join { .. } => true,
        Plan::Project { input, .. } | Plan::Filter { input, .. } => is_join(input),
        _ => false,
    }
}

/// The partitioned Append a grouping's input reads (through projections and filters):
/// an inheritance tree's, or a partitionwise join's.
fn inh_append(p: &Arc<Plan>) -> Option<&Arc<Plan>> {
    match &**p {
        Plan::Append {
            inputs,
            inh: Some(_),
        } if inputs.len() > 1
            && !matches!(&*inputs[0], Plan::Update(_) | Plan::Delete(_)) =>
        {
            Some(p)
        }
        Plan::Append { inputs, inh: None } if inputs.len() > 1 && inputs.iter().all(|i| is_join(i)) => Some(p),
        Plan::Project { input, .. } | Plan::Filter { input, .. } => inh_append(input),
        _ => None,
    }
}

/// The table column a join's output column `k` is, and whether an outer join may null
/// it.
fn origin(p: &Plan, k: usize) -> Option<(&ScanPlan, usize, bool)> {
    match p {
        Plan::Scan(s) => Some((s, k, false)),
        Plan::Project { input, exprs } => match exprs.get(k)? {
            Expr::Col(j, _) => origin(input, *j),
            Expr::Relabel(x, _) if matches!(&**x, Expr::Col(..)) => {
                let Expr::Col(j, _) = &**x else { unreachable!() };
                origin(input, *j)
            }
            _ => None,
        },
        Plan::Filter { input, .. } => origin(input, k),
        Plan::Join {
            kind,
            left,
            right,
            nleft,
            ..
        } => {
            if k < *nleft {
                let (s, c, n) = origin(left, k)?;
                Some((s, c, n || matches!(kind, JoinKind::Right | JoinKind::Full)))
            } else {
                let (s, c, n) = origin(right, k - nleft)?;
                Some((s, c, n || matches!(kind, JoinKind::Left | JoinKind::Full)))
            }
        }
        _ => None,
    }
}

/// The table scans in a join tree.
fn scans_in<'p>(p: &'p Plan, out: &mut Vec<&'p ScanPlan>) {
    match p {
        Plan::Scan(s) => out.push(s),
        Plan::Project { input, .. } | Plan::Filter { input, .. } => scans_in(input, out),
        Plan::Join { left, right, .. } => {
            scans_in(left, out);
            scans_in(right, out);
        }
        _ => {}
    }
}

/// `p` (down to the Append) with `member` in the Append's place.
fn with_member(p: &Arc<Plan>, member: &Arc<Plan>) -> Arc<Plan> {
    match &**p {
        Plan::Append { .. } => member.clone(),
        Plan::Project { input, exprs } => Arc::new(Plan::Project {
            input: with_member(input, member),
            exprs: exprs.clone(),
        }),
        Plan::Filter { input, pred } => Arc::new(Plan::Filter {
            input: with_member(input, member),
            pred: pred.clone(),
        }),
        _ => p.clone(),
    }
}

/// An expression over `p`'s row as the Append's columns (through the projections).
fn down_to_append(e: &Expr, p: &Plan) -> Expr {
    match p {
        Plan::Project { input, exprs } => {
            let e = e
                .clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(i, t) => exprs.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                        x => x,
                    })
                })
                .unwrap();
            down_to_append(&e, input)
        }
        Plan::Filter { input, .. } => down_to_append(e, input),
        _ => e.clone(),
    }
}

/// A member's table scan.
pub(super) fn member_scan_of(p: &Plan) -> Option<&ScanPlan> {
    member_scan(p)
}

fn member_scan(p: &Plan) -> Option<&ScanPlan> {
    match p {
        Plan::Scan(s) => Some(s),
        Plan::Project { input, .. } | Plan::Filter { input, .. } => member_scan(input),
        _ => None,
    }
}

fn strip(e: &Expr) -> &Expr {
    match e {
        Expr::Relabel(x, _) => strip(x),
        e => e,
    }
}

/// cost_append of an unordered Append.
fn append_of(kids: Vec<ExNode>) -> ExNode {
    // (A sub-partitioned partition's own Append of groupings: its members join this one.)
    let kids: Vec<ExNode> = kids
        .into_iter()
        .flat_map(|k| {
            if matches!(k.node_type, "Append" | "Merge Append") {
                k.children
            } else {
                vec![k]
            }
        })
        .collect();
    let mut a = ExNode::new("Append");
    a.startup_cost = kids.first().map_or(0.0, |k| k.startup_cost);
    for k in &kids {
        a.rows += k.rows;
        a.total_cost += k.total_cost;
        a.width = a.width.max(k.width);
    }
    a.total_cost += a.rows * costs::CPU_TUPLE_COST * 0.5;
    a.cols = kids.first().map(|k| k.cols.clone()).unwrap_or_default();
    a.children = kids.into_iter().map(|k| k.child("Member")).collect();
    a
}

/// cost_merge_append of children in order `keys`.
fn merge_append_of(kids: Vec<ExNode>, keys: Vec<String>) -> ExNode {
    let mut a = ExNode::new("Merge Append");
    let n = (kids.len() as f64).max(2.0);
    let cmp = 2.0 * costs::CPU_OPERATOR_COST;
    let logn = n.log2();
    let (mut startup, mut total) = (0.0, 0.0);
    for k in &kids {
        startup += k.startup_cost;
        total += k.total_cost;
        a.rows += k.rows;
        a.width = a.width.max(k.width);
    }
    a.startup_cost = startup + cmp * n * logn;
    a.total_cost = cmp * n * logn + total + a.rows * cmp * logn + costs::CPU_TUPLE_COST * 0.5 * a.rows;
    a.cols = kids.first().map(|k| k.cols.clone()).unwrap_or_default();
    a.details.push(Detail::List("Sort Key", keys.clone()));
    a.order = keys;
    a.children = kids.into_iter().map(|k| k.child("Member")).collect();
    a
}

/// A grouping node's sorted form, when it or its alternative is one.
fn sorted_form(n: &ExNode) -> Option<ExNode> {
    let sorted = |x: &ExNode| matches!(x.strategy, Some("Sorted")) || x.node_type == "Group";
    if sorted(n) {
        return Some(n.clone());
    }
    n.alt.as_deref().filter(|a| sorted(a)).cloned()
}

/// add_path over `paths` in turn (compare_path_costs_fuzzily, then their orders: one in
/// the order of `keys` beats one in none): the ones left.
pub(super) fn add_path_survivors(paths: Vec<ExNode>, keys: &[String]) -> Vec<ExNode> {
    #[derive(PartialEq)]
    enum C {
        Better1,
        Better2,
        Equal,
        Different,
    }
    const FUZZ: f64 = 1.01;
    let costs = |a: &ExNode, b: &ExNode, f: f64| -> C {
        let (da, db) = (parallel::disabled_nodes(a), parallel::disabled_nodes(b));
        if da != db {
            return if da < db { C::Better1 } else { C::Better2 };
        }
        if a.total_cost > b.total_cost * f {
            return if b.startup_cost > a.startup_cost * f { C::Different } else { C::Better2 };
        }
        if b.total_cost > a.total_cost * f {
            return if a.startup_cost > b.startup_cost * f { C::Different } else { C::Better1 };
        }
        if a.startup_cost > b.startup_cost * f {
            return C::Better2;
        }
        if b.startup_cost > a.startup_cost * f {
            return C::Better1;
        }
        C::Equal
    };
    let sorted = |n: &ExNode| !keys.is_empty() && n.order.len() >= keys.len() && n.order[..keys.len()] == keys[..];
    let mut kept: Vec<ExNode> = Vec::new();
    for new in paths {
        let mut accept = true;
        let mut i = 0;
        while i < kept.len() {
            let old = &kept[i];
            // (Their orders: 1 the new one's is better, 2 the old one's, 0 the same.)
            let keyscmp = match (sorted(&new), sorted(old)) {
                (true, false) => 1,
                (false, true) => 2,
                _ => 0,
            };
            let mut remove_old = false;
            match costs(&new, old, FUZZ) {
                C::Better1 if keyscmp != 2 => remove_old = true,
                C::Better2 if keyscmp != 1 => accept = false,
                C::Equal => match keyscmp {
                    1 => remove_old = true,
                    2 => accept = false,
                    _ => {
                        if costs(&new, old, 1.000_000_000_1) == C::Better1 {
                            remove_old = true;
                        } else {
                            accept = false;
                        }
                    }
                },
                _ => {}
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
    /// The partitions' partial groupings (of `kids`, each gathered from workers on its
    /// own, under a Finalize of its own when the partition was grouped whole) as one
    /// Parallel Append under a Gather (Merge, sorted), finalized: the candidates, and the
    /// partial groupings appended.
    fn par_partial_cands(
        &self,
        kids: &[ExNode],
        serial: &ExNode,
        keys: &[String],
        finalize: &dyn Fn(ExNode, ExNode, bool) -> ExNode,
    ) -> (Vec<ExNode>, usize) {
        let g = crate::sql::paths::Gucs::read(self.ctx);
        // The partitions' partial groupings run by workers (each a Gather over its
        // own): one Parallel Append of them under a Gather (Merge, sorted) instead.
        // (A sub-partitioned partition's: the partial groupings of its own Parallel
        // Append, joining this one.)
        let par_parts: Option<Vec<Vec<ExNode>>> = kids
            .iter()
            .map(|k| {
                // (A partition grouped whole: its partial groupings under its Finalize.)
                let k = match k.partial_mode {
                    Some("Finalize") if k.children.len() == 1 => &k.children[0],
                    _ => k,
                };
                if !matches!(k.node_type, "Gather" | "Gather Merge") || k.children.len() != 1 {
                    return None;
                }
                let mut x = k.children[0].clone();
                while x.node_type == "Sort" && x.children.len() == 1 {
                    x = x.children.pop().unwrap();
                }
                if x.title == "Parallel Append"
                    && !x.children.is_empty()
                    && x.children.iter().all(|c| c.partial_mode == Some("Partial"))
                {
                    return Some(x.children);
                }
                (x.partial_mode == Some("Partial")).then(|| vec![x])
            })
            .collect();
        let mut cands: Vec<ExNode> = Vec::new();
        let mut par_leaves = 0;
        if let Some(subs) = par_parts
            && self.par.is_some()
        {
            let mut subs: Vec<ExNode> = subs.into_iter().flatten().collect();
            par_leaves = subs.len();
            let cfg = self.par.clone().expect("checked");
            let workers = subs.iter().map(|x| x.workers).max().unwrap_or(1).max(1);
            subs.sort_by(|a, b| b.total_cost.total_cmp(&a.total_cost));
            let div = cfg.divisor(workers);
            let mut pa = ExNode::new("Append");
            pa.title = "Parallel Append".into();
            pa.parallel_aware = true;
            pa.workers = workers;
            pa.rows = subs.iter().map(|x| x.rows * (cfg.divisor(x.workers) / div)).sum();
            pa.startup_cost = subs.iter().map(|x| x.startup_cost).fold(f64::INFINITY, f64::min);
            pa.total_cost = subs.iter().map(|x| x.total_cost).sum::<f64>() + costs::CPU_TUPLE_COST * 0.5 * pa.rows;
            pa.width = subs.iter().map(|x| x.width).max().unwrap_or(0);
            pa.cols = subs.first().map(|x| x.cols.clone()).unwrap_or_default();
            pa.children = subs.into_iter().map(|c| c.child("Member")).collect();
            // (Both finalizations: a sorted one shaped from the hashed one when the
            // serial grouping has none.)
            let mut forms: Vec<ExNode> = std::iter::once(serial).chain(serial.alt.as_deref()).cloned().collect();
            if !forms.iter().any(|f| f.strategy == Some("Sorted"))
                && let Some(h) = forms.iter().find(|f| f.strategy == Some("Hashed"))
            {
                let mut f = h.clone();
                f.strategy = Some("Sorted");
                f.title = f.title.replace("HashAggregate", "GroupAggregate");
                f.details.retain(|d| !matches!(d, Detail::HashAggInfo));
                forms.push(f);
            }
            for form in &forms {
                let mut f = form.clone();
                f.children.clear();
                if form.strategy == Some("Hashed") {
                    let g = self.gather_of(&pa);
                    cands.push(finalize(f, g, true));
                } else {
                    let mut sn = ExNode::new("Sort");
                    sn.details.push(Detail::List("Sort Key", keys.to_vec()));
                    sn.details.push(Detail::SortInfo);
                    self.sort_costs(&mut sn, &pa);
                    sn.order = keys.to_vec();
                    sn.workers = pa.workers;
                    sn.children.push(pa.clone().child("Outer"));
                    let gm = self.gather_merge_of(&sn);
                    cands.push(finalize(f, gm, false));
                }
            }
        }
        let _ = &g;
        (cands, par_leaves)
    }

    /// A grouping's total cost as the ORDER BY over it sees it (`ordered`: one of its
    /// keys, `keys`): with a sort when it doesn't come in that order.
    fn ordered_cost(&self, n: &ExNode, keys: &[String], ordered: bool) -> f64 {
        if !ordered || (n.order.len() >= keys.len() && n.order[..keys.len()] == keys[..]) {
            return n.total_cost;
        }
        let mut s = ExNode::new("Sort");
        self.sort_costs(&mut s, n);
        s.total_cost
    }

    /// The partitionwise forms of the grouping `serial` (over `input`) when one is
    /// cheaper: the full one (an Append of the partitions' groupings, `having` checked
    /// in each) or the partial one (a Finalize grouping over the partitions' partial
    /// ones).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn partitionwise_agg(
        &mut self,
        input: &Arc<Plan>,
        groups: &[Expr],
        aggs: &[AggCall],
        serial: &ExNode,
        having: Option<&Expr>,
        keys: &[String],
        hint_full: bool,
    ) -> Option<ExNode> {
        {
            let st = self.ctx.sess.settings.borrow();
            if !st
                .get_raw("enable_partitionwise_aggregate")
                .is_some_and(|v| v == "on" || v == "true")
            {
                return None;
            }
        }
        if groups.is_empty() || matches!(serial.node_type, "Gather" | "Gather Merge") {
            return None;
        }
        let app = inh_append(input)?;
        let Plan::Append { inputs, .. } = &**app else {
            return None;
        };
        let members: Vec<&Arc<Plan>> = inputs
            .iter()
            .filter(|m| !matches!(&***m, Plan::Empty { .. }))
            .collect();
        if members.is_empty() {
            return None;
        }
        // Full when the GROUP BY holds every partition key column (group_by_has_partkey).
        let grouped: Vec<Expr> = groups.iter().map(|g| down_to_append(g, input)).collect();
        // (A key's collation: its partition key's declared one, or its column's.)
        let norm = |c: Option<String>| c.filter(|n| n != "default");
        let key_coll = |lv: &crate::sql::plan::PartLevel, ki: usize| {
            norm(lv.parent.partkey.as_ref().and_then(|pk| pk.cols.get(ki)).and_then(|c| {
                c.collation
                    .clone()
                    .or_else(|| c.col.and_then(|i| lv.parent.columns.get(i)).and_then(|col| col.collation.clone()))
            }))
        };
        let full = if is_join(members[0]) {
            // (A partitionwise join's key: a non-nullable side's; grouped under the key's
            // collation, group_by_has_partkey.)
            let origins: Vec<(&ScanPlan, usize, Option<String>)> = grouped
                .iter()
                .filter_map(|g| {
                    let (inner, coll) = match strip(g) {
                        Expr::Collate(x, n, _) => (strip(x), Some(Some(n.clone()))),
                        x => (x, None),
                    };
                    match inner {
                        Expr::Col(k, _) => origin(members[0], *k).filter(|o| !o.2).map(|o| {
                            let own = o.0.table.columns.get(o.1).and_then(|c| c.collation.clone());
                            (o.0, o.1, norm(coll.unwrap_or(own)))
                        }),
                        _ => None,
                    }
                })
                .collect();
            let mut scans = Vec::new();
            scans_in(members[0], &mut scans);
            scans.iter().any(|s| {
                s.part.as_ref().and_then(|l| l.last()).is_some_and(|lv| {
                    !lv.key.is_empty()
                        && lv.key.iter().enumerate().all(|(ki, k)| match strip(k) {
                            Expr::Col(j, _) => origins.iter().any(|(os, oc, gc)| {
                                std::ptr::eq(*os, *s) && oc == j && *gc == key_coll(lv, ki)
                            }),
                            _ => false,
                        })
                })
            })
        } else {
            let m = members[0];
            let scan = member_scan(m)?;
            let level = scan.part.as_ref()?.get(own_levels(&members)?.0[0])?;
            // The key over the leaf's row, as the Append's columns.
            let map: Option<&Vec<Expr>> = match &**m {
                Plan::Project { exprs, input } if matches!(&**input, Plan::Scan(_)) => Some(exprs),
                _ => None,
            };
            !level.key.is_empty()
                && level.key.iter().enumerate().all(|(ki, k)| {
                    let pcoll = key_coll(level, ki);
                    let kcol = match k {
                        Expr::Col(j, _) => scan.table.columns.get(*j).and_then(|c| c.collation.clone()),
                        _ => None,
                    };
                    let k = k
                        .clone()
                        .transform(&mut |x| {
                            Ok(match x {
                                Expr::Col(j, t) => match map {
                                    Some(ex) => match ex.iter().position(|e| matches!(strip(e), Expr::Col(i, _) if *i == j)) {
                                        Some(p) => Expr::Col(p, t),
                                        None => Expr::Col(usize::MAX, t),
                                    },
                                    None => Expr::Col(j, t),
                                },
                                x => x,
                            })
                        })
                        .unwrap();
                    // (group_by_has_partkey: a grouping collation other than the key's is
                    // no match.)
                    grouped.iter().any(|g| {
                        let (inner, gcoll) = match strip(g) {
                            Expr::Collate(x, n, _) => (strip(x), Some(n.clone())),
                            x => (x, kcol.clone()),
                        };
                        inner.same_as(strip(&k)) && norm(gcoll) == pcoll
                    })
                })
        };
        let partial_ok = aggs.iter().all(|a| {
            !a.distinct
                && a.order_by.is_empty()
                && a.direct.is_empty()
                && crate::sql::paths::parallel::agg_partial_ok(a.def.sig.oid)
        });
        if !full && !partial_ok {
            return None;
        }
        // Each partition's grouping (with the HAVING when full).
        let hint = self.group_order.take();
        // (Below a partial one, a sub-partitioned partition's grouping is partial too.)
        let full = full && !self.pw_partial;
        // The partitions of the Append's table: a leaf, or a sub-partitioned one's leaves
        // under an Append of their own.
        // (Per partition: its first leaf when it has several.)
        let mut firsts: Vec<Option<usize>> = Vec::new();
        let parts: Vec<Arc<Plan>> = if is_join(members[0]) {
            members.iter().map(|m| (*m).clone()).collect()
        } else {
            let (_, groups_of) = own_levels(&members)?;
            groups_of
                .iter()
                .map(|g| {
                    firsts.push((g.len() > 1).then(|| addr(members[g[0]])));
                    if g.len() == 1 {
                        members[g[0]].clone()
                    } else {
                        let Plan::Append { inh, .. } = &**app else { unreachable!() };
                        Arc::new(Plan::Append {
                            inputs: g.iter().map(|&i| members[i].clone()).collect(),
                            inh: inh.clone(),
                        })
                    }
                })
                .collect()
        };
        let saved_partial = std::mem::replace(&mut self.pw_partial, !full);
        let mut kids: Vec<ExNode> = Vec::new();
        // (Sub-partitioned partitions whose groupings join this Append: their own Append
        // isn't a plan node, and takes no name.)
        let mut flattened: Vec<usize> = Vec::new();
        let mut par_flattened: Vec<usize> = Vec::new();
        let mut par_leaves = 0usize;
        for (pi, m) in parts.iter().enumerate() {
            self.group_order = hint.clone();
            let child_input = with_member(input, m);
            let agg = Arc::new(Plan::Agg {
                input: child_input,
                groups: groups.to_vec(),
                sets: None,
                aggs: aggs.to_vec(),
            });
            let p = match having.filter(|_| full) {
                Some(h) => Arc::new(Plan::Filter {
                    input: agg,
                    pred: h.clone(),
                }),
                None => agg,
            };
            let saved = std::mem::take(&mut self.pending);
            let b = self.node(&p);
            self.pending = saved;
            // (A partial grouping of a sub-partitioned partition: its partial groupings,
            // not their finalization.)
            let mut k = b.node;
            if !full && k.partial_mode == Some("Finalize") && k.children.len() == 1 {
                k = k.children.pop().unwrap();
                // (Down to its partial groupings: through a sort of them.)
                while k.node_type == "Sort" && k.children.len() == 1 {
                    k = k.children.pop().unwrap();
                }
            }
            if k.node_type == "Append"
                && let Some(Some(f)) = firsts.get(pi)
            {
                flattened.push(*f);
            }
            // (Its partial groupings under a Gather: flattened if they join a Parallel
            // Append of all the partitions'.)
            if matches!(k.node_type, "Gather" | "Gather Merge")
                && let Some(Some(f)) = firsts.get(pi)
            {
                par_flattened.push(*f);
            }
            kids.push(k);
        }
        self.pw_partial = saved_partial;
        let g = crate::sql::paths::Gucs::read(self.ctx);
        let ops = costs::CPU_OPERATOR_COST * groups.len() as f64;
        let trans = costs::CPU_OPERATOR_COST * aggs.len() as f64;
        let finalize = |mut f: ExNode, input: ExNode, hashed: bool| -> ExNode {
            let in_rows = input.rows;
            if hashed {
                f.startup_cost = input.total_cost + in_rows * (trans + ops);
                f.total_cost = f.startup_cost + f.rows * costs::CPU_TUPLE_COST;
            } else {
                f.startup_cost = input.startup_cost;
                f.total_cost = input.total_cost + in_rows * (trans + ops) + f.rows * costs::CPU_TUPLE_COST;
                // (Its groups come out in the keys' order.)
                f.order = keys.to_vec();
            }
            if !aggs.is_empty() {
                let base = f.title.trim_start_matches("Finalize ").to_string();
                f.title = format!("Finalize {base}");
                f.partial_mode = Some("Finalize");
            }
            f.alt = None;
            f.children = vec![input.child("Outer")];
            f
        };
        // (Whether the partitions' groupings stay whole: else their partial groupings are
        // finalized above, and the HAVING with them.)
        let mut pw_full = full;
        let best = if full {
            // An Append of the groupings, or (for an ORDER BY of the group keys) a Merge
            // Append of their sorted forms.
            let plain = append_of(kids.clone());
            let merged = if hint_full {
                kids.iter()
                    .map(sorted_form)
                    .collect::<Option<Vec<_>>>()
                    .map(|ks| merge_append_of(ks, keys.to_vec()))
            } else {
                None
            };
            let mut best = match merged {
                Some(m) if self.ordered_cost(&m, keys, hint_full) < self.ordered_cost(&plain, keys, hint_full) => m,
                _ => plain.clone(),
            };
            // Or the groupings shared out to workers whole: a Gather over a Parallel Append
            // of them (add_paths_to_append_rel's parallel append of non-partial paths).
            let members: Vec<ExNode> = plain
                .children
                .iter()
                .map(|k| {
                    // (Of a grouping gathered from workers, its serial form.)
                    let mut k = match &k.safe_alt {
                        Some(s) => {
                            let mut s = (**s).clone().child("Member");
                            // (Its target list the grouping's.)
                            s.width = k.width;
                            s.cols = k.cols.clone();
                            s.output = k.output.clone();
                            s
                        }
                        None => k.clone(),
                    };
                    k.partial = None;
                    // (A grouping a worker can run whole: over a parallel-safe tree.)
                    k.par_safe = k.subplans.is_empty() && k.children.iter().all(parallel::safe_tree);
                    k
                })
                .collect();
            // (A partial path of the grouping relation: the node above gathers it, or sorts
            // it in the workers first.)
            if let Some(pa) = self.partial_append(&plain, &members)
                && pa.parallel_aware
            {
                best.partial = Some(Box::new(pa));
            }
            // Or the partitions' partial groupings (create_partitionwise_grouping_paths
            // makes them too), finalized over a Parallel Append of them all.
            let (pc, leaves) = self.par_partial_cands(&kids, serial, keys, &finalize);
            let mut best_cost = self.ordered_cost(&best, keys, hint_full);
            if let Some(pa) = &best.partial {
                let g = self.gather_of(pa);
                if parallel::parallel_wins(&g, &best) {
                    best_cost = best_cost.min(self.ordered_cost(&g, keys, hint_full));
                }
            }
            for c in pc {
                let (dc, db) = (parallel::disabled_nodes(&c), parallel::disabled_nodes(&best));
                if dc < db || (dc == db && self.ordered_cost(&c, keys, hint_full) < best_cost) {
                    best_cost = self.ordered_cost(&c, keys, hint_full);
                    best = c;
                    pw_full = false;
                    par_leaves = leaves;
                }
            }
            best
        } else {
            // The partial groupings, then a Finalize grouping of them: hashed over their
            // Append, or sorted over a Merge Append of their sorted forms.
            let partial = |mut k: ExNode| -> ExNode {
                if !aggs.is_empty() && k.node_type == "Aggregate" && k.partial_mode != Some("Partial") {
                    k.title = format!("Partial {}", k.title);
                    k.partial_mode = Some("Partial");
                }
                k.alt = None;
                k
            };
            let mut cands: Vec<ExNode> = Vec::new();
            // (The serial grouping's two forms give the finalize nodes' shapes.)
            let forms: Vec<&ExNode> = std::iter::once(serial).chain(serial.alt.as_deref()).collect();
            for form in forms {
                let hashed = form.strategy == Some("Hashed");
                let mut f = form.clone();
                f.children.clear();
                if hashed {
                    let a = append_of(kids.iter().cloned().map(partial).collect());
                    cands.push(finalize(f, a, true));
                } else {
                    if let Some(sorted) = kids.iter().map(sorted_form).collect::<Option<Vec<_>>>() {
                        let ma = merge_append_of(sorted.into_iter().map(partial).collect(), keys.to_vec());
                        cands.push(finalize(f.clone(), ma, false));
                    }
                    // (Or the partial groups appended, then sorted.)
                    let a = append_of(kids.iter().cloned().map(partial).collect());
                    let mut sn = ExNode::new("Sort");
                    sn.details.push(Detail::List("Sort Key", keys.to_vec()));
                    sn.details.push(Detail::SortInfo);
                    self.sort_costs(&mut sn, &a);
                    sn.disabled = !g.sort;
                    sn.order = keys.to_vec();
                    sn.children.push(a.child("Outer"));
                    cands.push(finalize(f, sn, false));
                }
            }
            // The partitions' partial groupings run by workers (each a Gather over its
            // own): one Parallel Append of them under a Gather (Merge, sorted) instead.
            let (pc, leaves) = self.par_partial_cands(&kids, serial, keys, &finalize);
            par_leaves = leaves;
            cands.extend(pc);
            // (add_path into the grouping relation: a sorted one fuzzily as cheap as a
            // hashed one replaces it. Under an ORDER BY of the group keys, the survivors
            // with the sort they need.)
            let cands = if hint_full { add_path_survivors(cands, keys) } else { cands };
            let mut best: Option<ExNode> = None;
            for c in cands {
                let better = best.as_ref().is_none_or(|b| {
                    let (dc, db) = (parallel::disabled_nodes(&c), parallel::disabled_nodes(b));
                    if dc != db {
                        return dc < db;
                    }
                    if hint_full {
                        self.ordered_cost(&c, keys, hint_full) < self.ordered_cost(b, keys, hint_full)
                    } else {
                        parallel::fuzzy_better(&c, b)
                    }
                });
                if better {
                    best = Some(c);
                }
            }
            let mut b = best?;
            b.disabled = b.strategy == Some("Hashed") && !g.hashagg;
            b
        };
        let (db, ds) = (parallel::disabled_nodes(&best), parallel::disabled_nodes(serial));
        let serial_cost = self
            .ordered_cost(serial, keys, hint_full)
            .min(
                serial
                    .alt
                    .as_deref()
                    .filter(|a| parallel::disabled_nodes(a) <= ds)
                    .map_or(f64::INFINITY, |a| self.ordered_cost(a, keys, hint_full)),
            );
        // (Its partial path gathered, when that wins.)
        let mut best_cost = self.ordered_cost(&best, keys, hint_full);
        if let Some(pa) = &best.partial {
            let g = self.gather_of(pa);
            if parallel::parallel_wins(&g, &best) {
                best_cost = best_cost.min(self.ordered_cost(&g, keys, hint_full));
            }
        }
        if db > ds || (db == ds && best_cost >= serial_cost) {
            return None;
        }
        self.pw_aggs.push(addr(app));
        self.pw_aggs.extend(flattened);
        // (The Parallel Append of all the partitions' partial groupings won.)
        fn par_append_of(n: &ExNode, leaves: usize) -> bool {
            (n.title == "Parallel Append" && n.children.len() == leaves)
                || n.children.iter().any(|c| par_append_of(c, leaves))
        }
        if par_leaves > 0 && par_append_of(&best, par_leaves) {
            self.pw_aggs.extend(par_flattened);
        }
        let mut best = best;
        best.pw_full = pw_full;
        Some(best)
    }
}
