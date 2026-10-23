//! Pulling WHERE-clause sublinks up into joins (prepjointree.c's pull_up_sublinks):
//! `EXISTS (…)` / `NOT EXISTS (…)` over a correlated single-table subquery become a semi /
//! anti join with it (convert_EXISTS_sublink_to_join), `x op ANY (SELECT …)` over an
//! uncorrelated one a semi join (convert_ANY_sublink_to_join). The join planner then picks
//! how to run them.

use super::optimize::{conjuncts, width};
use crate::catalog::Catalog;
use crate::sql::expr::{Expr, Subquery, SubqueryKind};
use crate::sql::plan::{Access, JoinKind, Plan, ScanPlan};
use crate::types::Value;
use std::sync::Arc;

fn and_all(mut items: Vec<Expr>) -> Option<Expr> {
    match items.len() {
        0 => None,
        1 => items.pop(),
        _ => Some(Expr::And(items)),
    }
}

/// `pred` (the WHERE clause over `plan`'s rows) with its top-level EXISTS / NOT EXISTS / ANY
/// conjuncts turned into semi / anti joins over `plan`, in order; the rest of the clause.
pub(super) fn pull_up(
    cat: &Catalog,
    ctx: &crate::exec::ExecCtx,
    plan: Plan,
    pred: Expr,
) -> (Plan, Option<Expr>) {
    let mut parts = Vec::new();
    conjuncts(pred, &mut parts);
    let parts: Vec<Expr> = parts
        .into_iter()
        .map(|e| values_to_any(e, ctx))
        .map(one_row_to_ops)
        // (A test that became constant true goes.)
        .filter(|e| !matches!(e, Expr::Const(Value::Bool(true), _)))
        .collect();
    // The clauses that stay restrictions of the FROM items go down to them first (the join
    // planner distributes them there), below the semi / anti joins.
    let pulls = |e: &Expr, plan: &Plan| -> bool {
        let (sub, anti) = match e {
            Expr::Subquery(s) => (s, false),
            Expr::Not(x) => match &**x {
                Expr::Subquery(s) if s.kind == SubqueryKind::Exists => (s, true),
                _ => return false,
            },
            _ => return false,
        };
        match sub.kind {
            SubqueryKind::Exists => exists_join(cat, plan, sub, anti).is_some(),
            SubqueryKind::Any if !anti => any_join(cat, plan, sub).is_some(),
            _ => false,
        }
    };
    let (subs, stay): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|e| pulls(e, &plan));
    if subs.is_empty() {
        return (plan, and_all(stay));
    }
    let mut plan = match and_all(stay) {
        Some(f) if !has_sublink(&f) || matches!(plan, Plan::Scan(_)) => {
            super::optimize::add_filter(cat, plan, f)
        }
        Some(f) => {
            // (Kept above the joins.)
            let (p, rest) = pull_up_parts(cat, plan, subs);
            let mut r: Vec<Expr> = rest.into_iter().collect();
            r.push(f);
            return (p, and_all(r));
        }
        None => plan,
    };
    let (p, rest) = pull_up_parts(cat, plan, subs);
    plan = p;
    (plan, rest)
}

/// The EXISTS / IN conjuncts of `pred` turned into joins over `plan`; the rest.
pub(super) fn pull_up_only(cat: &Catalog, plan: Plan, pred: Expr) -> (Plan, Option<Expr>) {
    let mut parts = Vec::new();
    conjuncts(pred, &mut parts);
    pull_up_parts(cat, plan, parts)
}

/// The sublinks `parts` turned into joins over `plan`, in order; the ones that weren't.
fn pull_up_parts(cat: &Catalog, plan: Plan, parts: Vec<Expr>) -> (Plan, Option<Expr>) {
    let mut plan = plan;
    let mut rest = Vec::new();
    for p in parts {
        let (sub, anti) = match &p {
            Expr::Subquery(s) => (s, false),
            Expr::Not(x) => match &**x {
                Expr::Subquery(s) if s.kind == SubqueryKind::Exists => (s, true),
                _ => {
                    rest.push(p);
                    continue;
                }
            },
            _ => {
                rest.push(p);
                continue;
            }
        };
        let joined = match sub.kind {
            SubqueryKind::Exists => exists_join(cat, &plan, sub, anti),
            SubqueryKind::Any if !anti => any_join(cat, &plan, sub),
            _ => None,
        }
        .map(|j| reduce_unique(cat, j));
        match joined {
            Some(j) => plan = j,
            None => rest.push(p),
        }
    }
    (plan, and_all(rest))
}

/// The table scan a subquery reads, when it is just that: no sampling, locking, LIMIT or
/// partitioned parent, nothing in its filter but plain expressions.
fn plain_scan(p: &Plan) -> Option<&ScanPlan> {
    let Plan::Scan(s) = p else { return None };
    let subs = EXISTS_SUBLINKS.with(|c| c.get());
    let ok = s.sample.is_none()
        && s.current_of.is_none()
        && s.mark.is_none()
        && !s.with_key
        && !s.sys
        && s.table.partkey.is_none()
        && s.filter.as_ref().is_none_or(|f| {
            !deep_outer(f) && if subs { !has_aggregates(f) && shallow_sublinks(f) } else { !has_sublink(f) }
        });
    ok.then_some(&**s)
}

thread_local! {
    /// An EXISTS's WHERE clause is being pulled up: its sub-selects go along (SubPlans of
    /// the semi join's clause), when they read nothing further out than this query.
    static EXISTS_SUBLINKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The flags not at rest (`scoped::unsettled_for_test`).
pub(super) fn unsettled(out: &mut Vec<&'static str>) {
    if EXISTS_SUBLINKS.get() {
        out.push("sublink::EXISTS_SUBLINKS");
    }
}

fn has_aggregates(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| found |= matches!(x, Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..)));
    found
}

/// Its sub-selects read nothing past their own parameters (no reference two levels up from
/// their own rows).
fn shallow_sublinks(e: &Expr) -> bool {
    fn plan_ok(p: &Plan) -> bool {
        let mut exprs = Vec::new();
        crate::sql::explain::node_exprs(p, &mut exprs);
        exprs.iter().all(|x| {
            let mut ok = true;
            x.walk(&mut |y| {
                if let Expr::Outer(d, ..) = y
                    && *d >= 2
                {
                    ok = false;
                }
            });
            ok
        }) && crate::sql::explain::plan_inputs(p).iter().all(|c| plan_ok(c))
    }
    let mut ok = true;
    e.walk(&mut |x| {
        if let Expr::Subquery(s) = x {
            ok &= s.func.is_none() && plan_ok(&s.plan);
        }
    });
    ok
}

fn has_sublink(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if matches!(x, Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..)) {
            found = true
        }
    });
    found
}

/// A reference to a query two or more levels up.
fn deep_outer(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if let Expr::Outer(d, ..) = x
            && *d != 1
        {
            found = true
        }
    });
    found
}

fn reads_outer(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if let Expr::Outer(1, ..) = x {
            found = true
        }
    });
    found
}

/// The scan as the right side of the join: its own quals only, its path chosen again.
fn rhs_scan(s: &ScanPlan, filter: Option<Expr>) -> Plan {
    let mut s = s.clone();
    s.filter = filter;
    // (A LIMIT pushed into the scan went with the EXISTS's LIMIT.)
    s.limit = None;
    s.index = None;
    s.reverse = false;
    s.access = Access::Unplanned;
    Plan::Scan(Box::new(s))
}

/// The subquery's one FROM item as a join's right side: a table scan, or the member scans
/// of a table's inheritance / partition tree (each passing the parent's columns through).
/// Its quals that read the outer query (over its row: the same for every member), the
/// right side with its own quals only, its column types, and whether a member scan has a
/// LIMIT pushed into it.
fn sub_rel(p: &Plan) -> Option<(Vec<Expr>, Plan, Vec<crate::types::DataType>, bool)> {
    let split = |s: &ScanPlan| {
        let mut parts = Vec::new();
        if let Some(f) = &s.filter {
            conjuncts(f.clone(), &mut parts);
        }
        parts.into_iter().partition::<Vec<Expr>, _>(reads_outer)
    };
    if let Some(s) = plain_scan(p) {
        let (corr, local) = split(s);
        let types = s.table.columns.iter().map(|c| c.ty.clone()).collect();
        return Some((corr, rhs_scan(s, and_all(local)), types, s.limit.is_some()));
    }
    // A system catalog: the same.
    if let Plan::SysScan { table, filter, alias, .. } = p {
        if filter.as_ref().is_some_and(|f| has_sublink(f) || deep_outer(f)) {
            return None;
        }
        let mut parts = Vec::new();
        if let Some(f) = filter {
            conjuncts(f.clone(), &mut parts);
        }
        let (corr, local): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(reads_outer);
        let types = table.columns().iter().map(|(_, t)| t.clone()).collect();
        return Some((
            corr,
            Plan::SysScan {
                table: *table,
                filter: and_all(local),
                path: None,
                alias: alias.clone(),
            },
            types,
            false,
        ));
    }
    // A materialized CTE's scan: uncorrelated rows of its own.
    if let Plan::CteScan { ncols, .. } = p {
        return Some((Vec::new(), p.clone(), vec![crate::types::DataType::Unknown; *ncols], false));
    }
    let Plan::Append {
        inputs,
        inh: inh @ Some(_),
        ..
    } = p
    else {
        return None;
    };
    let w = width(p);
    let mut corr: Option<Vec<Expr>> = None;
    let mut members = Vec::new();
    let mut limited = false;
    let mut types = Vec::new();
    for m in inputs {
        let (s, proj) = match &**m {
            // (A member whose columns are the parent's in another order, a multiply
            // inherited child's: its clauses mapped to the parent's columns.)
            Plan::Project { input, exprs }
                if exprs.len() == w && exprs.iter().all(|e| matches!(e, Expr::Col(..))) =>
            {
                (plain_scan(input)?, Some(exprs.clone()))
            }
            Plan::Scan(_) => (plain_scan(m)?, None),
            _ => return None,
        };
        if s.table.columns.len() < w {
            return None;
        }
        let (c, local) = split(s);
        // (Over the parent's columns.)
        let c: Vec<Expr> = match &proj {
            Some(exprs) if exprs.iter().enumerate().any(|(i, e)| !matches!(e, Expr::Col(j, _) if *j == i)) => {
                let mut ok = true;
                let mapped = c
                    .into_iter()
                    .map(|e| {
                        e.transform(&mut |x| {
                            Ok(match x {
                                Expr::Col(j, t) => match exprs.iter().position(|e| matches!(e, Expr::Col(k, _) if *k == j)) {
                                    Some(i) => Expr::Col(i, t),
                                    None => {
                                        ok = false;
                                        Expr::Col(j, t)
                                    }
                                },
                                x => x,
                            })
                        })
                        .unwrap()
                    })
                    .collect();
                if !ok {
                    return None;
                }
                mapped
            }
            _ => c,
        };
        match &corr {
            None => corr = Some(c),
            Some(prev) => {
                if prev.len() != c.len() || prev.iter().zip(&c).any(|(a, b)| !a.same_as(b)) {
                    return None;
                }
            }
        }
        limited |= s.limit.is_some();
        if types.is_empty() {
            types = match &proj {
                Some(exprs) => exprs.iter().map(|e| e.ty()).collect(),
                None => s.table.columns[..w].iter().map(|c| c.ty.clone()).collect(),
            };
        }
        let scan = rhs_scan(s, and_all(local));
        members.push(Arc::new(match proj {
            Some(exprs) => Plan::Project {
                input: Arc::new(scan),
                exprs,
            },
            None => scan,
        }));
    }
    let corr = corr?;
    Some((
        corr,
        Plan::Append {
            inputs: members,
            inh: inh.clone(),
            alias: None,
            pw: None,
        },
        types,
        limited,
    ))
}

/// A subquery whose FROM list joins tables: its WHERE clauses that read the outer query
/// (where the binder pushed them: filters, inner joins' conditions, the scans below inner
/// joins and outer joins' preserved sides; over the subquery's row), and the rest of it.
/// None when something else in it reads the outer query (PG keeps such a sublink), or it
/// has nodes other than scans, filters and joins.
fn sub_join(p: &Plan) -> Option<(Vec<Expr>, Plan)> {
    fn clean(e: &Expr) -> bool {
        let mut ok = true;
        e.walk(&mut |x| {
            if matches!(
                x,
                Expr::Outer(..) | Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..)
            ) {
                ok = false
            }
        });
        ok
    }
    fn split(e: &Expr, off: usize, out: &mut Vec<Expr>) -> Option<Option<Expr>> {
        let mut parts = Vec::new();
        conjuncts(e.clone(), &mut parts);
        let mut local = Vec::new();
        for q in parts {
            if clean(&q) {
                local.push(q);
            } else if reads_outer(&q) && !has_sublink(&q) && !deep_outer(&q) && !q.is_volatile() {
                out.push(q.shift_cols(off as isize));
            } else {
                return None;
            }
        }
        Some(and_all(local))
    }
    // `pull`: whether the clauses here can move up to the semi join (else nothing here
    // may read the outer query).
    fn go(p: &Plan, off: usize, pull: bool, out: &mut Vec<Expr>) -> Option<Plan> {
        match p {
            Plan::Scan(_) => {
                let s = plain_scan(p).filter(|s| s.limit.is_none())?;
                let n = out.len();
                let local = match &s.filter {
                    Some(f) if pull => split(f, off, out)?,
                    Some(f) => clean(f).then(|| Some(f.clone()))?,
                    None => None,
                };
                Some(if out.len() > n {
                    rhs_scan(s, local)
                } else {
                    Plan::Scan(Box::new(s.clone()))
                })
            }
            Plan::SysScan { table, filter, alias, .. } => {
                let n = out.len();
                let local = match filter {
                    Some(f) if pull => split(f, off, out)?,
                    Some(f) => clean(f).then(|| Some(f.clone()))?,
                    None => None,
                };
                let _ = n;
                Some(Plan::SysScan {
                    table: *table,
                    filter: local,
                    path: None,
                    alias: alias.clone(),
                })
            }
            Plan::Filter { input, pred } => {
                let local = if pull {
                    split(pred, off, out)?
                } else {
                    clean(pred).then(|| Some(pred.clone()))?
                };
                let input = go(input, off, pull, out)?;
                Some(match local {
                    Some(pred) => Plan::Filter {
                        input: Arc::new(input),
                        pred,
                    },
                    None => input,
                })
            }
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
            } if params.is_empty() => {
                let inner = *kind == JoinKind::Inner;
                let cond = match cond {
                    Some(c) if pull && inner => split(c, off, out)?,
                    Some(c) => clean(c).then(|| Some(c.clone()))?,
                    None => None,
                };
                if let Some((l, r)) = hash
                    && !l.iter().chain(r).all(clean)
                {
                    return None;
                }
                let (lp, rp) = match kind {
                    JoinKind::Inner => (pull, pull),
                    JoinKind::Left | JoinKind::Semi | JoinKind::Anti => (pull, false),
                    JoinKind::Right => (false, pull),
                    _ => (false, false),
                };
                let l = go(left, off, lp, out)?;
                let r = go(right, off + nleft, rp, out)?;
                Some(Plan::Join {
                    kind: *kind,
                    left: Arc::new(l),
                    right: Arc::new(r),
                    cond,
                    hash: hash.clone(),
                    params: Vec::new(),
                    nleft: *nleft,
                    nright: *nright,
                    meta: meta.clone(),
                })
            }
            // A merge join input's sort (the subquery planned already): the pulled-up
            // tree is planned again as one group, and an EXISTS needs no order.
            Plan::Sort { input, .. } if pull => go(input, off, pull, out),
            // A FROM subquery's projection reading nothing of the outer query (on a side
            // the outer query's clauses can't come from).
            Plan::Project { input, exprs } if !pull && exprs.iter().all(clean) => {
                let mut none = Vec::new();
                let input = go(input, off, false, &mut none)?;
                Some(Plan::Project {
                    input: Arc::new(input),
                    exprs: exprs.clone(),
                })
            }
            // Anything else (a FROM subquery planned already) on a side nothing of the
            // outer query may come from: as it is, when it reads nothing of it.
            _ if !pull && plan_clean(p) => Some(p.clone()),
            _ => None,
        }
    }
    if !matches!(p, Plan::Join { .. } | Plan::Filter { .. }) {
        return None;
    }
    let mut out = Vec::new();
    let rest = go(p, 0, true, &mut out)?;
    Some((out, rest))
}

/// A plan of plain relational nodes reading nothing of an outer query.
fn plan_clean(p: &Plan) -> bool {
    if !matches!(
        p,
        Plan::Scan(_)
            | Plan::SysScan { .. }
            | Plan::Filter { .. }
            | Plan::Project { .. }
            | Plan::Sort { .. }
            | Plan::Join { .. }
            | Plan::Agg { .. }
            | Plan::Window { .. }
            | Plan::Limit { .. }
            | Plan::Distinct { .. }
            | Plan::Append { .. }
            | Plan::Values { .. }
            | Plan::Empty { .. }
            | Plan::One
    ) {
        return false;
    }
    let mut exprs = Vec::new();
    crate::sql::explain::node_exprs(p, &mut exprs);
    let clean = exprs.iter().all(|e| {
        let mut ok = true;
        e.walk(&mut |x| {
            if matches!(x, Expr::Outer(..) | Expr::Subquery(_) | Expr::Srf(..)) {
                ok = false
            }
        });
        ok
    });
    clean && crate::sql::explain::plan_inputs(p).iter().all(|c| plan_clean(c))
}

/// A VALUES list reading nothing of an outer query: its sub-selects (SubPlans of its
/// own) uncorrelated too.
fn values_clean(p: &Plan) -> bool {
    let Plan::Values { rows, .. } = p else { return false };
    rows.iter().flatten().all(|e| {
        let mut ok = true;
        e.walk(&mut |x| match x {
            Expr::Outer(..) | Expr::Srf(..) => ok = false,
            Expr::Subquery(s) if !s.params.is_empty() => ok = false,
            _ => {}
        });
        ok
    })
}

/// A VALUES list reading the outer query's values (one level up) and nothing worse: no
/// set-returning or volatile calls; its sub-selects (SubPlans of the Values Scan) read
/// nothing further out either.
fn values_outer(p: &Plan) -> bool {
    let Plan::Values { rows, .. } = p else { return false };
    let mut outer = false;
    let ok = rows.iter().flatten().all(|e| {
        let mut ok = !crate::sql::explain::expr_volatile_deep(e);
        e.walk(&mut |x| match x {
            Expr::Outer(1, ..) => outer = true,
            Expr::Outer(..) | Expr::Srf(..) => ok = false,
            Expr::Subquery(s) => ok &= s.func.is_none(),
            _ => {}
        });
        ok
    });
    ok && outer
}

/// convert_EXISTS_sublink_to_join.
fn exists_join(cat: &Catalog, plan: &Plan, sub: &Subquery, anti: bool) -> Option<Plan> {
    // simplify_EXISTS_query: the select list and a positive constant LIMIT don't matter.
    let mut p: &Plan = &sub.plan;
    let mut limited = false;
    loop {
        match p {
            Plan::Project { input, exprs } if !exprs.iter().any(has_sublink) => p = input,
            // (Its GROUP BY without aggregates, DISTINCT and ORDER BY go too.)
            Plan::Agg {
                input,
                groups,
                sets: None,
                aggs,
            } if aggs.is_empty() && !groups.iter().any(has_sublink) => p = input,
            Plan::Distinct { input, .. } | Plan::Sort { input, .. } => p = input,
            // (LIMIT NULL / ALL: no limit at all.)
            Plan::Limit {
                input,
                limit: None,
                offset: None,
                with_ties: None,
            } => p = input,
            Plan::Limit {
                input,
                limit: Some(l),
                offset: None,
                with_ties: None,
            } if {
                // (Through the coercion to bigint of `LIMIT NULL`.)
                let mut c = l;
                while let Expr::Cast(x, ..) | Expr::Relabel(x, _) = c {
                    c = x;
                }
                match c {
                    Expr::Const(Value::Null, _) => true,
                    Expr::Const(Value::Int8(n), _) => *n > 0,
                    Expr::Const(Value::Int4(n), _) => *n > 0,
                    _ => false,
                }
            } =>
            {
                limited = true;
                p = input
            }
            _ => break,
        }
    }
    // EXISTS over a semi / anti join whose right side doesn't read the left one (a NOT
    // EXISTS in the subquery's WHERE that reads only the outer query): that sublink joins
    // the outer query itself (pull_up_sublinks with the outer rels available), then the
    // rest is an EXISTS of the left side.
    if !anti
        && let Plan::Join {
            kind: k @ (JoinKind::Semi | JoinKind::Anti),
            left,
            right,
            cond,
            hash: None,
            params,
            nleft,
            ..
        } = p
        && params.is_empty()
        && cond.as_ref().is_none_or(|c| c.columns().iter().all(|&x| x >= *nleft))
    {
        let mut r = (**right).clone();
        if let Some(c) = cond {
            r = super::optimize::add_filter(cat, r, c.clone().shift_cols(-(*nleft as isize)));
        }
        let sub_r = Subquery {
            plan: Arc::new(r),
            ..sub.clone()
        };
        let sub_l = Subquery {
            plan: left.clone(),
            ..sub.clone()
        };
        let inner = exists_join(cat, plan, &sub_r, *k == JoinKind::Anti)?;
        // (The outer rows' columns are where they were: the inner join keeps them first.)
        return exists_join(cat, &inner, &sub_l, false);
    }
    // A subquery that returns nothing: EXISTS is false (the semi join a dummy rel),
    // NOT EXISTS true.
    if let Plan::Empty { .. } = p
        && !sub.params.is_empty()
    {
        if anti {
            return Some(plan.clone());
        }
        let (cols, info) = match plan {
            Plan::Scan(s) => {
                let names = super::optimize::scan_names(s);
                let rels = vec![Some(super::optimize::scan_refname(s)); names.len()];
                (
                    Some(Arc::new(names)),
                    Some(Arc::new(crate::sql::plan::DummyInfo { rels, ec: Vec::new(), table: None, ..Default::default() })),
                )
            }
            _ => (None, None),
        };
        return Some(Plan::Empty {
            ncols: width(plan),
            cols,
            info,
        });
    }
    let rel = {
        let _g = crate::scoped::set(&EXISTS_SUBLINKS, true, "sublink::EXISTS_SUBLINKS");
        sub_rel(p)
    };
    let (corr, rhs, _, scan_limited) = match rel {
        Some(x) => x,
        None => {
            let (corr, rhs) = sub_join(p)?;
            (corr, rhs, Vec::new(), false)
        }
    };
    if scan_limited && !limited {
        return None;
    }
    // Uncorrelated: it stays an InitPlan. (A sub-select's own volatility counts.)
    if corr.is_empty() || corr.iter().any(crate::sql::explain::expr_volatile_deep) {
        return None;
    }
    // (Its local quals keep no sub-select: they would be the semi join's side's own.)
    if let Plan::Scan(s) = &rhs
        && s.filter.as_ref().is_some_and(has_sublink)
    {
        return None;
    }
    let nleft = width(plan);
    let cond: Vec<Expr> = corr
        .into_iter()
        .map(|e| outer_to_params(e.shift_cols(nleft as isize), &sub.params))
        .collect::<Option<_>>()?;
    let nright = width(&rhs);
    // (A semi join's clauses are equivalence classes: one equating an outer column with a
    // column the subquery fixes to a constant fixes the outer one too, and goes.)
    let (left, rhs, cond) = if anti { (plan.clone(), rhs, cond) } else { semi_constants(cat, plan, rhs, cond, nleft) };
    Some(super::optimize::join(
        cat,
        if anti { JoinKind::Anti } else { JoinKind::Semi },
        left,
        rhs,
        and_all(cond),
        Vec::new(),
        nleft,
        nright,
    ))
}

/// generate_base_implied_equalities_const over a semi join: `l = r` where the right side's
/// scan has `r = k` gives the left side `l = k`, and where the left side's scans have `l = k`
/// the right side `r = k`: the join clause implied by the two.
fn semi_constants(cat: &Catalog, left: &Plan, rhs: Plan, cond: Vec<Expr>, nleft: usize) -> (Plan, Plan, Vec<Expr>) {
    let rfixed = match &rhs {
        Plan::Scan(rs) => scan_constants(rs),
        _ => Vec::new(),
    };
    let lfixed = plan_constants(left);
    if rfixed.is_empty() && lfixed.is_empty() {
        return (left.clone(), rhs, cond);
    }
    // `col op k` with the comparison `q` (`col = k` or `k = col`) as written.
    let derive = |q: &Expr, col: Expr, k: &Expr| {
        let Expr::Call(qd, qa, qt) = q else { unreachable!() };
        if matches!(&qa[0], Expr::Col(..)) {
            Expr::Call(qd.clone(), vec![col, k.clone()], qt.clone())
        } else {
            Expr::Call(qd.clone(), vec![k.clone(), col], qt.clone())
        }
    };
    let mut keep = Vec::new();
    let mut push_left = Vec::new();
    let mut push_right = Vec::new();
    for c in cond {
        let pair = match &c {
            Expr::Call(d, args, _) if d.name == "=" && args.len() == 2 && d.coll.as_ref().is_none_or(|c| c.deterministic) => {
                let side = |e: &Expr| match e {
                    Expr::Col(i, t) => Some((*i, t.clone())),
                    _ => None,
                };
                match (side(&args[0]), side(&args[1])) {
                    (Some((l, lt)), Some((r, rt))) | (Some((r, rt)), Some((l, lt)))
                        if l < nleft && r >= nleft && lt.without_typmod() == rt.without_typmod() =>
                    {
                        Some((l, lt, r - nleft, rt))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let Some((l, lt, r, rt)) = pair else {
            keep.push(c);
            continue;
        };
        if let Some((_, k, q)) = rfixed.iter().find(|(i, _, _)| *i == r) {
            push_left.push(derive(q, Expr::Col(l, lt), k));
        } else if let Some((_, k, q)) = lfixed.iter().find(|(i, _, _)| *i == l) {
            push_right.push(derive(q, Expr::Col(r, rt), k));
        } else {
            keep.push(c);
        }
    }
    let left = if push_left.is_empty() {
        left.clone()
    } else {
        super::optimize::add_filter(cat, left.clone(), Expr::And(push_left))
    };
    let rhs = if push_right.is_empty() {
        rhs
    } else {
        super::optimize::add_filter(cat, rhs, Expr::And(push_right))
    };
    (left, rhs, keep)
}

/// A scan's quals equating a column with a constant: (column, constant, the qual).
fn scan_constants(s: &ScanPlan) -> Vec<(usize, Expr, Expr)> {
    let mut fixed = Vec::new();
    if let Some(f) = &s.filter {
        let mut parts = Vec::new();
        conjuncts(f.clone(), &mut parts);
        for q in parts {
            if let Expr::Call(d, args, _) = &q
                && d.name == "="
                && args.len() == 2
                && !q.is_volatile()
                && d.coll.as_ref().is_none_or(|c| c.deterministic)
            {
                for (a, b) in [(&args[0], &args[1]), (&args[1], &args[0])] {
                    if let Expr::Col(i, _) = a
                        && matches!(b, Expr::Const(..))
                    {
                        fixed.push((*i, b.clone(), q.clone()));
                    }
                }
            }
        }
    }
    fixed
}

/// `scan_constants` of a plan's rows: a scan's, or an inheritance tree's that every member
/// has (over the parent's columns).
fn plan_constants(p: &Plan) -> Vec<(usize, Expr, Expr)> {
    match p {
        Plan::Scan(s) => scan_constants(s),
        Plan::Append { inputs, inh: Some(_), .. } if !inputs.is_empty() => {
            let mut common: Option<Vec<(usize, Expr, Expr)>> = None;
            for m in inputs {
                let mine: Vec<(usize, Expr, Expr)> = match &**m {
                    Plan::Scan(s) => scan_constants(s),
                    Plan::Project { input, exprs } => match &**input {
                        Plan::Scan(s) => scan_constants(s)
                            .into_iter()
                            .filter_map(|(j, k, q)| {
                                let i = exprs.iter().position(|e| matches!(e, Expr::Col(c, _) if *c == j))?;
                                let q = q
                                    .transform(&mut |x| {
                                        Ok(match x {
                                            Expr::Col(c, t) if c == j => Expr::Col(i, t),
                                            x => x,
                                        })
                                    })
                                    .ok()?;
                                Some((i, k, q))
                            })
                            .collect(),
                        _ => return Vec::new(),
                    },
                    _ => return Vec::new(),
                };
                common = Some(match common {
                    None => mine,
                    Some(c) => c
                        .into_iter()
                        .filter(|(i, k, _)| mine.iter().any(|(j, l, _)| i == j && k.same_as(l)))
                        .collect(),
                });
            }
            common.unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// The subquery's references to this query: the expressions they stand for.
fn outer_to_params(e: Expr, params: &[Expr]) -> Option<Expr> {
    let mut ok = true;
    let out = e
        .transform(&mut |x| {
            Ok(match x {
                Expr::Outer(1, i, t) => match params.get(i) {
                    Some(p) => p.clone(),
                    None => {
                        ok = false;
                        Expr::Outer(1, i, t)
                    }
                },
                x => x,
            })
        })
        .ok()?;
    ok.then_some(out)
}

/// convert_ANY_sublink_to_join: `left op ANY (SELECT exprs FROM t WHERE …)`; the WHERE
/// clauses reading this query's columns (a correlated one, as Postgres 18 pulls up) join
/// along with the comparisons.
fn any_join(cat: &Catalog, plan: &Plan, sub: &Subquery) -> Option<Plan> {
    if sub.left.is_empty()
        || sub.left.len() != sub.ops.len()
        // (Several columns compare as rows: only `=` is a conjunction of joinable ones.)
        || sub.left.len() > 1 && sub.ops.iter().any(|o| o.name != "=")
        // (A sublink in the left operand stays a SubPlan of the join clause.)
        || sub.left.iter().any(|l| {
            let mut other = false;
            l.walk(&mut |x| other |= matches!(x, Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..)));
            l.columns().is_empty() || crate::sql::explain::expr_volatile_deep(l) || other
        })
    {
        return None;
    }
    // A select list with a set-returning function (is_simple_subquery refuses it): an
    // uncorrelated sub-select joins as a subquery of its own, its select list computed
    // inside it.
    if let Some(j) = any_join_srf(cat, plan, sub) {
        return Some(j);
    }
    // The select list (through the binder's casting projection) over one table scan.
    let mut exprs: Option<Vec<Expr>> = None;
    let mut p: &Plan = &sub.plan;
    while let Plan::Project { input, exprs: ex } = p {
        if ex.iter().any(|e| has_sublink(e) || e.is_volatile()) {
            return None;
        }
        exprs = Some(match exprs {
            None => ex.clone(),
            Some(outer) => outer
                .into_iter()
                .map(|e| {
                    e.transform(&mut |x| {
                        Ok(match x {
                            Expr::Col(i, t) => ex.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                            x => x,
                        })
                    })
                    .unwrap()
                })
                .collect(),
        });
        p = input;
    }
    let mut lateral: Vec<Expr> = Vec::new();
    let (corr, rhs, types, scan_limited) = match sub_rel(p) {
        Some(x) => x,
        // Any other uncorrelated subquery joins as it is: a VALUES list pulled up
        // (pull_up_simple_values), else a subquery of its own ("ANY_subquery").
        None if sub.params.is_empty()
            && (plan_clean(p) || values_clean(p))
            && !matches!(p, Plan::Empty { .. } | Plan::One) =>
        {
            let n = width(p);
            let types = (0..n)
                .map(|i| match p {
                    Plan::Values { rows, .. } => rows[0][i].ty(),
                    _ => sub
                        .ops
                        .get(i)
                        .and_then(|o| o.args.get(1).cloned())
                        .unwrap_or(crate::types::DataType::Unknown),
                })
                .collect();
            let rhs = match p {
                Plan::Values { .. } => p.clone(),
                _ => Plan::Barrier {
                    input: Arc::new(p.clone()),
                    alias: "ANY_subquery".into(),
                    cols: (1..=n).map(|i| format!("column{i}")).collect(),
                    secure: false,
                    // (Its ORDER BY's order is of no use to the semi join above:
                    // truncate_useless_pathkeys.)
                    sorted: matches!(p, Plan::Sort { .. }).then_some(0),
                },
            };
            (Vec::new(), rhs, types, false)
        }
        // A VALUES list reading the outer row: a LATERAL one (its values over the row as
        // the join's parameters); reading only queries further up, their values in place.
        None if !sub.params.is_empty() && values_outer(p) => {
            let Plan::Values { rows, ncols } = p else { unreachable!() };
            let level_only = sub.params.iter().all(|e| e.columns().is_empty() && !e.is_volatile() && !has_sublink(e));
            let rows = if level_only {
                rows.iter()
                    .map(|r| r.iter().map(|e| outer_to_params(e.clone(), &sub.params)).collect::<Option<Vec<_>>>())
                    .collect::<Option<Vec<_>>>()?
            } else {
                lateral = sub.params.clone();
                rows.clone()
            };
            let types = rows[0].iter().map(Expr::ty).collect();
            (Vec::new(), Plan::Values { rows, ncols: *ncols }, types, false)
        }
        None => return None,
    };
    if scan_limited {
        return None;
    }
    // The select list may read queries further up (their values reach this one as its
    // own outer references), not this query's rows.
    let exprs = match exprs {
        Some(ex) if ex.iter().any(Expr::has_outer_refs) => {
            let mut ok = true;
            let ex: Vec<Expr> = ex
                .into_iter()
                .map(|e| {
                    e.transform(&mut |x| {
                        Ok(match x {
                            Expr::Outer(1, i, t) => match sub.params.get(i) {
                                Some(v) if v.columns().is_empty() && !v.is_volatile() => v.clone(),
                                _ => {
                                    ok = false;
                                    Expr::Outer(1, i, t)
                                }
                            },
                            Expr::Outer(..) => {
                                ok = false;
                                x
                            }
                            x => x,
                        })
                    })
                    .unwrap()
                })
                .collect();
            if !ok {
                return None;
            }
            Some(ex)
        }
        ex => ex,
    };
    if !corr.is_empty() && (sub.params.is_empty() || corr.iter().any(|e| e.is_volatile() || has_sublink(e))) {
        return None;
    }
    let nleft = width(plan);
    let nright = width(&rhs);
    let corr: Vec<Expr> = corr
        .into_iter()
        .map(|e| outer_to_params(e.shift_cols(nleft as isize), &sub.params))
        .collect::<Option<_>>()?;
    let exprs = exprs.unwrap_or_else(|| {
        types
            .iter()
            .enumerate()
            .map(|(i, t)| {
                // (A CTE's column: the type its comparison operator takes.)
                let t = match t {
                    crate::types::DataType::Unknown => sub
                        .ops
                        .get(i)
                        .and_then(|o| o.args.get(1).cloned())
                        .unwrap_or(crate::types::DataType::Unknown),
                    t => t.clone(),
                };
                Expr::Col(i, t)
            })
            .collect()
    });
    if exprs.len() != sub.left.len() {
        return None;
    }
    // (The pulled-up subquery's own WHERE clauses come first: deconstruct_jointree
    // distributes the semi join's right side before its qual.)
    let cond: Vec<Expr> = corr
        .into_iter()
        .chain(sub.left.iter().zip(&exprs).zip(&sub.ops).map(|((l, r), op)| {
            Expr::Call(
                op.clone(),
                vec![l.clone(), r.clone().shift_cols(nleft as isize)],
                crate::types::DataType::Bool,
            )
        }))
        .collect();
    Some(super::optimize::join(
        cat,
        JoinKind::Semi,
        plan.clone(),
        rhs,
        and_all(cond),
        lateral,
        nleft,
        nright,
    ))
}

/// `any_join` for an uncorrelated sub-select whose select list returns a set (a
/// ProjectSet under its projections): the semi join's right side is the whole sub-select,
/// a subquery scan ("ANY_subquery") named by its columns.
fn any_join_srf(cat: &Catalog, plan: &Plan, sub: &Subquery) -> Option<Plan> {
    if !sub.params.is_empty() {
        return None;
    }
    let mut p: &Plan = &sub.plan;
    while let Plan::Project { input, exprs } = p {
        if exprs.iter().any(|e| has_sublink(e) || e.is_volatile()) {
            return None;
        }
        p = input;
    }
    let Plan::ProjectSet { input, exprs, srfs } = p else {
        return None;
    };
    let mut ok = !exprs.iter().any(|e| e.is_volatile());
    for e in exprs {
        e.walk(&mut |x| ok &= !matches!(x, Expr::Outer(..) | Expr::Subquery(_)));
    }
    for c in srfs {
        for a in &c.args {
            a.walk(&mut |x| ok &= !matches!(x, Expr::Outer(..) | Expr::Subquery(_)));
            ok &= !a.is_volatile();
        }
    }
    if !ok || !plan_clean(input) {
        return None;
    }
    let n = width(&sub.plan);
    if n != sub.left.len() {
        return None;
    }
    let cols = if sub.names.len() == n {
        (*sub.names).clone()
    } else {
        (1..=n).map(|i| format!("column{i}")).collect()
    };
    let rhs = Plan::Barrier {
        input: sub.plan.clone(),
        alias: "ANY_subquery".into(),
        cols,
        secure: false,
        sorted: None,
    };
    let nleft = width(plan);
    let types: Vec<crate::types::DataType> = sub
        .ops
        .iter()
        .map(|o| o.args.get(1).cloned().unwrap_or(crate::types::DataType::Unknown))
        .collect();
    let cond: Vec<Expr> = sub
        .left
        .iter()
        .zip(&sub.ops)
        .enumerate()
        .map(|(i, (l, op))| {
            Expr::Call(
                op.clone(),
                vec![l.clone(), Expr::Col(nleft + i, types[i].clone())],
                crate::types::DataType::Bool,
            )
        })
        .collect();
    Some(super::optimize::join(cat, JoinKind::Semi, plan.clone(), rhs, and_all(cond), Vec::new(), nleft, n))
}

/// reduce_unique_semijoins: a semi join whose right side (one table) has a unique index
/// every column of which the join's equalities (or its own `col = constant` quals) fix is
/// an inner join. (Its right columns trail the row; nothing above reads them.)
fn reduce_unique(cat: &Catalog, j: Plan) -> Plan {
    let Plan::Join {
        kind: JoinKind::Semi,
        left,
        right,
        cond,
        hash,
        params,
        nleft,
        nright,
        meta,
    } = j
    else {
        return j;
    };
    let mut fixed: Vec<usize> = Vec::new();
    if let (Some((_, rk)), Plan::Scan(s)) = (&hash, &*right) {
        for e in rk {
            if let Expr::Col(c, _) = e {
                fixed.push(*c);
            }
        }
        if let Some(f) = &s.filter {
            let mut parts = Vec::new();
            conjuncts(f.clone(), &mut parts);
            for q in parts {
                if let Expr::Call(d, args, _) = &q
                    && d.name == "="
                    && args.len() == 2
                {
                    for (a, b) in [(&args[0], &args[1]), (&args[1], &args[0])] {
                        if let Expr::Col(c, _) = a
                            && super::optimize::pseudo_constant(b)
                        {
                            fixed.push(*c);
                        }
                    }
                }
            }
        }
    }
    let unique = match &*right {
        Plan::Scan(s) if !fixed.is_empty() => {
            let t = &s.table;
            cat.table_indexes(t)
                .iter()
                .chain(t.pkey_index.and_then(|o| cat.index(o)))
                .any(|ix| {
                    (ix.unique || ix.primary)
                        && ix.valid
                        && ix.predicate.is_none()
                        && ix.columns.iter().all(|c| {
                            matches!(c.key, crate::catalog::IndexKey::Column(k) if fixed.contains(&k))
                        })
                })
        }
        _ => false,
    };
    // (Only the join's equalities may make it unique: one of them must be a key.)
    let joined_key = hash.as_ref().is_some_and(|(_, rk)| !rk.is_empty());
    Plan::Join {
        kind: if unique && joined_key {
            JoinKind::Inner
        } else {
            JoinKind::Semi
        },
        left,
        right,
        cond,
        hash,
        params,
        nleft,
        nright,
        meta,
    }
}

/// convert_VALUES_to_ANY: `x IN (VALUES (a), (b), ...)` of values fixed for the query
/// (no columns, nothing volatile or a sub-select) is `x = ANY (ARRAY[a, b, ...])`.
/// `x op ANY (SELECT e)` over one row without FROM: the semi join with the empty relation
/// pulled up (convert_ANY_sublink_to_join, then remove_useless_result_rtes) leaves the
/// comparison `x op e` (one per column, ANDed). The test expression must read the query's
/// columns, as Postgres pulls up only then.
fn one_row_to_ops(e: Expr) -> Expr {
    let Expr::Subquery(sub) = &e else { return e };
    if sub.kind != SubqueryKind::Any
        || sub.left.is_empty()
        || sub.left.len() != sub.ops.len()
        || !sub.params.is_empty()
        || sub.left.iter().all(|l| l.columns().is_empty())
        || sub.left.iter().any(|l| l.is_volatile() || has_sublink(l))
    {
        return e;
    }
    // (Through the binder's projections, composed.)
    let mut p: &Plan = &sub.plan;
    let mut maps: Vec<&Vec<Expr>> = Vec::new();
    while let Plan::Project { input, exprs } = p {
        maps.push(exprs);
        p = input;
    }
    if !matches!(p, Plan::One) || maps.is_empty() {
        return e;
    }
    let mut row: Vec<Expr> = Vec::new();
    for m in maps.iter().rev() {
        row = m
            .iter()
            .map(|x| {
                x.clone()
                    .transform(&mut |y| {
                        Ok(match y {
                            Expr::Col(i, t) => row.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                            y => y,
                        })
                    })
                    .unwrap()
            })
            .collect();
    }
    if row.len() != sub.left.len()
        || row
            .iter()
            .any(|v| !v.columns().is_empty() || v.is_volatile() || has_sublink(v) || v.has_outer_refs())
    {
        return e;
    }
    let mut cmps: Vec<Expr> = sub
        .left
        .iter()
        .zip(&sub.ops)
        .zip(row)
        .map(|((l, op), v)| Expr::Call(op.clone(), vec![l.clone(), v], crate::types::DataType::Bool))
        .collect();
    if cmps.len() == 1 { cmps.pop().unwrap() } else { Expr::And(cmps) }
}

fn values_to_any(e: Expr, ctx: &crate::exec::ExecCtx) -> Expr {
    let Expr::Subquery(sub) = &e else { return e };
    if sub.kind != SubqueryKind::Any || sub.left.len() != 1 || sub.ops.len() != 1 || !sub.params.is_empty() {
        return e;
    }
    // (Through the binder's casting projections.)
    let mut p: &Plan = &sub.plan;
    let mut maps: Vec<&Vec<Expr>> = Vec::new();
    while let Plan::Project { input, exprs } = p {
        if exprs.len() != 1 {
            return e;
        }
        // (Only coercions: a select list computing something over the VALUES is a FROM
        // subquery's, not a VALUES query.)
        let mut x = &exprs[0];
        while let Expr::Cast(inner, ..)
        | Expr::Relabel(inner, _)
        | Expr::CoerceToDomain(inner, _)
        | Expr::Collate(inner, ..) = x
        {
            x = inner;
        }
        if !matches!(x, Expr::Col(0, _)) {
            return e;
        }
        maps.push(exprs);
        p = input;
    }
    let Plan::Values { rows, .. } = p else { return e };
    if rows.len() < 2 || rows.iter().any(|r| r.len() != 1) {
        return e;
    }
    let mut elems = Vec::with_capacity(rows.len());
    for r in rows {
        let mut v = r[0].clone();
        for m in maps.iter().rev() {
            let inner = v.clone();
            v = m[0]
                .clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(0, _) => inner.clone(),
                        x => x,
                    })
                })
                .unwrap();
        }
        // (Only an array of constants: each value as eval_const_expressions leaves it.)
        match fold_const(v, ctx) {
            Some(c) => elems.push(c),
            None => return e,
        }
    }
    let elem_ty = elems[0].ty();
    if elems.iter().any(|x| x.ty() != elem_ty) {
        return e;
    }
    let arr_ty = crate::types::DataType::array_of(elem_ty.clone());
    let items = elems
        .into_iter()
        .map(|x| match x {
            Expr::Const(v, _) => v,
            _ => unreachable!(),
        })
        .collect();
    let right = Expr::Const(
        Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(elem_ty, items))),
        arr_ty,
    );
    let saop = Expr::ArrayOp {
        left: Box::new(sub.left[0].clone()),
        right: Box::new(right),
        op: sub.ops[0].clone(),
        all: false,
    };
    // (Over a constant too, the whole test is one.)
    fold_const(saop.clone(), ctx).unwrap_or(saop)
}

/// `v` evaluated, when it is a constant expression (no columns, parameters, outer values,
/// sub-selects or volatile calls).
fn fold_const(v: Expr, ctx: &crate::exec::ExecCtx) -> Option<Expr> {
    if let Expr::Const(..) = v {
        return Some(v);
    }
    let mut plain = v.columns().is_empty() && v.is_immutable() && !has_sublink(&v);
    v.walk(&mut |x| plain &= !matches!(x, Expr::Param(..) | Expr::Outer(..)));
    if !plain {
        return None;
    }
    let ty = v.ty();
    crate::sql::expr::eval(&v, &[], ctx).ok().map(|c| Expr::Const(c, ty))
}
