//! pull_up_simple_subquery's substitution of a FROM subquery's computed outputs
//! (prepjointree.c's pullup_replace_vars_callback, Postgres 18): a subquery over one table
//! is the table itself in the join tree, its select list's expressions evaluated where the
//! query reads them (above the joins), not in the table's scan. Only an output an outer
//! join could null and that wouldn't go NULL with the table's row (a PlaceHolderVar) stays
//! computed in the scan: one without the table's columns, or not strict, or a LATERAL
//! reference to a relation the outer join doesn't null alike.

use super::*;

/// A join tree's leaf scans with their computed outputs taken out: the tree as it becomes,
/// and per column of the tree's row as it was, the expression over the new row it is.
pub(super) struct Lifted {
    pub plan: Plan,
    pub map: Vec<Expr>,
    /// Empty relations (FROM-less subqueries, one-row VALUES) went from the join tree.
    pub results: bool,
}

fn and_all(mut items: Vec<Expr>) -> Option<Expr> {
    match items.len() {
        0 => None,
        1 => items.pop(),
        _ => Some(Expr::And(items)),
    }
}

fn nonstrict(e: &Expr) -> bool {
    crate::sql::binder::optimize::contain_nonstrict(e)
}

/// An expression that may move up the plan: no sub-select, aggregate, set-returning or
/// volatile call, and no reference to a query further out than the join's LATERAL values.
fn movable(e: &Expr) -> bool {
    let mut ok = !e.is_volatile();
    e.walk(&mut |x| match x {
        Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Grouping(_) | Expr::Srf(..) => {
            ok = false
        }
        Expr::Outer(d, ..) if *d != 1 => ok = false,
        Expr::Param(..) | Expr::Default(_) | Expr::CaseTest(_) => ok = false,
        _ => {}
    });
    ok
}

/// The LATERAL values (`Outer(1, i)`) an expression reads.
fn outer_refs(e: &Expr) -> Vec<usize> {
    let mut out = Vec::new();
    e.walk(&mut |x| {
        if let Expr::Outer(1, i, _) = x {
            out.push(*i);
        }
    });
    out
}

/// `e` (over the old row) over the new row; a column that stays a column keeps its type.
pub(super) fn subst(e: &Expr, map: &[Expr]) -> Expr {
    e.clone()
        .transform(&mut |x| {
            Ok(match x {
                Expr::Col(i, t) => match map.get(i) {
                    Some(Expr::Col(j, _)) => Expr::Col(*j, t),
                    Some(m) => m.clone(),
                    None => Expr::Col(i, t),
                },
                x => x,
            })
        })
        .unwrap()
}

fn subst_outer(e: Expr, params: &[Expr]) -> Expr {
    e.transform(&mut |x| {
        Ok(match x {
            Expr::Outer(1, i, t) => params.get(i).cloned().unwrap_or(Expr::Outer(1, i, t)),
            x => x,
        })
    })
    .unwrap()
}

/// Whether the plan reads any value of an enclosing level (a LATERAL join's parameters
/// included).
fn reads_outer(p: &Plan) -> bool {
    let mut exprs = Vec::new();
    crate::sql::explain::node_exprs(p, &mut exprs);
    if let Plan::Scan(sp) = p {
        exprs.extend(sp.written.iter());
        exprs.extend(sp.phv_quals.iter());
        exprs.extend(sp.current_of.iter());
    }
    if exprs.iter().any(|e| e.has_outer_refs()) {
        return true;
    }
    crate::sql::explain::plan_inputs(p).into_iter().any(|c| reads_outer(c))
}

/// A FROM subquery over one table scan: its select list and the scan.
fn subquery_leaf(p: &Plan) -> Option<(&ScanPlan, &Vec<Expr>)> {
    let Plan::Project { input, exprs } = p else {
        return None;
    };
    let Plan::Scan(sp) = &**input else {
        return None;
    };
    // (The subquery planned its scan for itself: the group plans it again.)
    if virtual_proj(p).is_some()
        || sp.sample.is_some()
        || sp.current_of.is_some()
        || sp.table.partkey.is_some()
        || sp.mark.is_some()
        || sp.limit.is_some()
        || sp.filter.as_ref().is_some_and(|f| !movable_filter(f))
    {
        return None;
    }
    Some((sp, exprs))
}

/// A scan filter that doesn't stop the scan from being a plain relation of the join tree.
fn movable_filter(f: &Expr) -> bool {
    let mut ok = true;
    f.walk(&mut |x| {
        if let Expr::Outer(d, ..) = x
            && *d != 1
        {
            ok = false
        }
    });
    ok
}

struct Cx {
    /// Outer joins seen so far (each a bit).
    next: u32,
    lifted: bool,
    /// Something lifted can't be placed: nothing is lifted.
    bad: bool,
    /// Relations (FROM items) seen so far (each a bit).
    nrel: u32,
    /// An empty relation was pulled up or removed.
    results: bool,
}

/// `nulled`: the outer joins above `p` (in the group) that null its rows. `lateral`: the
/// LATERAL values of the join `p` is the right side of (over that join's left row as it
/// becomes), with the outer joins nulling each one's relation; None when `p` isn't such a
/// side. Returns the new plan, the map, and per new column the outer joins nulling it and
/// the relation it comes from (a bit).
fn lift(
    p: &Plan,
    nulled: u32,
    lateral: Option<(&[Expr], &[(u32, u64)])>,
    cx: &mut Cx,
) -> (Plan, Vec<Expr>, Vec<(u32, u64)>) {
    let rel = 1u64 << cx.nrel.min(63);
    let identity = |p: &Plan, cx: &mut Cx| {
        cx.nrel += 1;
        let types = super::super::analyzejoins::col_types(p);
        let w = types.len();
        (
            p.clone(),
            types.into_iter().enumerate().map(|(i, t)| Expr::Col(i, t)).collect::<Vec<_>>(),
            vec![(nulled, rel); w],
        )
    };
    match p {
        Plan::Join {
            kind,
            left,
            right,
            cond,
            hash,
            params,
            nleft,
            nright,
            meta,
        } if matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full)
            && meta.from_call.is_none()
            && cx.next < 30 =>
        {
            let bit = 1u32 << cx.next;
            cx.next += 1;
            // (As written: Postgres pulls subqueries up before reducing outer joins.)
            let wk = meta.written_kind.unwrap_or(*kind);
            let lnull = nulled | if matches!(wk, JoinKind::Right | JoinKind::Full) { bit } else { 0 };
            let rnull = nulled | if matches!(wk, JoinKind::Left | JoinKind::Full) { bit } else { 0 };
            let (l2, ml, nl) = lift(left, lnull, None, cx);
            let nleft2 = crate::sql::binder::plan_width(&l2);
            let params2: Vec<Expr> = params.iter().map(|e| subst(e, &ml)).collect();
            let pnull: Vec<(u32, u64)> = params2
                .iter()
                .map(|e| {
                    e.columns().iter().fold((0u32, 0u64), |m, &c| {
                        let (n, r) = nl.get(c).copied().unwrap_or((0, 0));
                        (m.0 | n, m.1 | r)
                    })
                })
                .collect();
            // (A side the hash keys read: its lifted values must stay over that side.)
            let lat = (!params.is_empty()).then_some((&params2[..], &pnull[..]));
            let (r2, mr, nr) = lift(right, rnull, lat, cx);
            let nright2 = crate::sql::binder::plan_width(&r2);
            // The right side's columns over the combined row (its LATERAL values put in).
            let mr: Vec<Expr> = mr
                .into_iter()
                .map(|e| subst_outer(e.shift_cols(nleft2 as isize), &params2))
                .collect();
            let mut map = ml.clone();
            map.extend(mr.iter().cloned());
            let cond2 = cond.as_ref().map(|c| subst(c, &map));
            let hash2 = hash.as_ref().map(|(lk, rk)| {
                let rk: Vec<Expr> = rk.iter().map(|e| subst(e, &mr)).collect();
                // (A right key now reading the left side: not a hash key any more.)
                if rk.iter().any(|e| e.columns().iter().any(|&c| c < nleft2)) {
                    cx.bad = true;
                }
                (
                    lk.iter().map(|e| subst(e, &ml)).collect::<Vec<_>>(),
                    rk.into_iter()
                        .map(|e| if cx.bad { e } else { e.shift_cols(-(nleft2 as isize)) })
                        .collect::<Vec<_>>(),
                )
            });
            // A LATERAL subquery's WHERE clauses reading the left side are the join's
            // (deconstruct_jointree: they come before its ON clause).
            let (r2, moved) = if !params.is_empty() && matches!(kind, JoinKind::Inner | JoinKind::Left) {
                lateral_quals(r2, &params2, &pnull, nleft2)
            } else {
                (r2, Vec::new())
            };
            if !moved.is_empty() {
                cx.lifted = true;
            }
            let cond2 = match (moved.is_empty(), cond2) {
                (true, c) => c,
                (false, c) => {
                    let mut all = moved;
                    if let Some(c) = c {
                        conjuncts(c, &mut all);
                    }
                    and_all(all)
                }
            };
            // (A PlaceHolderVar of LATERAL values only the scan's clauses read: a
            // parameter of its own, the value the outer side computes.)
            let (r2, params2) = if !params.is_empty() && matches!(kind, JoinKind::Inner | JoinKind::Left) {
                hoist_phv_params(r2, params2)
            } else {
                (r2, params2)
            };
            let (r2, params2) = if !params.is_empty() && !reads_outer(&r2) {
                (r2, Vec::new())
            } else if !params.is_empty() {
                prune_params(r2, params2)
            } else {
                (r2, params2)
            };
            let remap_cols = |cols: &[usize]| {
                let mut out = Vec::new();
                for &c in cols {
                    match map.get(c) {
                        Some(e) => {
                            for k in e.columns() {
                                if !out.contains(&k) {
                                    out.push(k);
                                }
                            }
                        }
                        None => out.push(c),
                    }
                }
                out
            };
            let mut meta2 = (**meta).clone();
            meta2.kept = remap_cols(&meta.kept);
            meta2.pushed = remap_cols(&meta.pushed);
            let mut nul = nl;
            nul.extend(nr);
            let _ = (nleft, nright);
            // remove_useless_result_rtes: a left join of an empty relation (a pulled up
            // FROM-less subquery or one-row VALUES) on true is its left side, the right
            // side's PlaceHolderVars evaluated there; one that computes nothing, whatever
            // its clause.
            let fromless = |q: &Plan| match q {
                Plan::One => true,
                Plan::Project { input, .. } => matches!(&**input, Plan::One),
                _ => false,
            };
            if *kind == JoinKind::Left
                && wk == JoinKind::Left
                && fromless(&r2)
                && hash2.is_none()
                && params2.is_empty()
                && (cond2.is_none() || matches!(r2, Plan::One))
            {
                let plan = match (l2, r2) {
                    (l, Plan::One) => l,
                    (Plan::One, r) => r,
                    (l, Plan::Project { exprs, .. }) => {
                        let nl = crate::sql::binder::plan_width(&l);
                        let types = super::super::analyzejoins::col_types(&l);
                        let mut all: Vec<Expr> = (0..nl).map(|i| Expr::Col(i, types.get(i).cloned().unwrap_or(DataType::Unknown))).collect();
                        all.extend(exprs);
                        Plan::Project {
                            input: Arc::new(l),
                            exprs: all,
                        }
                    }
                    _ => unreachable!(),
                };
                return (plan, map, nul);
            }
            // remove_useless_result_rtes: an inner join with an empty relation (a pulled
            // up one-row VALUES) is its other side, the join's clauses checked there.
            if matches!(l2, Plan::One) && *kind == JoinKind::Inner && wk == JoinKind::Inner && hash2.is_none() && params2.is_empty() {
                let plan = match cond2 {
                    Some(c) => Plan::Filter { input: Arc::new(r2), pred: c },
                    None => r2,
                };
                return (plan, map, nul);
            }
            if matches!(r2, Plan::One) && *kind == JoinKind::Inner && wk == JoinKind::Inner && hash2.is_none() && params2.is_empty() {
                let plan = match (l2, cond2) {
                    (Plan::Join { kind: JoinKind::Inner, left, right, cond, hash, params, nleft, nright, meta }, Some(c)) => {
                        let mut all = Vec::new();
                        if let Some(c0) = cond {
                            conjuncts(c0, &mut all);
                        }
                        conjuncts(c, &mut all);
                        Plan::Join { kind: JoinKind::Inner, left, right, cond: and_all(all), hash, params, nleft, nright, meta }
                    }
                    (l, Some(c)) => Plan::Filter { input: Arc::new(l), pred: c },
                    (l, None) => l,
                };
                return (plan, map, nul);
            }
            (
                Plan::Join {
                    kind: *kind,
                    left: Arc::new(l2),
                    right: Arc::new(r2),
                    cond: cond2,
                    hash: hash2,
                    params: params2,
                    nleft: nleft2,
                    nright: nright2,
                    meta: Box::new(meta2),
                },
                map,
                nul,
            )
        }
        // A one-row VALUES list (a FROM-less subquery's): pull_up_simple_values, an
        // empty relation (RTE_RESULT) whose values are computed where they're read,
        // when no outer join of the group nulls them.
        Plan::Values { rows, .. } if rows.len() == 1 && nulled == 0 && rows[0].iter().all(movable) => {
            if rows[0].iter().any(|e| !outer_refs(e).is_empty()) && lateral.is_none() {
                return identity(p, cx);
            }
            cx.lifted = true;
            cx.results = true;
            cx.nrel += 1;
            (Plan::One, rows[0].clone(), Vec::new())
        }
        // A FROM-less subquery (an RTE_RESULT pulled up): its outputs computed where
        // they're read, those an outer join of the group nulls staying computed in it
        // (PlaceHolderVars evaluated at the empty relation).
        Plan::Project { input, exprs }
            if matches!(&**input, Plan::One) && !exprs.is_empty() && exprs.iter().all(movable) =>
        {
            if exprs.iter().any(|e| !outer_refs(e).is_empty()) && lateral.is_none() {
                return identity(p, cx);
            }
            let same_level = |i: &usize| {
                lateral.is_some_and(|(_, pn)| pn.get(*i).is_some_and(|n| nulled & !n.0 == 0))
            };
            let up: Vec<bool> = exprs
                .iter()
                .map(|e| {
                    if nulled == 0 {
                        return true;
                    }
                    if let Expr::Outer(1, i, _) = e {
                        return same_level(i);
                    }
                    outer_refs(e).iter().any(same_level) && !nonstrict(e)
                })
                .collect();
            cx.lifted = true;
            cx.results = true;
            cx.nrel += 1;
            let kept: Vec<Expr> = exprs.iter().zip(&up).filter(|(_, u)| !**u).map(|(e, _)| e.clone()).collect();
            let mut k = 0;
            let map: Vec<Expr> = exprs
                .iter()
                .zip(&up)
                .map(|(e, u)| {
                    if *u {
                        e.clone()
                    } else {
                        k += 1;
                        Expr::Col(k - 1, e.ty())
                    }
                })
                .collect();
            if kept.is_empty() {
                (Plan::One, map, Vec::new())
            } else {
                let w = kept.len();
                (
                    Plan::Project {
                        input: Arc::new(Plan::One),
                        exprs: kept,
                    },
                    map,
                    vec![(nulled, rel); w],
                )
            }
        }
        // (Its filter, the join clauses a LATERAL side took in, over the values: the
        // join's again, `lateral_quals`.)
        Plan::Filter { input, pred }
            if matches!(&**input, Plan::Values { rows, .. } if rows.len() == 1 && rows[0].iter().all(movable))
                && nulled == 0
                && lateral.is_some()
                && movable(pred) =>
        {
            let Plan::Values { rows, .. } = &**input else { unreachable!() };
            let row = rows[0].clone();
            let pred = subst(pred, &row);
            cx.lifted = true;
            cx.results = true;
            cx.nrel += 1;
            (
                Plan::Filter {
                    input: Arc::new(Plan::One),
                    pred,
                },
                row,
                Vec::new(),
            )
        }
        _ => {
            let Some((sp, exprs)) = subquery_leaf(p) else {
                return identity(p, cx);
            };
            let mut sc = sp.clone();
            sc.access = Access::Unplanned;
            sc.index = None;
            sc.reverse = false;
            let scan = Plan::Scan(Box::new(sc));
            let sw = crate::sql::binder::plan_width(&scan);
            // Which outputs move up: those that need no PlaceHolderVar.
            let up: Vec<bool> = exprs
                .iter()
                .map(|e| {
                    if matches!(e, Expr::Col(..)) || !movable(e) {
                        return false;
                    }
                    let refs = outer_refs(e);
                    if !refs.is_empty() && lateral.is_none() {
                        return false;
                    }
                    // Not nulled by an outer join of the group: computed where it's read.
                    if nulled == 0 {
                        return true;
                    }
                    // The relations a LATERAL reference reads are under the same lowest
                    // nulling outer join as this one.
                    let same_level = |i: &usize| {
                        lateral.is_some_and(|(_, pn)| pn.get(*i).is_some_and(|n| nulled & !n.0 == 0))
                    };
                    if let Expr::Outer(1, i, _) = e {
                        return same_level(i);
                    }
                    let own = !e.columns().is_empty();
                    (own || refs.iter().any(same_level)) && !nonstrict(e)
                })
                .collect();
            // (A subquery whose outputs are all columns: as it is, unless its WHERE
            // reads the LATERAL values, which become the join's. Nor one lifted already:
            // the relation's columns, then what stays computed.)
            let lateral_where = lateral.is_some() && sp.filter.as_ref().is_some_and(|f| !outer_refs(f).is_empty());
            let ncol = exprs.iter().take_while(|e| matches!(e, Expr::Col(..))).count();
            let canonical = ncol < exprs.len()
                && exprs[..ncol].windows(2).all(|w| matches!((&w[0], &w[1]), (Expr::Col(a, _), Expr::Col(b, _)) if a < b))
                && exprs[ncol..].iter().all(|e| !matches!(e, Expr::Col(..)))
                && !up.iter().any(|u| *u);
            if (exprs.iter().all(|e| matches!(e, Expr::Col(..))) || canonical) && !lateral_where {
                return identity(p, cx);
            }
            cx.lifted = true;
            cx.nrel += 1;
            let kept: Vec<Expr> = exprs
                .iter()
                .zip(&up)
                .filter(|(e, u)| !**u && !matches!(e, Expr::Col(..)))
                .map(|(e, _)| e.clone())
                .collect();
            // The scan's columns the projection returns (when it stays): those the query
            // reads (the outputs that are columns, the lifted expressions' columns) and
            // those of its clauses over the LATERAL values (join clauses), in order.
            let mut base: Vec<usize> = Vec::new();
            for (e, u) in exprs.iter().zip(&up) {
                if matches!(e, Expr::Col(..)) || *u {
                    base.extend(e.columns());
                }
            }
            if let Some(f) = &sp.filter {
                let mut parts = Vec::new();
                conjuncts(f.clone(), &mut parts);
                for q in parts.iter().filter(|q| !outer_refs(q).is_empty()) {
                    base.extend(q.columns());
                }
            }
            base.sort_unstable();
            base.dedup();
            let at = |c: usize| if kept.is_empty() { c } else { base.iter().position(|&b| b == c).unwrap_or(c) };
            let to_out = |e: &Expr| -> Expr {
                e.clone()
                    .transform(&mut |x| {
                        Ok(match x {
                            Expr::Col(c, t) => Expr::Col(at(c), t),
                            x => x,
                        })
                    })
                    .unwrap()
            };
            let nbase = if kept.is_empty() { sw } else { base.len() };
            let mut map = Vec::with_capacity(exprs.len());
            let mut k = 0;
            for (e, u) in exprs.iter().zip(&up) {
                map.push(match e {
                    Expr::Col(..) => to_out(e),
                    _ if *u => to_out(e),
                    _ => {
                        k += 1;
                        Expr::Col(nbase + k - 1, e.ty())
                    }
                });
            }
            let plan = if kept.is_empty() {
                scan
            } else {
                let mut out: Vec<Expr> = match &scan {
                    Plan::Scan(s) => base
                        .iter()
                        .map(|&i| Expr::Col(i, s.table.columns.get(i).map_or(DataType::Unknown, |c| c.ty.clone())))
                        .collect(),
                    _ => unreachable!(),
                };
                out.extend(kept);
                Plan::Project {
                    input: Arc::new(scan),
                    exprs: out,
                }
            };
            let w = crate::sql::binder::plan_width(&plan);
            (plan, map, vec![(nulled, rel); w])
        }
    }
}

/// The scan's restriction clauses that read the LATERAL values (`params`, over the left
/// row; `pinfo`: the relations each reads) taken out as clauses over the join's row: join
/// clauses, unless the relations they read are among those the scan's own LATERAL
/// references need (its parameterized path then checks them).
fn lateral_quals(r: Plan, params: &[Expr], pinfo: &[(u32, u64)], nleft: usize) -> (Plan, Vec<Expr>) {
    // A filter over another plan (a VALUES list): its clauses reading the LATERAL values
    // that read more than the plan below does.
    if let Plan::Filter { input, pred } = &r
        && !matches!(&**input, Plan::Scan(_) | Plan::Project { .. })
    {
        return filter_lateral_quals(input, pred, params, pinfo, nleft).unwrap_or((r, Vec::new()));
    }
    let (sp, kept) = match &r {
        Plan::Scan(sp) => (sp, &[][..]),
        Plan::Project { input, exprs } => match &**input {
            Plan::Scan(sp) => {
                let ncol = exprs.iter().take_while(|e| matches!(e, Expr::Col(..))).count();
                (sp, &exprs[ncol..])
            }
            _ => return (r, Vec::new()),
        },
        _ => return (r, Vec::new()),
    };
    // A scan column's place in the side's row.
    let pos = |c: usize| -> Option<usize> {
        match &r {
            Plan::Project { exprs, .. } => exprs.iter().position(|e| matches!(e, Expr::Col(j, _) if *j == c)),
            _ => Some(c),
        }
    };
    let Some(f) = &sp.filter else {
        return (r, Vec::new());
    };
    if sp.sample.is_some()
        || sp.current_of.is_some()
        || sp.table.partkey.is_some()
        || sp.limit.is_some()
        || sp.mark.is_some()
    {
        return (r, Vec::new());
    }
    let rels_of = |e: &Expr| outer_refs(e).iter().fold(0u64, |m, &i| m | pinfo.get(i).map_or(u64::MAX, |x| x.1));
    let fine = |e: &Expr| {
        let mut fine = !e.is_volatile();
        e.walk(&mut |x| match x {
            Expr::Subquery(_) | Expr::Param(..) => fine = false,
            Expr::Outer(d, ..) if *d != 1 => fine = false,
            _ => {}
        });
        fine
    };
    let mut parts = Vec::new();
    conjuncts(f.clone(), &mut parts);
    // The relations the scan must have from the left side anyway.
    let mut need = kept.iter().fold(0u64, |m, e| m | rels_of(e));
    for q in &parts {
        if !outer_refs(q).is_empty() && !fine(q) {
            need |= rels_of(q);
        }
    }
    let (out, keep): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|e| {
        !outer_refs(e).is_empty()
            && fine(e)
            && (need == 0 || rels_of(e) & !need != 0)
            && e.columns().iter().all(|&c| pos(c).is_some())
    });
    if out.is_empty() {
        return (r, Vec::new());
    }
    let mut sp2 = sp.clone();
    sp2.filter = and_all(keep);
    // (Planned for the LATERAL values: the group plans it again.)
    sp2.access = Access::Unplanned;
    sp2.index = None;
    sp2.reverse = false;
    sp2.written.retain(|w| !out.iter().any(|o| o.same_as(w)));
    let r2 = match r.clone() {
        Plan::Scan(_) => Plan::Scan(sp2),
        Plan::Project { exprs, .. } => Plan::Project {
            input: Arc::new(Plan::Scan(sp2)),
            exprs,
        },
        _ => unreachable!(),
    };
    // (Nothing gained when the side stays LATERAL for no reason of its own.)
    if need == 0 && reads_outer(&r2) {
        return (r, Vec::new());
    }
    let moved = out
        .into_iter()
        .map(|e| {
            let e = e
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(c, t) => Expr::Col(nleft + pos(c).unwrap_or(c), t),
                        x => x,
                    })
                })
                .unwrap();
            subst_outer(e, params)
        })
        .collect();
    (r2, moved)
}

/// `lateral_quals` for a filter over a plan other than a scan: the clauses reading the
/// LATERAL values of relations the plan's own references don't need, as join clauses.
fn filter_lateral_quals(
    input: &Arc<Plan>,
    pred: &Expr,
    params: &[Expr],
    pinfo: &[(u32, u64)],
    nleft: usize,
) -> Option<(Plan, Vec<Expr>)> {
    let rels_of = |e: &Expr| outer_refs(e).iter().fold(0u64, |m, &i| m | pinfo.get(i).map_or(u64::MAX, |x| x.1));
    let fine = |e: &Expr| {
        let mut fine = !e.is_volatile();
        e.walk(&mut |x| match x {
            Expr::Subquery(_) | Expr::Param(..) => fine = false,
            Expr::Outer(d, ..) if *d != 1 => fine = false,
            _ => {}
        });
        fine
    };
    let mut below = Vec::new();
    crate::sql::explain::node_exprs(input, &mut below);
    let need = below.iter().fold(0u64, |m, e| m | rels_of(e));
    if reads_outer_below(input) {
        return None;
    }
    let mut parts = Vec::new();
    conjuncts(pred.clone(), &mut parts);
    let (out, keep): (Vec<Expr>, Vec<Expr>) = parts
        .into_iter()
        .partition(|e| !outer_refs(e).is_empty() && fine(e) && (need == 0 || rels_of(e) & !need != 0));
    if out.is_empty() {
        return None;
    }
    let r2 = match and_all(keep) {
        Some(k) => Plan::Filter { input: input.clone(), pred: k },
        None => (**input).clone(),
    };
    let moved = out
        .into_iter()
        .map(|e| subst_outer(e.shift_cols(nleft as isize), params))
        .collect();
    Some((r2, moved))
}

/// Whether a plan's inputs (not the node itself) read the LATERAL values.
fn reads_outer_below(p: &Plan) -> bool {
    crate::sql::explain::plan_inputs(p).into_iter().any(|c| reads_outer(c))
}

/// A LATERAL scan's clauses over a pulled-up subquery's PlaceHolderVar that reads only the
/// LATERAL values (`phv_quals`): the PlaceHolderVar is a nestloop parameter of its own
/// (computed on the outer side, a Memoize's cache key), the clauses read it as one.
fn hoist_phv_params(r: Plan, mut params: Vec<Expr>) -> (Plan, Vec<Expr>) {
    let Plan::Scan(sp) = &r else {
        return (r, params);
    };
    if sp.phv_quals.is_empty() || !matches!(sp.access, Access::Unplanned) || sp.index.is_some() {
        return (r, params);
    }
    let mut changed = false;
    let mut hoist = |e: &Expr, params: &mut Vec<Expr>| -> Expr {
        e.clone()
            .transform(&mut |x| {
                if x.columns().is_empty()
                    && !matches!(x, Expr::Outer(..) | Expr::Const(..))
                    && !outer_refs(&x).is_empty()
                    && movable(&x)
                    && outer_refs(&x).iter().all(|i| *i < params.len())
                {
                    let v = subst_outer(x.clone(), params);
                    let ty = x.ty();
                    let k = match params.iter().position(|p| p.same_as(&v)) {
                        Some(k) => k,
                        None => {
                            params.push(v);
                            params.len() - 1
                        }
                    };
                    changed = true;
                    return Ok(Expr::Outer(1, k, ty));
                }
                Ok(x)
            })
            .unwrap()
    };
    let mut sp2 = (**sp).clone();
    let phv: Vec<Expr> = sp2.phv_quals.iter().map(|q| hoist(q, &mut params)).collect();
    let fix = |q: &Expr, params: &mut Vec<Expr>, hoist: &mut dyn FnMut(&Expr, &mut Vec<Expr>) -> Expr| {
        if sp.phv_quals.iter().any(|p| p.same_as(q)) { hoist(q, params) } else { q.clone() }
    };
    let mut parts = Vec::new();
    if let Some(f) = &sp2.filter {
        conjuncts(f.clone(), &mut parts);
    }
    let parts: Vec<Expr> = parts.iter().map(|q| fix(q, &mut params, &mut hoist)).collect();
    sp2.written = sp2.written.iter().map(|q| fix(q, &mut params, &mut hoist)).collect();
    sp2.filter = and_all(parts);
    sp2.phv_quals = phv;
    if !changed {
        return (r, params);
    }
    (Plan::Scan(Box::new(sp2)), params)
}

/// A LATERAL scan's parameters it still reads, renumbered (the left side's values the join
/// passes down are what its references need, no more).
fn prune_params(r: Plan, params: Vec<Expr>) -> (Plan, Vec<Expr>) {
    // (A VALUES list's: the ones its rows read.)
    let values = match &r {
        Plan::Values { .. } => true,
        Plan::Filter { input, .. } => matches!(&**input, Plan::Values { .. }),
        _ => false,
    };
    if values {
        let mut exprs = Vec::new();
        crate::sql::explain::node_exprs(&r, &mut exprs);
        if let Plan::Filter { input, .. } = &r {
            crate::sql::explain::node_exprs(input, &mut exprs);
        }
        let mut used = vec![false; params.len()];
        for e in &exprs {
            for i in outer_refs(e) {
                match used.get_mut(i) {
                    Some(u) => *u = true,
                    None => return (r, params),
                }
            }
        }
        if used.iter().all(|u| *u) {
            return (r, params);
        }
        let pos: Vec<Option<usize>> = used
            .iter()
            .scan(0usize, |n, &u| {
                Some(u.then(|| {
                    *n += 1;
                    *n - 1
                }))
            })
            .collect();
        let fix = |e: &Expr| -> Expr {
            e.clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Outer(1, i, t) => Expr::Outer(1, pos[i].unwrap_or(i), t),
                        x => x,
                    })
                })
                .unwrap()
        };
        let fix_values = |v: &Plan| match v {
            Plan::Values { rows, ncols } => Plan::Values {
                rows: rows.iter().map(|row| row.iter().map(&fix).collect()).collect(),
                ncols: *ncols,
            },
            p => p.clone(),
        };
        let r2 = match &r {
            Plan::Filter { input, pred } => Plan::Filter {
                input: Arc::new(fix_values(input)),
                pred: fix(pred),
            },
            v => fix_values(v),
        };
        let params = params.into_iter().zip(&used).filter(|(_, u)| **u).map(|(p, _)| p).collect();
        return (r2, params);
    }
    let scan_only = match &r {
        Plan::Scan(_) => true,
        Plan::Project { input, .. } => matches!(&**input, Plan::Scan(_)),
        _ => false,
    };
    if !scan_only {
        return (r, params);
    }
    let mut used = vec![false; params.len()];
    let mut exprs = Vec::new();
    crate::sql::explain::node_exprs(&r, &mut exprs);
    let scan = match &r {
        Plan::Project { input, .. } => &**input,
        p => p,
    };
    if !std::ptr::eq(scan, &r) {
        crate::sql::explain::node_exprs(scan, &mut exprs);
    }
    if let Plan::Scan(sp) = scan {
        // (A path planned over them reads them in its conditions too.)
        if !matches!(sp.access, Access::Unplanned) || sp.index.is_some() {
            return (r, params);
        }
        exprs.extend(sp.written.iter().chain(&sp.phv_quals).chain(&sp.current_of));
    }
    for e in &exprs {
        for i in outer_refs(e) {
            match used.get_mut(i) {
                Some(u) => *u = true,
                None => return (r, params),
            }
        }
    }
    if used.iter().all(|u| *u) {
        return (r, params);
    }
    let pos: Vec<Option<usize>> = used
        .iter()
        .scan(0usize, |n, &u| {
            Some(u.then(|| {
                *n += 1;
                *n - 1
            }))
        })
        .collect();
    let fix = |e: &Expr| -> Expr {
        e.clone()
            .transform(&mut |x| {
                Ok(match x {
                    Expr::Outer(1, i, t) => Expr::Outer(1, pos[i].unwrap_or(i), t),
                    x => x,
                })
            })
            .unwrap()
    };
    let fix_scan = |sp: &ScanPlan| {
        let mut sp = sp.clone();
        sp.filter = sp.filter.as_ref().map(fix);
        sp.written = sp.written.iter().map(&fix).collect();
        sp.phv_quals = sp.phv_quals.iter().map(&fix).collect();
        sp
    };
    let r2 = match &r {
        Plan::Scan(sp) => Plan::Scan(Box::new(fix_scan(sp))),
        Plan::Project { input, exprs } => match &**input {
            Plan::Scan(sp) => Plan::Project {
                input: Arc::new(Plan::Scan(Box::new(fix_scan(sp)))),
                exprs: exprs.iter().map(&fix).collect(),
            },
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    let params = params.into_iter().zip(&used).filter(|(_, u)| **u).map(|(p, _)| p).collect();
    (r2, params)
}

/// The join tree at `p` with its subqueries' liftable outputs taken out of their scans;
/// None when nothing moves.
pub(super) fn lift_group(p: &Plan) -> Option<Lifted> {
    if !matches!(p, Plan::Join { .. }) {
        return None;
    }
    let mut cx = Cx {
        next: 0,
        lifted: false,
        bad: false,
        nrel: 0,
        results: false,
    };
    let (plan, map, _) = lift(p, 0, None, &mut cx);
    if !cx.lifted || cx.bad {
        return None;
    }
    // (Columns read through the map keep their types.)
    let types = super::super::analyzejoins::col_types(p);
    let map = map
        .into_iter()
        .zip(types)
        .map(|(e, t)| match e {
            Expr::Col(j, DataType::Unknown) => Expr::Col(j, t),
            e => e,
        })
        .collect();
    Some(Lifted { plan, map, results: cx.results })
}
