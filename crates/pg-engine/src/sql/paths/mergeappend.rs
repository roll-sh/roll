//! generate_orderedappend_paths: an ORDER BY over an append relation (an inheritance tree,
//! a UNION ALL, one inside the other) merges its members each in that order (read from an
//! index in order, or sorted) instead of sorting them all, when that costs less.
//!
//! The plan keeps its Sort over the Append, now over members that each come in the
//! Sort's order; EXPLAIN shows that as a Merge Append (`merge_appended`).

use super::*;

/// A member scan of the append relation: the scan, the sort keys and needed columns over
/// its row, and the width the member's sort sees.
struct Leaf<'p> {
    scan: &'p ScanPlan,
    keys: Vec<Expr>,
    needed: Vec<bool>,
    width: u32,
}

/// `keys` (over `p`'s row) through projections: over `p`'s input row.
fn through(exprs: &[Expr], keys: &[Expr]) -> Vec<Expr> {
    keys.iter()
        .map(|k| {
            k.clone()
                .transform(&mut |e| {
                    Ok(match e {
                        Expr::Col(i, t) => exprs.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                        e => e,
                    })
                })
                .unwrap()
        })
        .collect()
}

/// The append relation's member scans under `p`, or None when a member isn't a plain
/// scan (through projections). `arm`: the member's top plan (under the Append).
fn leaves<'p>(
    cat: &crate::catalog::Catalog,
    p: &'p Plan,
    arm: Option<&'p Plan>,
    keys: Vec<Expr>,
    needed: Vec<bool>,
    appends: &mut usize,
    out: &mut Vec<Leaf<'p>>,
) -> Option<()> {
    match p {
        Plan::Project { input, exprs } => {
            let n = child_needs(p, &needed).pop().unwrap_or_default();
            leaves(cat, input, arm, through(exprs, &keys), n, appends, out)
        }
        Plan::Append { .. } | Plan::SetOp { .. } => {
            let inputs = append_inputs(p)?;
            *appends += 1;
            for i in inputs.into_iter().filter(|i| !matches!(&***i, Plan::Empty { .. })) {
                let n = if crate::sql::binder::plan_width(i) == needed.len() {
                    needed.clone()
                } else {
                    vec![true; crate::sql::binder::plan_width(i)]
                };
                leaves(cat, i, Some(i), keys.clone(), n, appends, out)?;
            }
            Some(())
        }
        // (A UNION ALL's arms were planned as queries of their own: planned again.)
        Plan::Scan(s) if replannable(s) && arm.is_some() => {
            out.push(Leaf {
                scan: s,
                keys,
                needed,
                width: sort_width(cat, arm?),
            });
            Some(())
        }
        _ => None,
    }
}

/// A set operation's ORDER BY (and LIMIT), over arms planned on their own: an ordered
/// UNION ALL may merge them.
pub fn plan_ordered_append(ctx: &ExecCtx, plan: &mut Plan) {
    let planner = Planner::new(ctx);
    let (sort, lim) = match plan {
        Plan::Limit { input, limit, offset, .. } => {
            let lim = Some((limit_est(limit, true), limit_est(offset, false)));
            (take(input), lim)
        }
        other => (other, None),
    };
    if let Plan::Sort { input, .. } = sort {
        let w = crate::sql::binder::plan_width(input);
        planner.merge_append(sort, &vec![true; w], lim);
    }
}

impl Planner<'_, '_> {
    /// A member's path in the order: its cheapest one already in it (by total cost, or
    /// by startup cost for the fast-start Merge Append), else its cheapest one sorted
    /// (generate_orderedappend_paths).
    fn member_path(&self, rel: &Rel, keys: &[Expr], dirs: &[(bool, bool)], width: u32, startup: bool, limit: Option<f64>) -> Option<Path> {
        // (Less the keys a clause of the member equates with a constant: redundant.)
        let want = Want {
            keys: keys
                .iter()
                .zip(dirs)
                .filter(|(e, _)| !super::eq_const(rel, e))
                .map(|(e, (desc, nulls_first))| PathKey {
                    expr: e.clone(),
                    desc: *desc,
                    nulls_first: *nulls_first,
                    cmp: 0,
                })
                .collect(),
            limit: None,
            merge: Vec::new(),
        };
        let list = self.paths(rel, &want);
        let mut best: Option<&Path> = None;
        for p in list.iter().filter(|p| presorted(&want.keys, &p.keys).0) {
            let better = best.is_none_or(|b| {
                if startup && p.disabled == b.disabled && p.startup != b.startup {
                    p.startup < b.startup
                } else {
                    cheaper_total(p, b)
                }
            });
            if better {
                best = Some(p);
            }
        }
        if let Some(b) = best {
            return Some(b.clone());
        }
        let c = cheapest(&list)?;
        let (st, tot) = costs::sort_cost(c.total, c.rows, width, limit);
        Some(Path {
            kind: PathKind::Sort(Box::new(c.clone())),
            startup: st,
            total: tot,
            rows: c.rows,
            keys: want.keys,
            disabled: c.disabled + u32::from(!self.g.sort),
        })
    }

    /// A Sort over an append relation: its members merged in order when cheaper than
    /// sorting them all. Whether it planned them.
    pub(super) fn merge_append(&self, p: &mut Plan, needed: &[bool], lim: Option<(f64, f64)>) -> bool {
        let Plan::Sort { input, keys } = p else {
            return false;
        };
        if keys.is_empty() || keys.iter().any(|k| k.using.is_some()) {
            return false;
        }
        let key_exprs: Vec<Expr> = keys.iter().map(|k| k.expr.clone()).collect();
        let dirs: Vec<(bool, bool)> = keys.iter().map(|k| (k.desc, k.nulls_first)).collect();
        let cat = self.cat.clone();
        let mut appends = 0;
        let mut found = Vec::new();
        if leaves(&cat, input, None, key_exprs.clone(), needed.to_vec(), &mut appends, &mut found).is_none()
            || appends == 0
            || found.is_empty()
        {
            return false;
        }
        let limit_tuples = match lim {
            Some((c, o)) if c > 0.0 && o >= 0.0 => Some(c + o),
            _ => None,
        };
        let rels: Option<Vec<Rel>> = found.iter().map(|l| self.rel(l.scan, &l.needed)).collect();
        let Some(rels) = rels else {
            return false;
        };
        // The members appended (each its cheapest path), then sorted.
        let (mut a_total, mut rows, mut width, mut a_disabled) = (0.0, 0.0, 0u32, 0u32);
        for (l, rel) in found.iter().zip(&rels) {
            let plain = Want {
                keys: Vec::new(),
                limit: None,
                merge: Vec::new(),
            };
            let list = self.paths(rel, &plain);
            let Some(c) = cheapest(&list) else {
                return false;
            };
            a_total += c.total;
            a_disabled += c.disabled;
            rows += c.rows;
            width = width.max(l.width);
        }
        // (A single member is the relation's plan itself: no Append.)
        let append_cpu = if found.len() == 1 { 0.0 } else { CPU_TUPLE_COST * 0.5 };
        a_total += rows * append_cpu;
        let (s_startup, s_total) = costs::sort_cost(a_total, rows, width, limit_tuples);
        let sorted = Path {
            kind: PathKind::Seq,
            startup: s_startup,
            total: s_total,
            rows,
            keys: Vec::new(),
            disabled: a_disabled + u32::from(!self.g.sort),
        };
        let order = partition_order(input, &key_exprs, &dirs).filter(|o| count_members(o) == found.len());
        // Or merged (cost_merge_append): of the members' cheapest paths in the order by
        // total cost, or (when a LIMIT wants a fast start) by startup cost.
        let n = found.len() as f64;
        let cmp = 2.0 * CPU_OPERATOR_COST;
        let logn = n.log2();
        let mut best: Option<(Path, bool)> = None;
        let fraction = |p: &Path| match lim {
            Some((count, offset)) => {
                let f = if count < 0.0 || (count > 0.0 && offset < 0.0) { 0.1 } else { 1.0 };
                let (st, tot) = limit_costs(p, count, offset);
                st + f * (tot - st)
            }
            None => p.total,
        };
        for startup in [false, true] {
            if startup && lim.is_none() {
                continue;
            }
            let (mut m_startup, mut m_run, mut m_disabled) = (0.0, 0.0, 0u32);
            let mut ps = Vec::new();
            for (l, rel) in found.iter().zip(&rels) {
                let Some(o) = self.member_path(rel, &l.keys, &dirs, l.width, startup, limit_tuples) else {
                    return false;
                };
                m_startup += o.startup;
                m_run += o.total - o.startup;
                m_disabled += o.disabled;
                ps.push(o);
            }
            let st = m_startup + cmp * n * logn;
            let mut merged = Path {
                kind: PathKind::Seq,
                startup: st,
                total: st + m_run + rows * cmp * logn + append_cpu * rows,
                rows,
                keys: Vec::new(),
                disabled: m_disabled,
            };
            // (Or partitions read in their order: a plain Append, no merging.)
            if let Some(o) = &order {
                let (st, tot, _, dis) = ordered_cost(o, &ps);
                (merged.startup, merged.total, merged.disabled) = (st, tot, dis);
            }
            let better = best.as_ref().is_none_or(|(b, _)| {
                if merged.disabled != b.disabled {
                    merged.disabled < b.disabled
                } else {
                    fraction(&merged) < fraction(b)
                }
            });
            if better {
                best = Some((merged, startup));
            }
        }
        let Some((merged, startup)) = best else {
            return false;
        };
        let better = if merged.disabled != sorted.disabled {
            merged.disabled < sorted.disabled
        } else {
            fraction(&merged) < fraction(&sorted)
        };
        drop(found);
        if !better {
            return false;
        }
        let Plan::Sort { input, keys } = p else { unreachable!() };
        let sort_keys = keys.clone();
        self.order_members(take(input), &sort_keys, needed, startup, limit_tuples);
        true
    }

    /// generate_orderedappend_paths for an append relation in a join group: its members
    /// (under `p`, `keys` over `p`'s row) merged in that order, each its cheapest path in
    /// it or sorted (cost_merge_append): (startup, total, rows, disabled). None unless
    /// some member reads the order from an index (all_child_pathkeys come from the
    /// members' own ordered paths).
    pub(super) fn merged_cost(
        &self,
        p: &Plan,
        keys: &[SortKey],
        needed: &[bool],
        startup: bool,
    ) -> Option<(f64, f64, f64, u32)> {
        if keys.is_empty() || keys.iter().any(|k| k.using.is_some()) {
            return None;
        }
        let key_exprs: Vec<Expr> = keys.iter().map(|k| k.expr.clone()).collect();
        let dirs: Vec<(bool, bool)> = keys.iter().map(|k| (k.desc, k.nulls_first)).collect();
        let mut appends = 0;
        let mut found = Vec::new();
        leaves(&self.cat, p, None, key_exprs, needed.to_vec(), &mut appends, &mut found)?;
        if appends == 0 || found.len() < 2 {
            return None;
        }
        let n = found.len() as f64;
        let cmp = 2.0 * CPU_OPERATOR_COST;
        let logn = n.log2();
        let (mut m_startup, mut m_run, mut rows, mut disabled) = (0.0, 0.0, 0.0, 0u32);
        let mut indexed = false;
        for l in &found {
            let rel = self.rel(l.scan, &l.needed)?;
            let o = self.member_path(&rel, &l.keys, &dirs, l.width, startup, None)?;
            indexed |= !matches!(o.kind, PathKind::Sort(_));
            m_startup += o.startup;
            m_run += o.total - o.startup;
            disabled += o.disabled;
            rows += o.rows;
        }
        if !indexed {
            return None;
        }
        let st = m_startup + cmp * n * logn;
        let total = st + m_run + rows * cmp * logn + CPU_TUPLE_COST * 0.5 * rows;
        Some((st, total, rows, disabled))
    }

    /// Each member under `p` in the order of `keys` (over `p`'s row): read in that order,
    /// or sorted.
    pub(super) fn order_members(&self, p: &mut Plan, keys: &[SortKey], needed: &[bool], startup: bool, limit: Option<f64>) {
        match p {
            Plan::Project { .. } => {
                let n = child_needs(p, needed).pop().unwrap_or_default();
                let Plan::Project { input, exprs } = p else { unreachable!() };
                let mapped = through(exprs, &keys.iter().map(|k| k.expr.clone()).collect::<Vec<_>>());
                let keys: Vec<SortKey> = keys
                    .iter()
                    .zip(mapped)
                    .map(|(k, expr)| SortKey { expr, ..k.clone() })
                    .collect();
                self.order_members(take(input), &keys, &n, startup, limit);
            }
            Plan::Append { .. } | Plan::SetOp { .. } => {
                let inputs: Vec<&mut Arc<Plan>> = match p {
                    Plan::Append { inputs, .. } => inputs.iter_mut().collect(),
                    Plan::SetOp {
                        kind: crate::sql::plan::SetOpKind::Union,
                        all: true,
                        left,
                        right,
                        ..
                    } => vec![left, right],
                    _ => return,
                };
                let dirs: Vec<(bool, bool)> = keys.iter().map(|k| (k.desc, k.nulls_first)).collect();
                for i in inputs.into_iter().filter(|i| !matches!(&***i, Plan::Empty { .. })) {
                    let w = crate::sql::binder::plan_width(i);
                    let n = if w == needed.len() { needed.to_vec() } else { vec![true; w] };
                    if !member_scan(i) {
                        self.order_members(take(i), keys, &n, startup, limit);
                        continue;
                    }
                    let width = sort_width(&self.cat, i);
                    let exprs: Vec<Expr> = keys.iter().map(|k| k.expr.clone()).collect();
                    let Some((s, skeys, sneeded)) = member_scan_mut(take(i), exprs, n) else {
                        continue;
                    };
                    let Some(rel) = self.rel(s, &sneeded) else {
                        continue;
                    };
                    let Some(path) = self.member_path(&rel, &skeys, &dirs, width, startup, limit) else {
                        continue;
                    };
                    match &path.kind {
                        PathKind::Sort(inner) => {
                            apply(&rel, s, inner);
                            *i = Arc::new(Plan::Sort {
                                input: i.clone(),
                                keys: keys.to_vec(),
                            });
                        }
                        _ => {
                            apply(&rel, s, &path);
                            if let Access::Index { ordered, .. } = &mut s.access {
                                *ordered = true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// The scan of a member (through projections), with `keys` and `needed` (over the
/// member's row) over the scan's row.
fn member_scan_mut(p: &mut Plan, keys: Vec<Expr>, needed: Vec<bool>) -> Option<(&mut ScanPlan, Vec<Expr>, Vec<bool>)> {
    match p {
        Plan::Project { .. } => {
            let n = child_needs(p, &needed).pop().unwrap_or_default();
            let Plan::Project { input, exprs } = p else { unreachable!() };
            let keys = through(exprs, &keys);
            member_scan_mut(take(input), keys, n)
        }
        Plan::Scan(s) => Some((s, keys, needed)),
        _ => None,
    }
}

/// An append relation's members: an Append's, a UNION ALL's.
fn append_inputs(p: &Plan) -> Option<Vec<&Arc<Plan>>> {
    match p {
        Plan::Append { inputs, .. } => Some(inputs.iter().collect()),
        Plan::SetOp {
            kind: crate::sql::plan::SetOpKind::Union,
            all: true,
            left,
            right,
            ..
        } => Some(vec![left, right]),
        _ => None,
    }
}

/// A scan the planner may choose the path of (again).
fn replannable(s: &ScanPlan) -> bool {
    s.sample.is_none() && s.current_of.is_none() && s.table.partkey.is_none()
}

/// A member that is a scan (through projections).
fn member_scan(p: &Plan) -> bool {
    match p {
        Plan::Project { input, .. } => member_scan(input),
        Plan::Scan(_) => true,
        _ => false,
    }
}

/// A member of an Append over partitions in the partition order (`partition_order`): a
/// member (its position among the Append's live inputs), a sub-partitioned table's own
/// Append in its partition order, or a Merge Append of a sub-partitioned table's members.
#[derive(Debug)]
pub enum PartOrder {
    Leaf(usize),
    Append(Vec<PartOrder>),
    Merge(Vec<usize>),
}

/// The members of an inheritance Append under `p` (through projections), each its scan
/// and `keys` (over `p`'s row) over the scan's row.
fn part_members(p: &Plan, keys: Vec<Expr>) -> Option<Vec<(&ScanPlan, Vec<Expr>)>> {
    fn member(p: &Plan, keys: Vec<Expr>) -> Option<(&ScanPlan, Vec<Expr>)> {
        match p {
            Plan::Sort { input, .. } => member(input, keys),
            Plan::Project { input, exprs } => member(input, through(exprs, &keys)),
            Plan::Scan(s) => Some((s, keys)),
            _ => None,
        }
    }
    match p {
        Plan::Project { input, exprs } => part_members(input, through(exprs, &keys)),
        Plan::Append { inputs, inh: Some(_) } => inputs
            .iter()
            .filter(|i| !matches!(&***i, Plan::Empty { .. }))
            .map(|i| member(i, keys.clone()))
            .collect(),
        _ => None,
    }
}

/// generate_orderedappend_paths's match_partition_order: when an append relation of
/// partitions comes in the order of `keys` (over `p`'s row, in directions `dirs`) by
/// reading its partitions one after the other in their bounds' order (or backwards),
/// the members in that order: a plain Append, no Merge Append.
pub fn partition_order(p: &Plan, keys: &[Expr], dirs: &[(bool, bool)]) -> Option<Vec<PartOrder>> {
    let members = part_members(p, keys.to_vec())?;
    if members.len() < 2 {
        return None;
    }
    let root = members[0].0.part.as_ref()?.last()?.parent.oid;
    if members
        .iter()
        .any(|(s, _)| s.part.as_ref().and_then(|l| l.last()).is_none_or(|l| l.parent.oid != root))
    {
        return None;
    }
    let idx: Vec<usize> = (0..members.len()).collect();
    level_order(&members, root, &idx, dirs)
}

/// `partition_order` for the members `idx` under partitioned table `parent`.
fn level_order(members: &[(&ScanPlan, Vec<Expr>)], parent: u32, idx: &[usize], dirs: &[(bool, bool)]) -> Option<Vec<PartOrder>> {
    use crate::catalog::PartStrategy;
    use crate::sql::plan::PartLevel;
    // The members by the partition of `parent` they are in.
    let mut groups: Vec<(u32, &PartLevel, Vec<usize>)> = Vec::new();
    for &i in idx {
        let levels = members[i].0.part.as_ref()?;
        let li = levels.iter().position(|l| l.parent.oid == parent)?;
        let child = if li == 0 { members[i].0.table.oid } else { levels[li - 1].parent.oid };
        match groups.last_mut() {
            Some((c, _, v)) if *c == child => v.push(i),
            _ => groups.push((child, &levels[li], vec![i])),
        }
    }
    let level = groups.first()?.1;
    // partitions_are_ordered over the live partitions.
    match level.strategy {
        PartStrategy::Range => {
            if groups.iter().any(|g| g.1.bound.is_default()) {
                return None;
            }
        }
        PartStrategy::List => {
            let inter = interleaved(level);
            if groups.iter().any(|g| inter.contains(&g.1.bound)) {
                return None;
            }
        }
        _ => return None,
    }
    // build_partition_pathkeys: the key's columns the query orders by, in order, past
    // those a filter equates with constants (redundant).
    let (scan, qk) = &members[groups[0].2[0]];
    let mut parts = Vec::new();
    if let Some(f) = &scan.filter {
        conjuncts(f.clone(), &mut parts);
    }
    let strip = |e: &Expr| match e {
        Expr::Relabel(x, _) => (**x).clone(),
        e => e.clone(),
    };
    let fixed = |e: &Expr| {
        parts.iter().any(|q| match q {
            Expr::Call(d, args, _) if d.name == "=" && args.len() == 2 && !args.iter().any(|a| matches!(a, Expr::Collate(..))) => {
                (pseudo_constant(&args[1]) && strip(&args[0]).same_as(&strip(e)))
                    || (pseudo_constant(&args[0]) && strip(&args[1]).same_as(&strip(e)))
            }
            _ => false,
        })
    };
    // (partkey_is_bool_constant_for_query: a boolean key a filter fixes.)
    let bool_fixed = |e: &Expr| {
        e.ty() == DataType::Bool
            && parts.iter().any(|q| match q {
                Expr::Not(x) => x.same_as(e),
                q => q.same_as(e),
            })
    };
    let q: Vec<usize> = (0..qk.len()).filter(|&j| !fixed(&qk[j])).collect();
    let (mut pk, mut partial) = (Vec::new(), false);
    for k in &level.key {
        if fixed(k) {
            continue;
        }
        if let Some(j) = q.iter().copied().find(|&j| strip(&qk[j]).same_as(&strip(k))) {
            if !pk.contains(&j) {
                pk.push(j);
            }
            continue;
        }
        if bool_fixed(k) {
            continue;
        }
        partial = true;
        break;
    }
    let contained = |a: &[usize], b: &[usize], dir: (bool, bool)| {
        a.len() <= b.len() && a.iter().zip(b).all(|(x, y)| x == y && dirs.get(*x) == Some(&dir))
    };
    let matches = |dir| contained(&q, &pk, dir) || (!partial && contained(&pk, &q, dir));
    let backward = if matches((false, false)) {
        false
    } else if matches((true, true)) {
        true
    } else {
        return None;
    };
    let mut out: Vec<PartOrder> = groups
        .into_iter()
        .map(|(child, _, v)| {
            if v.len() == 1 {
                PartOrder::Leaf(v[0])
            } else {
                match level_order(members, child, &v, dirs) {
                    Some(sub) => PartOrder::Append(sub),
                    None => PartOrder::Merge(v),
                }
            }
        })
        .collect();
    if backward {
        out.reverse();
    }
    Some(out)
}

/// A list-partitioned level's partitions that may hold values between other partitions'
/// (create_list_bounds' interleaved_parts): out of order, holding NULL besides values, or
/// the default one.
fn interleaved(level: &crate::sql::plan::PartLevel) -> Vec<crate::catalog::PartBound> {
    use crate::catalog::PartBound;
    use crate::catalog::partition::{bound_order, key_cmp, under};
    let _g = under(level.colls.clone());
    let mut bounds: Vec<&PartBound> = level.siblings.iter().chain([&level.bound]).collect();
    bounds.sort_by(|a, b| bound_order(a, b));
    let mut out: Vec<usize> = Vec::new();
    if bounds.len() < 2 {
        return Vec::new();
    }
    let mut vals: Vec<(&Value, usize)> = Vec::new();
    let mut null_index = None;
    for (i, b) in bounds.iter().enumerate() {
        if let PartBound::List(v) = b {
            for d in v {
                if d.0.is_null() {
                    null_index = Some(i);
                } else {
                    vals.push((&d.0, i));
                }
            }
        }
    }
    let default = bounds.iter().position(|b| b.is_default());
    if vals.len() + usize::from(null_index.is_some()) + usize::from(default.is_some()) != bounds.len() {
        vals.sort_by(|a, b| key_cmp(0, a.0, b.0));
        let mut last = None;
        for &(_, i) in &vals {
            if last.is_some_and(|l| i < l) || null_index == Some(i) {
                out.push(i);
            }
            last = Some(i);
        }
    }
    out.extend(default);
    out.into_iter().map(|i| bounds[i].clone()).collect()
}

/// An ordered Append's (`partition_order`) startup and total costs, rows and disabled
/// nodes, over its members' paths `ps` (cost_append with pathkeys).
fn ordered_cost(o: &[PartOrder], ps: &[Path]) -> (f64, f64, f64, u32) {
    let (mut st, mut tot, mut rows, mut dis) = (0.0, 0.0, 0.0, 0u32);
    for (n, m) in o.iter().enumerate() {
        let (s, t, r, d) = match m {
            PartOrder::Leaf(i) => (ps[*i].startup, ps[*i].total, ps[*i].rows, ps[*i].disabled),
            PartOrder::Append(sub) => ordered_cost(sub, ps),
            PartOrder::Merge(v) => {
                let k = v.len() as f64;
                let cmp = 2.0 * CPU_OPERATOR_COST;
                let logn = k.log2();
                let r: f64 = v.iter().map(|i| ps[*i].rows).sum();
                let s = v.iter().map(|i| ps[*i].startup).sum::<f64>() + cmp * k * logn;
                let run: f64 = v.iter().map(|i| ps[*i].total - ps[*i].startup).sum();
                let d = v.iter().map(|i| ps[*i].disabled).sum();
                (s, s + run + r * cmp * logn + CPU_TUPLE_COST * 0.5 * r, r, d)
            }
        };
        if n == 0 {
            st = s;
        }
        tot += t;
        rows += r;
        dis += d;
    }
    (st, tot + CPU_TUPLE_COST * 0.5 * rows, rows, dis)
}

/// The members an ordered Append (`partition_order`) reads.
pub fn count_members(o: &[PartOrder]) -> usize {
    o.iter()
        .map(|m| match m {
            PartOrder::Leaf(_) => 1,
            PartOrder::Append(sub) => count_members(sub),
            PartOrder::Merge(v) => v.len(),
        })
        .sum()
}
