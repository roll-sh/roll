//! The errors constant folding raises, as Postgres raises them.
//!
//! The binder folds constant expressions as it builds them, but Postgres folds in
//! eval_const_expressions while it plans a query level: after pulling subqueries up (whose
//! unused outputs then never appear), after replacing the unused outputs of a subquery it
//! plans apart with NULLs (remove_unused_subquery_outputs), and never for a CTE nothing
//! reads. A constant expression that fails to fold stays unfolded (`Binder::fold_failed`);
//! once the statement is planned, the first one Postgres would still have simplified fails
//! it (`first_error`), in eval_const_expressions' order: left to right, an AND / OR stopping
//! at the constant that decides it, a CASE skipping the arms whose condition is a false or
//! NULL constant and everything after a true one, a COALESCE stopping at its first non-NULL
//! constant; a level's CTEs first (SS_process_ctes), each expression's sub-selects after it
//! (SS_process_sublinks), the target list before the quals.

use std::sync::Arc;

use crate::error::Error;
use crate::exec::ExecCtx;
use crate::sql::expr::{Expr, SubqueryKind, eval};
use crate::sql::plan::Plan;
use crate::types::Value;

/// An expression the planner evaluates now: an immutable function over constants (a strict
/// one with a NULL among them is NULL without a call), an immutable cast of a constant, a
/// NULLIF of constants.
fn foldable(e: &Expr) -> bool {
    match e {
        Expr::Call(def, args, _) => {
            def.volatility == 'i'
                && !def.is_srf()
                && args.iter().all(Expr::is_const)
                && !(def.strict && args.iter().any(|a| matches!(a, Expr::Const(v, _) if v.is_null())))
        }
        Expr::Cast(a, ..) => a.is_const() && e.is_immutable(),
        Expr::NullIf(a, b, d, _) => a.is_const() && b.is_const() && d.volatility == 'i',
        _ => false,
    }
}

struct Cx<'a, 'c> {
    ctx: &'a ExecCtx<'c>,
    pending: &'a [(Expr, Error)],
}

impl Cx<'_, '_> {
    /// The error folding `e` gives: the one recorded when it was bound (with that place's
    /// CONTEXT), else its own.
    fn fold(&self, e: &Expr) -> Option<Error> {
        let err = eval(e, &[], self.ctx).err()?;
        Some(self.pending.iter().find(|(x, _)| x.same_as(e)).map_or(err, |(_, r)| r.clone()))
    }

    /// eval_const_expressions over `e`; the sub-selects it reads go to `subs`.
    fn expr<'e>(&self, e: &'e Expr, subs: &mut Vec<(&'e Arc<Plan>, bool)>) -> Option<Error> {
        match e {
            Expr::Const(..) => None,
            Expr::And(args) | Expr::Or(args) => {
                let and = matches!(e, Expr::And(_));
                // (A constant deciding the clause replaces it: the sub-selects of the
                // arguments before it are never planned.)
                let mut mine = Vec::new();
                for a in args {
                    if let Some(x) = self.expr(a, &mut mine) {
                        return Some(x);
                    }
                    if matches!(a, Expr::Const(Value::Bool(b), _) if *b != and) {
                        return None;
                    }
                }
                subs.extend(mine);
                None
            }
            Expr::Case { operand, whens, else_, .. } => {
                if let Some(o) = operand
                    && let Some(x) = self.expr(o, subs)
                {
                    return Some(x);
                }
                for (c, r) in whens {
                    if let Some(x) = self.expr(c, subs) {
                        return Some(x);
                    }
                    match c {
                        Expr::Const(Value::Bool(false) | Value::Null, _) => continue,
                        Expr::Const(Value::Bool(true), _) => return self.expr(r, subs),
                        _ => {
                            if let Some(x) = self.expr(r, subs) {
                                return Some(x);
                            }
                        }
                    }
                }
                else_.as_deref().and_then(|x| self.expr(x, subs))
            }
            Expr::Coalesce(args, _) => {
                for a in args {
                    if let Some(x) = self.expr(a, subs) {
                        return Some(x);
                    }
                    if matches!(a, Expr::Const(v, _) if !v.is_null()) {
                        break;
                    }
                }
                None
            }
            _ => {
                for c in e.children() {
                    if let Some(x) = self.expr(c, subs) {
                        return Some(x);
                    }
                }
                // (A function call run while planning (evaluate_function) that failed when
                // the binder ran it.)
                if let Expr::Subquery(s) = e
                    && s.call.is_some()
                    && let Some((_, err)) = self.pending.iter().find(|(x, _)| x.same_as(e))
                {
                    return Some(err.clone());
                }
                // (A SQL function's body Postgres neither inlines nor runs while planning is
                // planned when the call runs: what fails in it fails only then.)
                if let Expr::Subquery(s) = e
                    && s.call.as_ref().is_some_and(|c| !c.inline)
                {
                    return None;
                }
                if let Expr::Subquery(s) = e {
                    subs.push((&s.plan, s.kind == SubqueryKind::Exists));
                    return None;
                }
                if foldable(e) { self.fold(e) } else { None }
            }
        }
    }

    /// Simplify `e` and plan the sub-selects it reads.
    fn exprs_of(&self, es: &[&Expr]) -> Option<Error> {
        for e in es {
            let mut subs = Vec::new();
            if let Some(x) = self.expr(e, &mut subs) {
                return Some(x);
            }
            for (s, exists) in subs {
                let r = if exists { self.exists_plan(s) } else { self.plan(s) };
                if r.is_some() {
                    return r;
                }
            }
        }
        None
    }

    /// A query level, in subquery_planner's order: its CTEs (SS_process_ctes), its target
    /// list, then its quals (preprocess_qual_conditions: a join's sides before its own
    /// clauses, a pulled-up subquery's WHERE before the WHERE above it), then LIMIT /
    /// OFFSET.
    fn plan(&self, p: &Plan) -> Option<Error> {
        if let Plan::With { ctes, body, .. } = p {
            // (Those nothing references aren't in the plan: `define_ctes`.)
            for (_, c) in ctes {
                if let Some(x) = self.plan(c) {
                    return Some(x);
                }
            }
            return self.plan(body);
        }
        // (A writing statement: its own target list, then the query it reads.)
        if matches!(p, Plan::Insert(_) | Plan::Update(_) | Plan::Delete(_) | Plan::Merge(_)) {
            return self.exprs_of(&exprs(p)).or_else(|| crate::sql::explain::plan_inputs(p).into_iter().find_map(|c| self.plan(c)));
        }
        let mut top = p;
        let mut later: Vec<&Expr> = Vec::new();
        loop {
            match top {
                Plan::Limit { input, .. } | Plan::Sort { input, .. } | Plan::LockRows { input, .. } | Plan::Distinct { input, .. } => {
                    later.extend(exprs(top));
                    top = input;
                }
                _ => break,
            }
        }
        let r = match top {
            Plan::Project { input, .. } => self.exprs_of(&exprs(top)).or_else(|| self.quals(input)),
            _ => self.quals(top),
        };
        r.or_else(|| self.exprs_of(&later))
    }

    /// The rest of a level, inputs before the node's own expressions.
    fn quals(&self, p: &Plan) -> Option<Error> {
        if matches!(p, Plan::With { .. } | Plan::Insert(_) | Plan::Update(_) | Plan::Delete(_) | Plan::Merge(_)) {
            return self.plan(p);
        }
        crate::sql::explain::plan_inputs(p)
            .into_iter()
            .find_map(|c| self.quals(c))
            .or_else(|| self.exprs_of(&exprs(p)))
    }
}

impl Cx<'_, '_> {
    /// An EXISTS sub-select: simplify_EXISTS_query throws its target list (and ORDER BY,
    /// DISTINCT) away before planning it, unless it aggregates, has windows, a set
    /// operation, set-returning calls or an OFFSET.
    fn exists_plan(&self, p: &Plan) -> Option<Error> {
        if !exists_simplified(p) {
            return self.plan(p);
        }
        let mut cur = p;
        loop {
            match cur {
                Plan::Project { input, .. } | Plan::Sort { input, .. } | Plan::Distinct { input, .. } => cur = input,
                // (Its GROUP BY goes too.)
                Plan::Agg { input, aggs, sets: None, .. } if aggs.is_empty() => cur = input,
                Plan::Limit { input, limit, .. } => {
                    if let Some(x) = limit.as_ref().and_then(|l| self.expr(l, &mut Vec::new())) {
                        return Some(x);
                    }
                    cur = input;
                }
                _ => return self.quals(cur),
            }
        }
    }
}

/// Whether simplify_EXISTS_query applies to an EXISTS sub-select's level: no aggregation,
/// window, set-returning call, set operation or OFFSET (its target list then goes).
pub(crate) fn exists_simplified(p: &Plan) -> bool {
    match p {
        // (A HAVING.)
        Plan::Filter { input, .. } if matches!(&**input, Plan::Agg { .. }) => false,
        // (A GROUP BY without aggregates or grouping sets: still simplified.)
        Plan::Agg { input, aggs, sets: None, .. } if aggs.is_empty() => exists_simplified(input),
        Plan::Project { input, .. } | Plan::Sort { input, .. } | Plan::Distinct { input, .. } | Plan::Filter { input, .. } => exists_simplified(input),
        Plan::Limit { input, offset: None, .. } => exists_simplified(input),
        Plan::Limit { .. } | Plan::LockRows { .. } | Plan::Agg { .. } | Plan::Window { .. } | Plan::ProjectSet { .. } | Plan::SetOp { .. } => false,
        _ => true,
    }
}

/// The constant expressions left unfolded in a plan about to be dropped (an empty
/// relation, degenerate grouping): those that failed to fold, with what dropped plans
/// already kept.
pub(crate) fn candidates(p: &Plan) -> Vec<Expr> {
    let mut out = Vec::new();
    candidates_go(p, &[], &mut out);
    out
}

/// Per output column of a plan about to be dropped, the `candidates` that only a reader of
/// that column adds to `base` (`DummyInfo::kept_cols`).
pub(crate) fn output_candidates(p: &Plan, base: &[Expr]) -> Vec<(usize, Expr)> {
    let w = crate::sql::binder::plan_width(p);
    let new = |c: Vec<Expr>| -> Vec<Expr> { c.into_iter().filter(|e| !base.iter().any(|b| b.same_as(e))).collect() };
    let mut all = Vec::new();
    candidates_go(p, &vec![true; w], &mut all);
    if new(all).is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for k in 0..w {
        let mut need = vec![false; w];
        need[k] = true;
        let mut c = Vec::new();
        candidates_go(p, &need, &mut c);
        out.extend(new(c).into_iter().map(|e| (k, e)));
    }
    out
}

// (Only what Postgres would have simplified: quals, and the outputs something in the
// dropped plan reads; an output nothing reads was a subquery's unused one.)
fn candidates_go(p: &Plan, needed: &[bool], out: &mut Vec<Expr>) {
    let push = |e: &Expr, out: &mut Vec<Expr>| push_candidates(e, out);
    match p {
        Plan::Empty { info: Some(i), .. } => {
            out.extend(i.kept.iter().cloned());
            for (k, e) in &i.kept_cols {
                if needed.get(*k).copied().unwrap_or(false) && !out.iter().any(|o| o.same_as(e)) {
                    out.push(e.clone());
                }
            }
        }
        Plan::Project { exprs, .. } => {
            for (k, e) in exprs.iter().enumerate() {
                if needed.get(k).copied().unwrap_or(false) {
                    push(e, out);
                }
            }
        }
        _ => {
            for e in exprs(p) {
                push(e, out);
            }
        }
    }
    let needs = crate::sql::paths::child_needs(p, needed);
    for (k, c) in crate::sql::explain::plan_inputs(p).into_iter().enumerate() {
        let n = needs.get(k).cloned().unwrap_or_else(|| vec![true; crate::sql::binder::plan_width(c)]);
        candidates_go(c, &n, out);
    }
}

/// The constants in `e` that failed to fold, and those of the sub-selects it reads:
/// preprocess_expression plans a sub-select (make_subplan) as it simplifies the expression,
/// whatever later becomes of the relation reading it (an outer join's side found empty).
fn push_candidates(e: &Expr, out: &mut Vec<Expr>) {
    e.walk(&mut |x| {
        if foldable(x) && !out.iter().any(|o| o.same_as(x)) {
            out.push(x.clone());
        }
        // (A SQL function's body Postgres neither inlines nor runs while planning is planned
        // when it runs.)
        if let Expr::Subquery(s) = x
            && !s.call.as_ref().is_some_and(|c| !c.inline)
        {
            let read = s.kind != SubqueryKind::Exists || !exists_simplified(&s.plan);
            candidates_go(&s.plan, &vec![read; crate::sql::binder::plan_width(&s.plan)], out);
        }
    });
}

/// `candidates` of some expressions.
pub(crate) fn candidates_in(es: &[Expr]) -> Vec<Expr> {
    let mut out: Vec<Expr> = Vec::new();
    for e in es {
        e.walk(&mut |x| {
            if foldable(x) && !out.iter().any(|o| o.same_as(x)) {
                out.push(x.clone());
            }
        });
    }
    out
}

/// The expressions of a plan node the planner simplifies.
fn exprs(p: &Plan) -> Vec<&Expr> {
    let mut out = Vec::new();
    crate::sql::explain::node_exprs(p, &mut out);
    if let Plan::Empty { info: Some(i), .. } = p {
        out.extend(&i.kept);
    }
    // (The defaults of the columns an INSERT doesn't name: in its target list.)
    if let Plan::Insert(ins) = p
        && ins.copy.is_none()
    {
        for (i, d) in ins.rules.defaults.iter().enumerate() {
            if let Some(d) = d
                && !ins.target.contains(&i)
            {
                out.push(d);
            }
        }
    }
    if let Plan::Agg { aggs, .. } = p {
        for a in aggs {
            out.extend(&a.direct);
            out.extend(a.order_by.iter().map(|k| &k.expr));
        }
    }
    out
}

/// The error a planned statement fails with: the first constant expression still in its
/// plan that fails to fold, its error as recorded in `pending` when it was bound.
pub(crate) fn first_error(plan: &Plan, pending: &[(Expr, Error)], ctx: &ExecCtx) -> Option<Error> {
    Cx { ctx, pending }.plan(plan)
}

/// Whether simplifying `e` fails (an inlined function's body and a function call run while
/// planning, `pending`, included).
pub(crate) fn fails(e: &Expr, ctx: &ExecCtx, pending: &[(Expr, Error)]) -> bool {
    let cx = Cx { ctx, pending };
    let mut subs = Vec::new();
    // (A sub-select is planned after the expression is simplified (SS_process_sublinks):
    // what fails in it doesn't stop the AND / OR around it; a SQL function's body inlined
    // into it does.)
    let mut calls: Vec<*const Plan> = Vec::new();
    e.walk(&mut |x| {
        if let Expr::Subquery(s) = x
            && s.call.is_some()
        {
            calls.push(Arc::as_ptr(&s.plan));
        }
    });
    cx.expr(e, &mut subs).is_some()
        || subs
            .into_iter()
            .filter(|(p, _)| calls.contains(&Arc::as_ptr(p)))
            .any(|(p, exists)| if exists { cx.exists_plan(p) } else { cx.plan(p) }.is_some())
}
