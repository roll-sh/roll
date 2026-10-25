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
                for a in args {
                    if let Some(x) = self.expr(a, subs) {
                        return Some(x);
                    }
                    if matches!(a, Expr::Const(Value::Bool(b), _) if *b != and) {
                        break;
                    }
                }
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
    fn go(p: &Plan, out: &mut Vec<Expr>) {
        if let Plan::Empty { info: Some(i), .. } = p {
            out.extend(i.kept.iter().cloned());
        }
        for e in exprs(p) {
            e.walk(&mut |x| {
                if foldable(x) && !out.iter().any(|o| o.same_as(x)) {
                    out.push(x.clone());
                }
            });
        }
        if let Plan::With { ctes, .. } = p {
            for (_, c) in ctes {
                go(c, out);
            }
        }
        for c in crate::sql::explain::plan_inputs(p) {
            go(c, out);
        }
    }
    let mut out = Vec::new();
    go(p, &mut out);
    out
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

/// Whether simplifying `e` fails (an inlined function's body, a sub-select planned with
/// it, included).
pub(crate) fn fails(e: &Expr, ctx: &ExecCtx) -> bool {
    let cx = Cx { ctx, pending: &[] };
    let mut subs = Vec::new();
    cx.expr(e, &mut subs).is_some()
        || subs.into_iter().any(|(p, exists)| if exists { cx.exists_plan(p) } else { cx.plan(p) }.is_some())
}
