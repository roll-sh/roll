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
use crate::sql::expr::{Expr, Subquery, SubqueryKind, eval};
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
    /// Planning a sub-select: the constants its parameters stand for (the enclosing level's
    /// expressions it reads as `Outer(1, i)`, a pulled-up subquery's constant output), which
    /// Postgres has put in place of the outer references before it plans it.
    outer: Vec<Option<Expr>>,
}

impl Cx<'_, '_> {
    /// The error folding `e` gives: the one recorded when it was bound (with that place's
    /// CONTEXT), else its own.
    fn fold(&self, e: &Expr) -> Option<Error> {
        let err = eval(e, &[], self.ctx).err()?;
        Some(self.pending.iter().find(|(x, _)| x.same_as(e)).map_or(err, |(_, r)| r.clone()))
    }

    /// eval_const_expressions over `e`; the sub-selects it reads go to `subs`.
    fn expr<'e>(&self, e: &'e Expr, subs: &mut Vec<(&'e Subquery, bool)>) -> Option<Error> {
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
                    subs.push((s, s.kind == SubqueryKind::Exists));
                    return None;
                }
                if foldable(e) {
                    return self.fold(e);
                }
                // (A tree of constants the binder never saw whole: folded whole.)
                if unfolded(e) {
                    return eval(e, &[], self.ctx).err();
                }
                self.fold_outer(e)
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
                let r = self.sub(s, exists);
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
    /// Plan a sub-select, its parameters' constants in place of its outer references.
    fn sub(&self, s: &Subquery, exists: bool) -> Option<Error> {
        let outer = s
            .params
            .iter()
            .map(|p| match p {
                Expr::Const(..) => Some(p.clone()),
                Expr::Outer(1, i, _) => self.outer.get(*i).cloned().flatten(),
                _ => None,
            })
            .collect::<Vec<_>>();
        let cx = Cx { ctx: self.ctx, pending: self.pending, outer };
        crate::stack::grow(|| if exists { cx.exists_plan(&s.plan) } else { cx.plan(&s.plan) })
    }

    /// A call over constants and outer references that stand for constants: folded with
    /// them in place (eval_const_expressions after the pull-up put them there).
    fn fold_outer(&self, e: &Expr) -> Option<Error> {
        if self.outer.iter().all(Option::is_none) || !matches!(e, Expr::Call(..) | Expr::Cast(..)) {
            return None;
        }
        let mut pure = true;
        let mut reads = false;
        e.walk(&mut |x| match x {
            Expr::Outer(1, i, _) if self.outer.get(*i).is_some_and(Option::is_some) => reads = true,
            Expr::Const(..) | Expr::Cast(..) => {}
            Expr::Call(d, ..) => pure &= d.volatility == 'i' && !d.is_srf(),
            _ => pure = false,
        });
        if !pure || !reads || !e.is_immutable() {
            return None;
        }
        let e = e
            .clone()
            .transform(&mut |x| {
                Ok(match x {
                    Expr::Outer(1, i, t) => match self.outer.get(i).cloned().flatten() {
                        Some(c) => c,
                        None => Expr::Outer(1, i, t),
                    },
                    x => x,
                })
            })
            .ok()?;
        eval(&e, &[], self.ctx).err()
    }

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
            crate::stack::grow(|| candidates_go(&s.plan, &vec![read; crate::sql::binder::plan_width(&s.plan)], out));
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

/// An immutable call or cast over constants and such calls only, not all of whose
/// arguments are constants: an expression a pulled-up subquery's constant output was put
/// into after binding, which eval_const_expressions folds whole.
fn unfolded(e: &Expr) -> bool {
    // (Bounded: it runs at every node of an expression; constant trees are small.)
    fn tree(e: &Expr, depth: usize) -> bool {
        depth < 64
            && match e {
                Expr::Const(..) => true,
                Expr::Call(d, args, _) => d.volatility == 'i' && !d.is_srf() && args.iter().all(|a| tree(a, depth + 1)),
                Expr::Cast(a, ..) => tree(a, depth + 1) && e.is_immutable(),
                _ => false,
            }
    }
    match e {
        Expr::Call(_, args, _) => !args.iter().all(Expr::is_const) && tree(e, 0),
        Expr::Cast(a, ..) => !a.is_const() && tree(e, 0),
        _ => false,
    }
}

/// Whether `e` is a tree of immutable calls and casts over constants, in one pass from the
/// leaves; `found` set at an `unfolded` one (or, `subs`, at a sub-select reading a constant
/// for an outer reference), where the scan stops.
fn const_tree(e: &Expr, found: &mut bool, subs: bool) -> bool {
    crate::stack::grow(|| {
        if *found {
            return false;
        }
        match e {
            Expr::Const(..) => true,
            Expr::Call(d, args, _) => {
                let (mut tree, mut consts) = (true, true);
                for a in args {
                    tree &= const_tree(a, found, subs);
                    consts &= a.is_const();
                    if *found && subs {
                        return false;
                    }
                }
                let tree = tree && d.volatility == 'i' && !d.is_srf();
                *found |= tree && !consts;
                tree
            }
            Expr::Cast(a, ..) => {
                // (Only a constant tree below is checked whole: those are small.)
                let tree = const_tree(a, found, subs) && e.is_immutable();
                *found |= tree && !a.is_const();
                tree
            }
            _ => {
                if subs
                    && let Expr::Subquery(s) = e
                    && (s.params.iter().any(Expr::is_const) || const_params(&s.plan))
                {
                    *found = true;
                    return false;
                }
                for c in e.children() {
                    const_tree(c, found, subs);
                    if *found && subs {
                        return false;
                    }
                }
                false
            }
        }
    })
}

/// Whether planning `p` may fold what binding couldn't: a sub-select reading a constant its
/// level put in place of an outer reference (`Cx::fold_outer`), or a tree of constants put
/// together after binding (`unfolded`).
pub(crate) fn const_params(p: &Plan) -> bool {
    crate::stack::grow(|| {
        let mut found = false;
        for e in exprs(p) {
            const_tree(e, &mut found, true);
            if found {
                return true;
            }
        }
        crate::sql::explain::plan_inputs(p).into_iter().any(|c| const_params(c))
    })
}

/// The error a planned statement fails with: the first constant expression still in its
/// plan that fails to fold, its error as recorded in `pending` when it was bound.
pub(crate) fn first_error(plan: &Plan, pending: &[(Expr, Error)], ctx: &ExecCtx) -> Option<Error> {
    Cx { ctx, pending, outer: Vec::new() }.plan(plan)
}

/// Whether simplifying `e` fails (an inlined function's body and a function call run while
/// planning, `pending`, included).
pub(crate) fn fails(e: &Expr, ctx: &ExecCtx, pending: &[(Expr, Error)]) -> bool {
    let cx = Cx { ctx, pending, outer: Vec::new() };
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
            .filter(|(s, _)| calls.contains(&Arc::as_ptr(&s.plan)))
            .any(|(s, exists)| cx.sub(s, exists).is_some())
}
