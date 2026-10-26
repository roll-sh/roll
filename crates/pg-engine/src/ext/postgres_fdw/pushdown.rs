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
    if let Plan::LockRows { input, marks, width } = p
        && let Some(level) = remote_lock(cat, input, marks, *width)
    {
        *p = level;
    }
    match p {
        Plan::Update(up) => mark_target(cat, &mut up.source, up.rules.table.oid, true),
        Plan::Delete(dp) => mark_target(cat, &mut dp.source, dp.table.oid, false),
        Plan::Limit { input, limit, offset, with_ties: None } if matches!(&**input, Plan::Sort { .. }) => {
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
                    ..Default::default()
                }));
                *p = if lim {
                    sin
                } else {
                    Plan::Limit { input: Arc::new(sin), limit: limit.clone(), offset: offset.clone(), with_ties: None }
                };
                return;
            }
        }
        // (A LIMIT right over the scan: add_foreign_final_paths without an ordering.)
        Plan::Limit { input, limit, offset, with_ties: None } if !matches!(&**input, Plan::Sort { .. }) => {
            if let Some((_, path)) = order_for(cat, fmt, input, &[]) {
                let mut inp = (**input).clone();
                if let Some(s) = scan_at(&mut inp, &path) {
                    let local = !super::scan::spec(s, None, cat, fmt).local.is_empty();
                    let d = Deparser::new(cat, &s.table, fmt);
                    let shippable = |e: &Option<Expr>| e.as_ref().is_none_or(|x| d.shippable(x));
                    if !local && shippable(limit) && shippable(offset) {
                        s.fdw = Some(Arc::new(FdwPushdown {
                            order: Vec::new(),
                            limit: limit.clone(),
                            offset: offset.clone(),
                            has_limit: true,
                            ..Default::default()
                        }));
                        *p = inp;
                        return;
                    }
                }
            }
        }
        Plan::Sort { input, keys } => {
            if let Some((order, path)) = order_for(cat, fmt, input, keys) {
                let mut inp = (**input).clone();
                if let Some(s) = scan_at(&mut inp, &path) {
                    s.fdw = Some(Arc::new(FdwPushdown { order, limit: None, offset: None, has_limit: false, ..Default::default() }));
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

/// The target scans of an UPDATE / DELETE of a foreign table (`FdwPushdown::target`): the
/// scans of it returning their rows' keys, wherever the source has them (joins included).
/// Their whole rows go up too for an UPDATE, and for a DELETE of a table with row triggers
/// of it (add_row_identity_columns).
fn mark_target(cat: &Catalog, source: &mut Arc<Plan>, oid: u32, update: bool) {
    if !has_foreign(source) {
        return;
    }
    let p = Arc::make_mut(source);
    if let Plan::Scan(s) = p {
        // (An inheritance member too: its scan returns the key as the root's does.)
        if s.table.foreign.is_some() && s.with_key && (s.table.oid == oid || !s.table.inherits.is_empty()) {
            use crate::catalog::trigger_type as tt;
            let wholerow = update
                || cat.triggers.values().any(|tg| tg.table == s.table.oid && tg.tgtype & tt::ROW != 0 && tg.tgtype & tt::DELETE != 0);
            s.fdw = Some(Arc::new(FdwPushdown { target: true, wholerow, ..Default::default() }));
        }
        return;
    }
    for c in crate::sql::explain::plan_inputs_mut(p) {
        mark_target(cat, c, oid, update);
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

/// A level whose locking clause marks only postgres_fdw tables: their row marks are whole-row
/// copies (ROW_MARK_COPY, the wrapper choosing none) and the rows are locked by the remote
/// query's FOR UPDATE / FOR SHARE, so PostgreSQL's plan has no LockRows. The level without
/// it: each locked scan sends the locking clause and returns its columns (no ctid).
fn remote_lock(cat: &Catalog, input: &Arc<Plan>, marks: &[crate::sql::plan::LockMark], width: usize) -> Option<Plan> {
    let remote = |t: &crate::catalog::Table| t.foreign.is_some() && is_postgres_fdw(cat, t);
    if marks.is_empty() || !marks.iter().all(|m| m.lock.is_some() && !m.tables.is_empty() && m.tables.iter().all(|t| remote(t))) {
        return None;
    }
    // The level's columns (the marks' keys come after them).
    let (scan_in, exprs) = match &**input {
        Plan::Project { input, exprs } if exprs.len() >= width => (input.clone(), exprs[..width].to_vec()),
        _ => {
            let types = crate::sql::paths::analyzejoins::col_types(input);
            (input.clone(), (0..width).map(|i| Expr::Col(i, types.get(i).cloned().unwrap_or(crate::types::DataType::Unknown))).collect())
        }
    };
    // (Only scans keyed at the end of their row: dropping the key moves no column.)
    let mut inner = (*scan_in).clone();
    let mut ok = true;
    set_remote_locks(&mut inner, marks, &mut ok);
    if !ok || exprs.iter().any(|e| e.columns().iter().any(|c| *c >= crate::sql::binder::plan_width(&inner))) {
        return None;
    }
    Some(Plan::Project { input: Arc::new(inner), exprs })
}

fn set_remote_locks(p: &mut Plan, marks: &[crate::sql::plan::LockMark], ok: &mut bool) {
    // (A lone scan: dropping its key moves no other relation's columns.)
    let Plan::Scan(s) = p else {
        *ok = false;
        return;
    };
    match marks.iter().find(|m| m.tables.iter().any(|t| t.oid == s.table.oid)) {
        Some(m) if s.with_key && !s.sys && s.fdw.is_none() => {
            s.with_key = false;
            s.mark = m.lock;
            s.fdw = Some(Arc::new(FdwPushdown { copy_mark: true, ..Default::default() }));
        }
        _ => *ok = false,
    }
}
