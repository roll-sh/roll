//! pullup_replace_vars for a MERGE's source: a pulled-up subquery's output computed by a
//! sub-select (`(SELECT count(*) ...) AS cnt`) is copied into each place reading it, the
//! join's clauses and the actions' conditions and new values, each copy its own SubPlan,
//! evaluated there (as Postgres does once the subquery is flattened), not once in the
//! subquery's projection.
//!
//! Only through inner joins and the preserved side of outer ones: on a nullable side the
//! output is a PlaceHolderVar, computed below the join (a copy above it would see the
//! null-extended row).

use crate::sql::expr::Expr;
use crate::sql::plan::{JoinKind, MergeCmd, Plan};
use std::sync::Arc;

/// A MERGE's source and actions with its source subqueries' sub-select outputs copied in.
pub(super) fn copy_into_merge(plan: &mut Plan) {
    let Plan::Merge(m) = plan else { return };
    let defs = defs(Arc::make_mut(&mut m.source));
    if defs.iter().all(Option::is_none) {
        return;
    }
    // (Each action's target list, then its qual: preprocess_expression's order, which the
    // copies' ids, and so their relations' names, follow.)
    for a in &mut m.actions {
        // (A multiple-column SET's sub-select is one, however many columns read it.)
        let mut done: std::collections::HashMap<usize, Expr> = std::collections::HashMap::new();
        let mut one = |e: &Expr| -> Expr {
            let e = e
                .clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Subquery(s) => {
                            let id = s.id;
                            done.entry(id)
                                .or_insert_with(|| nest_copies(subst(&Expr::Subquery(s), &defs)))
                                .clone()
                        }
                        x => x,
                    })
                })
                .unwrap_or_else(|_| e.clone());
            subst(&e, &defs)
        };
        match &mut a.cmd {
            MergeCmd::Update { sets, .. } => {
                for (_, e) in sets.iter_mut() {
                    *e = one(e);
                }
            }
            MergeCmd::Insert { values, .. } => {
                for e in values.iter_mut() {
                    *e = one(e);
                }
            }
            _ => {}
        }
        if let Some(c) = &mut a.cond {
            *c = subst(c, &defs);
        }
    }
    if let Some(c) = &mut m.join_cond {
        *c = subst(c, &defs);
    }
    // (RETURNING reads the source's columns where the joined row has them.)
    for e in &mut m.returning {
        *e = subst(e, &defs);
    }
}

/// `e` with each column a definition is known for replaced by a copy of it, each copy's
/// sub-selects ids of their own (each its own SubPlan; the executor caches by id).
fn subst(e: &Expr, defs: &[Option<Expr>]) -> Expr {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new((1 << 39) + (1 << 38));
    e.clone()
        .transform(&mut |x| {
            Ok(match x {
                Expr::Col(i, t) => match defs.get(i) {
                    Some(Some(d)) => d.clone().transform(&mut |y| {
                        Ok(match y {
                            Expr::Subquery(mut s) => {
                                s.id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                s.plan = deep_copy(&s.plan);
                                // (A MERGE's actions are preprocessed after its quals: no
                                // target list's.)
                                s.tlist = false;
                                Expr::Subquery(s)
                            }
                            y => y,
                        })
                    })?,
                    _ => Expr::Col(i, t),
                },
                x => x,
            })
        })
        .unwrap_or_else(|_| e.clone())
}

/// A copy of a sub-select as pullup_replace_vars makes one for another place reading a
/// pulled-up subquery's output: an id of its own (its own SubPlan; the executor keys its
/// caches by id) and plan nodes of its own (its relations range table entries of their own,
/// named apart).
pub(crate) fn make_copy(s: &mut crate::sql::expr::Subquery) {
    let mut fresh = std::collections::HashMap::new();
    copy_ids(s, &mut fresh);
}

/// `make_copy`, the sub-selects inside the copy copies too (planned again with it, each
/// SubPlan or InitPlan of its own), one new id per old one (`fresh`).
fn copy_ids(s: &mut crate::sql::expr::Subquery, fresh: &mut std::collections::HashMap<usize, usize>) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new((1 << 39) + (1 << 36));
    s.id = *fresh.entry(s.id).or_insert_with(|| {
        // (A MIN / MAX InitPlan the planner made stays one of the planner's.)
        if s.id >= super::PLANNER_SUBQUERY_BASE {
            super::PLANNER_SUBQUERY_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        } else {
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        }
    });
    let mut plan = deep_copy(&s.plan);
    let mut inner = |e: &mut Expr| {
        let mut any = false;
        e.walk(&mut |x| any |= matches!(x, Expr::Subquery(_)));
        if !any {
            return;
        }
        if let Ok(x) = e.clone().transform(&mut |y| {
            Ok(match y {
                Expr::Subquery(mut q) => {
                    copy_ids(&mut q, fresh);
                    Expr::Subquery(q)
                }
                y => y,
            })
        }) {
            *e = x;
        }
    };
    plan_exprs_mut(Arc::make_mut(&mut plan), &mut inner);
    s.plan = plan;
}

/// Every expression of a plan tree (not of its sub-selects' plans), mutably.
fn plan_exprs_mut(p: &mut Plan, f: &mut dyn FnMut(&mut Expr)) {
    use crate::sql::plan::{Access, BitmapQual, ConflictAction, IndexBounds, OnConflict};
    fn bounds(b: &mut IndexBounds, f: &mut dyn FnMut(&mut Expr)) {
        b.eq.iter_mut().for_each(&mut *f);
        if let Some(a) = &mut b.any {
            f(a);
        }
        if let Some((x, _)) = &mut b.lower {
            f(x);
        }
        if let Some((x, _)) = &mut b.upper {
            f(x);
        }
    }
    fn bitmap(q: &mut BitmapQual, f: &mut dyn FnMut(&mut Expr)) {
        match q {
            BitmapQual::Index { quals, pred, bounds: b, .. } => {
                for x in quals {
                    f(&mut x.expr);
                    f(&mut x.orig);
                }
                pred.iter_mut().for_each(&mut *f);
                bounds(b, f);
            }
            BitmapQual::And(v) | BitmapQual::Or(v) => v.iter_mut().for_each(|q| bitmap(q, f)),
        }
    }
    match p {
        Plan::Values { rows, .. } => rows.iter_mut().flatten().for_each(&mut *f),
        Plan::Scan(s) => {
            if let Some(x) = &mut s.filter {
                f(x);
            }
            s.written.iter_mut().for_each(&mut *f);
            s.phv_quals.iter_mut().for_each(&mut *f);
            if let Some(x) = &mut s.current_of {
                f(x);
            }
            if let Some((_, b)) = &mut s.index {
                bounds(b, f);
            }
            match &mut s.access {
                Access::Index { quals, .. } => {
                    for x in quals {
                        f(&mut x.expr);
                        f(&mut x.orig);
                    }
                }
                Access::Bitmap(b, _) => bitmap(Arc::make_mut(b), f),
                Access::Tid { quals, removed } => quals.iter_mut().chain(removed.iter_mut()).for_each(&mut *f),
                Access::TidRange(q) => q.iter_mut().for_each(&mut *f),
                _ => {}
            }
        }
        Plan::SysScan { filter: Some(x), .. } => f(x),
        Plan::FunctionScan { calls, .. } => calls.iter_mut().flat_map(|c| c.args.iter_mut()).for_each(&mut *f),
        Plan::PlCall { args, .. } => args.iter_mut().for_each(&mut *f),
        Plan::Filter { pred, .. } => f(pred),
        Plan::Project { exprs, .. } => exprs.iter_mut().for_each(&mut *f),
        Plan::ProjectSet { exprs, srfs, .. } => {
            exprs.iter_mut().for_each(&mut *f);
            srfs.iter_mut().flat_map(|c| c.args.iter_mut()).for_each(&mut *f);
        }
        Plan::Agg { groups, aggs, .. } => {
            groups.iter_mut().for_each(&mut *f);
            for a in aggs {
                a.args.iter_mut().for_each(&mut *f);
                if let Some(x) = &mut a.filter {
                    f(x);
                }
            }
        }
        Plan::Sort { keys, .. } => keys.iter_mut().for_each(|k| f(&mut k.expr)),
        Plan::Window { calls, .. } => {
            for c in calls {
                c.args.iter_mut().for_each(&mut *f);
                if let crate::sql::plan::WinFunc::Agg(a) = &mut c.func
                    && let Some(x) = &mut a.filter
                {
                    f(x);
                }
                c.partition_by.iter_mut().for_each(&mut *f);
                c.order_by.iter_mut().for_each(|k| f(&mut k.expr));
            }
        }
        Plan::Limit { limit, offset, .. } => limit.iter_mut().chain(offset.iter_mut()).for_each(&mut *f),
        Plan::Join { cond, hash, params, .. } => {
            if let Some(c) = cond {
                f(c);
            }
            if let Some((l, r)) = hash {
                l.iter_mut().chain(r.iter_mut()).for_each(&mut *f);
            }
            params.iter_mut().for_each(&mut *f);
        }
        Plan::Insert(i) => {
            if let Some(OnConflict { action: ConflictAction::Update { sets, filter, .. }, .. }) = &mut i.on_conflict {
                sets.iter_mut().for_each(|s| f(&mut s.1));
                if let Some(x) = filter {
                    f(x);
                }
            }
            i.returning.iter_mut().for_each(&mut *f);
        }
        _ => {}
    }
    for c in crate::sql::explain::plan_inputs_mut(p) {
        plan_exprs_mut(Arc::make_mut(c), f);
    }
}

/// A plan of nodes of its own (a copy's relations are range table entries of their own,
/// named apart).
fn deep_copy(p: &Arc<Plan>) -> Arc<Plan> {
    fn go(a: &mut Arc<Plan>) {
        for c in crate::sql::explain::plan_inputs_mut(Arc::make_mut(a)) {
            go(c);
        }
    }
    let mut n = Arc::new((**p).clone());
    go(&mut n);
    n
}

/// A sub-select of no relation (`(SELECT s.b, s.cnt)`) one of whose values is a copy of a
/// sub-select: the copy goes inside it, where the planner meets it (an InitPlan of that
/// SubPlan, reading the outer row as parameters), not computed for it outside.
fn nest_copies(e: Expr) -> Expr {
    e.clone()
        .transform(&mut |x| {
            let Expr::Subquery(mut s) = x else { return Ok(x) };
            let Plan::Project { input, exprs } = &*s.plan else { return Ok(Expr::Subquery(s)) };
            if !matches!(&**input, Plan::One) {
                return Ok(Expr::Subquery(s));
            }
            let mut exprs = exprs.clone();
            let mut moved = false;
            for k in 0..s.params.len() {
                let Expr::Subquery(c) = &s.params[k] else { continue };
                let mut c = c.clone();
                // Its own parameters, the outer row's values, are the enclosing one's now.
                let mut params = Vec::new();
                for p in std::mem::take(&mut c.params) {
                    let ty = p.ty();
                    s.params.push(p);
                    params.push(Expr::Outer(1, s.params.len() - 1, ty));
                }
                c.params = params;
                let ty = s.params[k].ty();
                let copy = Expr::Subquery(c);
                for e in exprs.iter_mut() {
                    *e = e.clone().transform(&mut |y| {
                        Ok(match y {
                            Expr::Outer(1, i, _) if i == k => copy.clone(),
                            y => y,
                        })
                    })?;
                }
                s.params[k] = Expr::Const(crate::types::Value::Null, ty);
                moved = true;
            }
            if moved {
                s.plan = Arc::new(Plan::Project {
                    input: input.clone(),
                    exprs,
                });
            }
            Ok(Expr::Subquery(s))
        })
        .unwrap_or(e)
}

/// A copyable output: a sub-select (nothing volatile) over columns the same projection
/// returns as they are.
fn copyable(e: &Expr, exprs: &[Expr]) -> Option<Expr> {
    let mut sub = false;
    e.walk(&mut |x| sub |= matches!(x, Expr::Subquery(_)));
    if !sub || super::expr_volatile_deep(e) {
        return None;
    }
    let mut ok = true;
    let mapped = e.clone().remap_cols(&|j| {
        exprs.iter().position(|o| matches!(o, Expr::Col(k, _) if *k == j)).unwrap_or(usize::MAX)
    });
    mapped.walk(&mut |x| ok &= !matches!(x, Expr::Col(usize::MAX, _)));
    ok.then_some(mapped)
}

/// Per output column of `p`: the copyable sub-select it is, over `p`'s own output; the
/// places reading such columns inside `p` get their copies.
fn defs(p: &mut Plan) -> Vec<Option<Expr>> {
    let w = crate::sql::binder::plan_width(p);
    let none = |n: usize| vec![None; n];
    match p {
        Plan::Project { input, exprs } => {
            let inner = defs(Arc::make_mut(input));
            if inner.iter().any(Option::is_some) {
                for e in exprs.iter_mut() {
                    *e = subst(e, &inner);
                }
            }
            let out: Vec<Option<Expr>> = exprs.iter().map(|e| copyable(e, exprs)).collect();
            out
        }
        Plan::Filter { input, pred } => {
            let inner = defs(Arc::make_mut(input));
            if inner.iter().any(Option::is_some) {
                *pred = subst(pred, &inner);
            }
            inner
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
            ..
        } => {
            if !params.is_empty() {
                return none(w);
            }
            let l = defs(Arc::make_mut(left));
            let r = defs(Arc::make_mut(right));
            let (nl, nr) = (*nleft, *nright);
            // (The join's own clauses read a side's copies unless it is the nullable one.)
            let lside = if matches!(kind, JoinKind::Right | JoinKind::Full) { none(nl) } else { l.clone() };
            let rside = if matches!(kind, JoinKind::Inner | JoinKind::Right) { r.clone() } else { none(nr) };
            if lside.iter().chain(&rside).any(Option::is_some) {
                let mut both = lside.clone();
                both.resize(nl, None);
                both.extend(rside.iter().cloned().map(|d| d.map(|e| e.shift_cols(nl as isize))));
                if let Some(c) = cond {
                    *c = subst(c, &both);
                }
                if let Some((lk, rk)) = hash {
                    for e in lk.iter_mut() {
                        *e = subst(e, &lside);
                    }
                    for e in rk.iter_mut() {
                        *e = subst(e, &rside);
                    }
                }
            }
            let shifted = |r: Vec<Option<Expr>>| -> Vec<Option<Expr>> {
                r.into_iter().map(|d| d.map(|e| e.shift_cols(nl as isize))).collect()
            };
            let mut out = match kind {
                JoinKind::Inner => {
                    let mut o = l;
                    o.resize(nl, None);
                    o.extend(shifted(r));
                    o
                }
                JoinKind::Left | JoinKind::Semi | JoinKind::Anti => {
                    let mut o = l;
                    o.resize(nl, None);
                    o
                }
                JoinKind::Right => {
                    let mut o = none(nl);
                    o.extend(shifted(r));
                    o
                }
                JoinKind::Full => none(nl + nr),
            };
            out.resize(w, None);
            out
        }
        _ => none(w),
    }
}
