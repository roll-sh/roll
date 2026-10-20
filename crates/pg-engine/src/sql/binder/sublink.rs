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
pub(super) fn pull_up(cat: &Catalog, plan: Plan, pred: Expr) -> (Plan, Option<Expr>) {
    let mut parts = Vec::new();
    conjuncts(pred, &mut parts);
    let parts: Vec<Expr> = parts.into_iter().map(values_to_any).collect();
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
    let ok = s.sample.is_none()
        && s.current_of.is_none()
        && s.mark.is_none()
        && !s.with_key
        && !s.sys
        && s.table.partkey.is_none()
        && s.filter.as_ref().is_none_or(|f| !has_sublink(f) && !deep_outer(f));
    ok.then_some(&**s)
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
    if let Plan::SysScan { table, filter } = p {
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
            },
            types,
            false,
        ));
    }
    let Plan::Append {
        inputs,
        inh: inh @ Some(_),
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
            Plan::Project { input, exprs }
                if exprs.len() == w
                    && exprs
                        .iter()
                        .enumerate()
                        .all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i)) =>
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
            types = s.table.columns[..w].iter().map(|c| c.ty.clone()).collect();
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
            Plan::SysScan { table, filter } => {
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
        let cols = match plan {
            Plan::Scan(s) => Some(Arc::new(super::optimize::scan_names(s))),
            _ => None,
        };
        return Some(Plan::Empty {
            ncols: width(plan),
            cols,
        });
    }
    let (corr, rhs, _, scan_limited) = match sub_rel(p) {
        Some(x) => x,
        None => {
            let (corr, rhs) = sub_join(p)?;
            (corr, rhs, Vec::new(), false)
        }
    };
    if scan_limited && !limited {
        return None;
    }
    // Uncorrelated: it stays an InitPlan.
    if corr.is_empty() || corr.iter().any(Expr::is_volatile) {
        return None;
    }
    let nleft = width(plan);
    let cond: Vec<Expr> = corr
        .into_iter()
        .map(|e| outer_to_params(e.shift_cols(nleft as isize), &sub.params))
        .collect::<Option<_>>()?;
    let nright = width(&rhs);
    Some(super::optimize::join(
        cat,
        if anti { JoinKind::Anti } else { JoinKind::Semi },
        plan.clone(),
        rhs,
        and_all(cond),
        Vec::new(),
        nleft,
        nright,
    ))
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
        || sub.left.iter().any(|l| l.columns().is_empty() || l.is_volatile() || has_sublink(l))
    {
        return None;
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
    let (corr, rhs, types, scan_limited) = sub_rel(p)?;
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
            .map(|(i, t)| Expr::Col(i, t.clone()))
            .collect()
    });
    if exprs.len() != sub.left.len() {
        return None;
    }
    let cond: Vec<Expr> = sub
        .left
        .iter()
        .zip(&exprs)
        .zip(&sub.ops)
        .map(|((l, r), op)| {
            Expr::Call(
                op.clone(),
                vec![l.clone(), r.clone().shift_cols(nleft as isize)],
                crate::types::DataType::Bool,
            )
        })
        .chain(corr)
        .collect();
    Some(super::optimize::join(
        cat,
        JoinKind::Semi,
        plan.clone(),
        rhs,
        and_all(cond),
        Vec::new(),
        nleft,
        nright,
    ))
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
fn values_to_any(e: Expr) -> Expr {
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
        if !v.columns().is_empty() || v.is_volatile() || has_sublink(&v) || v.has_outer_refs() {
            return e;
        }
        elems.push(v);
    }
    let elem_ty = elems[0].ty();
    if elems.iter().any(|x| x.ty() != elem_ty) {
        return e;
    }
    let arr_ty = crate::types::DataType::array_of(elem_ty.clone());
    // (Of constants: an array constant.)
    let right = if elems.iter().all(|x| matches!(x, Expr::Const(..))) {
        let items = elems
            .into_iter()
            .map(|x| match x {
                Expr::Const(v, _) => v,
                _ => unreachable!(),
            })
            .collect();
        Expr::Const(
            Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(elem_ty, items))),
            arr_ty,
        )
    } else {
        Expr::Array(elems, arr_ty)
    };
    Expr::ArrayOp {
        left: Box::new(sub.left[0].clone()),
        right: Box::new(right),
        op: sub.ops[0].clone(),
        all: false,
    }
}
