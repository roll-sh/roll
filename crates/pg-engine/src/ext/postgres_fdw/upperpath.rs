//! Which aggregations over a foreign scan or join run remotely (add_foreign_grouping_paths,
//! foreign_grouping_ok), as a rewrite of the planned query level: `[Project] [Filter
//! (HAVING)] Agg` over the relation becomes a projection over a Foreign Scan of the
//! grouped relation.

use super::join::Base;
use super::joinscan::JoinScan;
use super::upper::UpperScan;
use crate::catalog::Catalog;
use crate::sql::expr::Expr;
use crate::sql::plan::{AggCall, Plan};
use crate::types::text::FormatCtx;
use std::sync::Arc;

/// `e` with column `i` replaced by `cols[i]`.
fn through(e: &Expr, cols: &[Expr]) -> Option<Expr> {
    e.clone()
        .transform(&mut |x| match x {
            Expr::Col(i, t) => Ok(cols.get(i).cloned().unwrap_or(Expr::Col(usize::MAX, t))),
            x => Ok(x),
        })
        .ok()
        .filter(|r| !r.columns().contains(&usize::MAX))
}

/// The scan or join relation under `p`'s projections, with `p`'s columns over its row.
fn rel_below(cat: &Catalog, fmt: &FormatCtx, p: &Plan) -> Option<(JoinScan, Vec<Expr>)> {
    match p {
        Plan::Foreign(f) => {
            let js = f.private.0.as_any().downcast_ref::<JoinScan>()?;
            if !js.order.is_empty() || js.limit.is_some() || !js.info.local_conds.is_empty() {
                return None;
            }
            let types = crate::sql::paths::analyzejoins::col_types(p);
            let cols = f.out.iter().enumerate().map(|(k, &g)| Expr::Col(g, types.get(k).cloned().unwrap_or(crate::types::DataType::Unknown))).collect();
            Some((js.clone(), cols))
        }
        Plan::Project { input, exprs } => {
            let (js, cols) = rel_below(cat, fmt, input)?;
            let mapped = exprs.iter().map(|e| through(e, &cols)).collect::<Option<Vec<_>>>()?;
            Some((js, mapped))
        }
        // (A local join of a relation the wrapper could run remotely: its Foreign Scan,
        // which aggregating over may make the cheaper plan.)
        Plan::Join { meta, .. } if meta.fdw_alt.0.is_some() => rel_below(cat, fmt, meta.fdw_alt.0.as_ref()?),
        Plan::Scan(s) => {
            if s.table.foreign.is_none()
                || !super::scan::is_postgres_fdw(cat, &s.table)
                || s.fdw.is_some()
                || s.mark.is_some()
                || s.with_key
                || s.sys
                || s.current_of.is_some()
            {
                return None;
            }
            let spec = super::scan::spec(s, None, cat, fmt);
            if !spec.local.is_empty() {
                return None;
            }
            let n = crate::sql::binder::plan_width(p);
            let base = Base::at((**s).clone(), 0, n, 0.0);
            let info = super::join::plain_base(cat, s, spec.remote);
            let types = crate::sql::paths::analyzejoins::col_types(p);
            let cols = (0..n).map(|i| Expr::Col(i, types.get(i).cloned().unwrap_or(crate::types::DataType::Unknown))).collect();
            Some((JoinScan { info: Arc::new(info), bases: vec![base], tlist: Vec::new(), order: Vec::new(), limit: None }, cols))
        }
        _ => None,
    }
}

/// The aggregate references in `e`.
fn agg_refs(e: &Expr) -> Vec<usize> {
    let mut v = Vec::new();
    e.walk(&mut |x| {
        if let Expr::Agg(i, _) = x
            && !v.contains(i)
        {
            v.push(*i);
        }
    });
    v
}

/// `p` (a projection, a HAVING filter, an Agg, over a foreign relation) as a projection
/// over a Foreign Scan of the grouped relation, when the aggregation can run remotely.
pub fn grouped(cat: &Catalog, fmt: &FormatCtx, p: &Plan) -> Option<Plan> {
    let (proj, below) = match p {
        Plan::Project { input, exprs } => (Some(exprs), &**input),
        p => (None, p),
    };
    let (having, below) = match below {
        Plan::Filter { input, pred } => (Some(pred), &**input),
        p => (None, p),
    };
    let Plan::Agg { input, groups, sets: None, aggs } = below else { return None };
    let (js, cols) = rel_below(cat, fmt, input)?;
    let ng = groups.len();
    let g: Vec<Expr> = groups.iter().map(|e| through(e, &cols)).collect::<Option<_>>()?;
    let mut a: Vec<AggCall> = Vec::new();
    for x in aggs {
        let mut y = x.clone();
        y.args = x.args.iter().map(|e| through(e, &cols)).collect::<Option<_>>()?;
        // (Direct arguments are over the group's keys.)
        y.direct = x.direct.iter().map(|e| through(e, &g)).collect::<Option<_>>()?;
        y.filter = match &x.filter {
            Some(f) => Some(through(f, &cols)?),
            None => None,
        };
        for k in &mut y.order_by {
            k.expr = through(&k.expr, &cols)?;
        }
        a.push(y);
    }
    let mut q = super::joinsql::Sql::new(cat, fmt, &js.bases);
    q.aggs = &a;
    let ship = |e: &Expr| q.shippable(&js.info, e);
    // An expression over the Agg's row as one over the join row.
    let sub = |e: &Expr| {
        e.clone()
            .transform(&mut |x| match x {
                Expr::Col(k, _) if k < ng => Ok(g[k].clone()),
                Expr::Col(k, t) => Ok(Expr::Agg(k - ng, t)),
                x => Ok(x),
            })
            .ok()
    };
    let identity: Vec<Expr>;
    let pexprs: &Vec<Expr> = match proj {
        Some(e) => e,
        None => {
            let types = crate::sql::paths::analyzejoins::col_types(below);
            identity = (0..ng + a.len()).map(|k| Expr::Col(k, types.get(k).cloned().unwrap_or(crate::types::DataType::Unknown))).collect();
            &identity
        }
    };
    let mut tlist: Vec<Expr> = Vec::new();
    let mut gpos: Vec<Option<usize>> = vec![None; ng];
    let add = |t: &mut Vec<Expr>, e: Expr| {
        if !t.iter().any(|x| x.same_as(&e)) {
            t.push(e);
        }
    };
    for e in pexprs {
        match e {
            Expr::Col(k, _) if *k < ng => {
                if !ship(&g[*k]) {
                    return None;
                }
                tlist.push(g[*k].clone());
                gpos[*k].get_or_insert(tlist.len());
            }
            e => {
                let se = sub(e)?;
                if ship(&se) {
                    add(&mut tlist, se);
                } else {
                    for i in agg_refs(&se) {
                        let ae = Expr::Agg(i, a[i].ty.clone());
                        if !ship(&ae) {
                            return None;
                        }
                        add(&mut tlist, ae);
                    }
                }
            }
        }
    }
    for k in 0..ng {
        if gpos[k].is_none() {
            if !ship(&g[k]) {
                return None;
            }
            tlist.push(g[k].clone());
            gpos[k] = Some(tlist.len());
        }
    }
    finish(cat, fmt, &js, a, tlist, gpos.into_iter().flatten().collect(), having, pexprs, &sub)
}

/// The HAVING clauses split into sent and checked here, the projection over the grouped
/// relation's row, and the plan.
#[allow(clippy::too_many_arguments)]
fn finish(
    cat: &Catalog,
    fmt: &FormatCtx,
    js: &JoinScan,
    a: Vec<AggCall>,
    mut tlist: Vec<Expr>,
    group_by: Vec<usize>,
    having: Option<&Expr>,
    pexprs: &[Expr],
    sub: &dyn Fn(&Expr) -> Option<Expr>,
) -> Option<Plan> {
    let mut q = super::joinsql::Sql::new(cat, fmt, &js.bases);
    q.aggs = &a;
    let ship = |e: &Expr| q.shippable(&js.info, e);
    let mut remote = Vec::new();
    let mut local_src = Vec::new();
    if let Some(h) = having {
        let mut parts = Vec::new();
        conjuncts(h, &mut parts);
        for c in parts {
            let sc = sub(&c)?;
            if ship(&sc) {
                remote.push(sc);
            } else {
                for i in agg_refs(&sc) {
                    let ae = Expr::Agg(i, a[i].ty.clone());
                    if !ship(&ae) {
                        return None;
                    }
                    if !tlist.iter().any(|x| x.same_as(&ae)) {
                        tlist.push(ae);
                    }
                }
                local_src.push(c);
            }
        }
    }
    let exprs = pexprs.iter().map(|e| to_t(e, &tlist, sub)).collect::<Option<Vec<_>>>()?;
    let local = local_src.iter().map(|e| to_t(e, &tlist, sub)).collect::<Option<Vec<_>>>()?;
    let scans = js.bases.iter().map(|b| Arc::new(Plan::Scan(Box::new(b.scan.clone())))).collect();
    let cols = js.bases.iter().map(|b| b.cols.clone()).collect();
    let n = tlist.len();
    let up = UpperScan { input: js.clone(), aggs: a, tlist, group_by, having: remote, local, order: Vec::new(), limit: None };
    let fr = crate::sql::plan::ForeignRel {
        scans,
        cols,
        out: (0..n).collect(),
        private: crate::sql::plan::FdwPrivate(Arc::new(up)),
        est: (js.info.rows, js.info.startup, js.info.total, js.info.width),
    };
    Some(Plan::Project { input: Arc::new(Plan::Foreign(Box::new(fr))), exprs })
}

/// An expression over the Agg's row as one over the grouped relation's (its target
/// list's entries).
fn to_t(e: &Expr, tlist: &[Expr], sub: &dyn Fn(&Expr) -> Option<Expr>) -> Option<Expr> {
    let se = sub(e)?;
    if let Some(j) = tlist.iter().position(|x| x.same_as(&se)) {
        return Some(Expr::Col(j, se.ty()));
    }
    if let Expr::Col(..) = e {
        return None;
    }
    let mut ok = true;
    let r = e.clone().map_children(&mut |c| match to_t(&c, tlist, sub) {
        Some(x) => Ok(x),
        None => {
            ok = false;
            Ok(c)
        }
    });
    r.ok().filter(|_| ok)
}

fn conjuncts(e: &Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::And(xs) => xs.iter().for_each(|x| conjuncts(x, out)),
        e => out.push(e.clone()),
    }
}

/// Rewrite the query level's aggregations over foreign relations that can run remotely.
pub fn push_grouped(cat: &Catalog, fmt: &FormatCtx, p: &mut Plan) {
    if matches!(p, Plan::Project { .. } | Plan::Filter { .. } | Plan::Agg { .. })
        && let Some(np) = grouped(cat, fmt, p)
    {
        *p = np;
        return;
    }
    // (A Foreign Scan's relations are its own.)
    if let Plan::Foreign(_) = p {
        return;
    }
    for c in crate::sql::explain::plan_inputs_mut(p) {
        push_grouped(cat, fmt, Arc::make_mut(c));
    }
}
