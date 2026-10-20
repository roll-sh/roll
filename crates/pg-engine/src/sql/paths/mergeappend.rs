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
        let want = Want {
            keys: keys
                .iter()
                .zip(dirs)
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
        if leaves(&cat, input, None, key_exprs, needed.to_vec(), &mut appends, &mut found).is_none()
            || appends == 0
            || found.len() < 2
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
        a_total += rows * CPU_TUPLE_COST * 0.5;
        let (s_startup, s_total) = costs::sort_cost(a_total, rows, width, limit_tuples);
        let sorted = Path {
            kind: PathKind::Seq,
            startup: s_startup,
            total: s_total,
            rows,
            keys: Vec::new(),
            disabled: a_disabled + u32::from(!self.g.sort),
        };
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
            for (l, rel) in found.iter().zip(&rels) {
                let Some(o) = self.member_path(rel, &l.keys, &dirs, l.width, startup, limit_tuples) else {
                    return false;
                };
                m_startup += o.startup;
                m_run += o.total - o.startup;
                m_disabled += o.disabled;
            }
            let st = m_startup + cmp * n * logn;
            let merged = Path {
                kind: PathKind::Seq,
                startup: st,
                total: st + m_run + rows * cmp * logn + CPU_TUPLE_COST * 0.5 * rows,
                rows,
                keys: Vec::new(),
                disabled: m_disabled,
            };
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

    /// Each member under `p` in the order of `keys` (over `p`'s row): read in that order,
    /// or sorted.
    fn order_members(&self, p: &mut Plan, keys: &[SortKey], needed: &[bool], startup: bool, limit: Option<f64>) {
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
