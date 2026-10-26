//! What postgres_fdw sends of a query level besides a scan's conditions: the ORDER BY of a
//! sort right above a foreign scan and the LIMIT above that (add_foreign_ordered_paths,
//! add_foreign_final_paths, and the presorted paths merge joins take).

use super::deparse::Deparser;
use super::scan::is_postgres_fdw;
use crate::catalog::Catalog;
use crate::sql::expr::Expr;
use crate::sql::plan::{FdwPushdown, Plan, ScanPlan, SortKey};
use std::sync::Arc;

/// Sort keys of the plan above `p` as expressions over the scan below its projections.
fn through_projects(p: &Plan, keys: &[SortKey]) -> Option<(Vec<SortKey>, ScanPath)> {
    match p {
        Plan::Scan(_) => Some((keys.to_vec(), ScanPath::Here)),
        Plan::Project { input, exprs } => {
            let mapped: Vec<SortKey> = keys
                .iter()
                .map(|k| {
                    let e = k.expr.clone().transform(&mut |x| match x {
                        Expr::Col(i, _) => Ok(exprs.get(i).cloned().unwrap_or(x)),
                        x => Ok(x),
                    });
                    e.ok().map(|expr| SortKey { expr, ..k.clone() })
                })
                .collect::<Option<_>>()?;
            let (k, path) = through_projects(input, &mapped)?;
            Some((k, ScanPath::Below(Box::new(path))))
        }
        _ => None,
    }
}

/// Where the scan is under a chain of projections.
enum ScanPath {
    Here,
    Below(Box<ScanPath>),
}

fn scan_at<'a>(p: &'a mut Plan, path: &ScanPath) -> Option<&'a mut ScanPlan> {
    match (p, path) {
        (Plan::Scan(s), ScanPath::Here) => Some(s),
        (Plan::Project { input, .. }, ScanPath::Below(rest)) => scan_at(Arc::make_mut(input), rest),
        _ => None,
    }
}

/// The ORDER BY a sort over `input` would send, when every key can go.
fn order_for(cat: &Catalog, fmt: &crate::types::text::FormatCtx, input: &Plan, keys: &[SortKey]) -> Option<(Vec<SortKey>, ScanPath)> {
    let (mapped, path) = through_projects(input, keys)?;
    let mut probe = input;
    while let Plan::Project { input, .. } = probe {
        probe = input;
    }
    let Plan::Scan(s) = probe else { return None };
    if s.table.foreign.is_none() || !is_postgres_fdw(cat, &s.table) || s.fdw.is_some() || s.mark.is_some() || s.with_key {
        return None;
    }
    let d = Deparser::new(cat, &s.table, fmt);
    let ok = mapped.iter().all(|k| k.using.is_none() && d.shippable(&k.expr) && k.expr.columns().iter().all(|c| *c < s.table.columns.len()));
    ok.then_some((mapped, path))
}

/// Rewrite the plan: sorts over foreign scans become remote ORDER BYs, a LIMIT over them a
/// remote LIMIT when nothing is checked locally.
pub fn push(cat: &Catalog, fmt: &crate::types::text::FormatCtx, p: &mut Plan) {
    // (Children first: a sort under a merge join reads a presorted remote scan too.)
    match p {
        Plan::Limit { input, limit, offset, with_ties: None } => {
            if let Plan::Sort { input: sin, keys } = &**input
                && let Some((order, path)) = order_for(cat, fmt, sin, keys)
            {
                let mut sin = (**sin).clone();
                let Some(s) = scan_at(&mut sin, &path) else { return };
                let local = !super::scan::spec(s, None, cat, fmt).local.is_empty();
                let d = Deparser::new(cat, &s.table, fmt);
                let shippable = |e: &Option<Expr>| e.as_ref().is_none_or(|x| d.shippable(x));
                let lim = !local && shippable(limit) && shippable(offset);
                s.fdw = Some(Arc::new(FdwPushdown {
                    order,
                    limit: if lim { limit.clone() } else { None },
                    offset: if lim { offset.clone() } else { None },
                    has_limit: lim,
                }));
                *p = if lim {
                    sin
                } else {
                    Plan::Limit { input: Arc::new(sin), limit: limit.clone(), offset: offset.clone(), with_ties: None }
                };
                return;
            }
        }
        Plan::Sort { input, keys } => {
            if let Some((order, path)) = order_for(cat, fmt, input, keys) {
                let mut inp = (**input).clone();
                if let Some(s) = scan_at(&mut inp, &path) {
                    s.fdw = Some(Arc::new(FdwPushdown { order, limit: None, offset: None, has_limit: false }));
                    *p = inp;
                    return;
                }
            }
        }
        _ => {}
    }
    for c in crate::sql::explain::plan_inputs_mut(p) {
        if has_foreign(c) {
            push(cat, fmt, Arc::make_mut(c));
        }
    }
}

fn has_foreign(p: &Plan) -> bool {
    match p {
        Plan::Scan(s) => s.table.foreign.is_some(),
        p => crate::sql::explain::plan_inputs(p).into_iter().any(|c| has_foreign(c)),
    }
}

/// `push` over a query level just planned (only when a foreign server exists at all).
pub fn push_level(ctx: &crate::exec::ExecCtx, p: &mut Plan) {
    let cat = ctx.catalog();
    if cat.servers.is_empty() || !has_foreign(p) {
        return;
    }
    push(&cat, ctx.fmt(), p);
}
