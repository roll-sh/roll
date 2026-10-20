//! remove_unused_subquery_outputs: the output columns of a subquery (or view) that nothing
//! above reads aren't computed, so an error one would raise (`1/0`) doesn't happen.
//! Volatile and set-returning ones still run, as do those a sort, grouping, DISTINCT or set
//! operation uses.

use super::*;

/// contain_volatile_functions: a sub-select counts by what it runs, not as volatile itself.
pub(super) fn volatile(e: &Expr) -> bool {
    let mut v = false;
    e.walk(&mut |x| match x {
        Expr::Call(d, ..) if d.volatility == 'v' => v = true,
        Expr::Subquery(s) => match s.func {
            Some('i' | 's') => {}
            Some(_) => v = true,
            None => v |= plan_volatile(&s.plan),
        },
        _ => {}
    });
    v
}

pub(super) fn plan_volatile(p: &Plan) -> bool {
    if p.writes() {
        return true;
    }
    let mut es = Vec::new();
    crate::sql::explain::node_exprs(p, &mut es);
    if es.iter().any(|e| volatile(e)) {
        return true;
    }
    match p {
        Plan::FunctionScan { calls, .. } | Plan::ProjectSet { srfs: calls, .. }
            if calls
                .iter()
                .any(|c| c.func.volatility == 'v' || c.body.as_deref().is_some_and(plan_volatile)) =>
        {
            return true;
        }
        Plan::PlCall { func, .. } if func.volatility == 'v' => return true,
        _ => {}
    }
    crate::sql::explain::plan_inputs(p).iter().any(|c| plan_volatile(c))
}

/// An output column to drop: not volatile, not set-returning (and not a constant already).
fn droppable(e: &Expr) -> bool {
    let mut srf = false;
    e.walk(&mut |x| srf |= matches!(x, Expr::Srf(..)));
    // (A column read costs nothing and raises nothing: left alone, so the plan's
    // shapes — an inheritance member's projection of the parent's columns — stay.)
    !srf && !matches!(e, Expr::Const(..) | Expr::Col(..)) && !volatile(e)
}

impl Planner<'_, '_> {
    /// `needed`: the columns of `p`'s row its consumers read.
    pub(super) fn drop_unused_outputs(&self, p: &mut Plan, needed: &[bool]) {
        let unused = |i: usize| !needed.get(i).copied().unwrap_or(true);
        match p {
            Plan::Project { input, exprs } => {
                // (Over a ProjectSet: an expression of a call's value is set-returning.)
                let srf_cols: Vec<usize> = match &**input {
                    Plan::ProjectSet {
                        input: pin,
                        exprs: pexprs,
                        ..
                    } => {
                        let iw = crate::sql::binder::plan_width(pin);
                        (0..pexprs.len())
                            .filter(|&k| pexprs[k].columns().iter().any(|&c| c >= iw))
                            .collect()
                    }
                    _ => Vec::new(),
                };
                for (i, e) in exprs.iter_mut().enumerate() {
                    if unused(i) && droppable(e) && !e.columns().iter().any(|c| srf_cols.contains(c)) {
                        *e = Expr::Const(Value::Null, e.ty());
                    }
                }
            }
            Plan::ProjectSet { input, exprs, .. } => {
                // (Not those over the calls' values: the calls run anyway.)
                let iw = crate::sql::binder::plan_width(input);
                for (i, e) in exprs.iter_mut().enumerate() {
                    if unused(i) && droppable(e) && e.columns().iter().all(|&c| c < iw) {
                        *e = Expr::Const(Value::Null, e.ty());
                    }
                }
            }
            _ => {}
        }
        // An aggregate the projection above doesn't read isn't computed either.
        if let Plan::Project { input, exprs } = p
            && let Plan::Agg { groups, aggs, .. } = &**input
        {
            let ng = groups.len();
            let mut read = vec![false; ng + aggs.len()];
            for (i, e) in exprs.iter().enumerate() {
                if !unused(i) || !matches!(e, Expr::Col(..)) {
                    mark(&mut read, e);
                }
            }
            let gone: Vec<usize> = (0..aggs.len())
                .filter(|&k| {
                    let a = &aggs[k];
                    !read[ng + k]
                        && !a.args.iter().chain(&a.direct).chain(&a.filter).any(volatile)
                        && !a.order_by.iter().any(|o| volatile(&o.expr))
                })
                .collect();
            if !gone.is_empty() {
                let Plan::Agg { aggs, .. } = take(input) else { unreachable!() };
                for &k in gone.iter().rev() {
                    aggs.remove(k);
                }
                for e in exprs.iter_mut() {
                    // (An unread column of a dropped aggregate: NULL.)
                    if let Expr::Col(i, t) = e
                        && *i >= ng
                        && gone.contains(&(*i - ng))
                    {
                        *e = Expr::Const(Value::Null, t.clone());
                        continue;
                    }
                    *e = std::mem::replace(e, Expr::Const(Value::Null, DataType::Unknown))
                        .transform(&mut |x| {
                            Ok(match x {
                                Expr::Col(i, t) if i >= ng => {
                                    let below = gone.iter().filter(|&&k| ng + k < i).count();
                                    Expr::Col(i - below, t)
                                }
                                x => x,
                            })
                        })
                        .unwrap();
                }
            }
        }
        let mut needs = child_needs(p, needed);
        // DISTINCT ON: its keys (the sort below has them too) and what's read.
        if let Plan::Distinct { input, on: Some(on), .. } = &*p {
            let mut n = vec![false; crate::sql::binder::plan_width(input)];
            for (i, x) in needed.iter().enumerate() {
                if *x && i < n.len() {
                    n[i] = true;
                }
            }
            on.iter().for_each(|e| mark(&mut n, e));
            needs = vec![n];
        }
        let mut needs = needs.into_iter();
        let mut visit = |c: &mut Arc<Plan>| {
            let n = needs
                .next()
                .unwrap_or_else(|| vec![true; crate::sql::binder::plan_width(c)]);
            self.drop_unused_outputs(take(c), &n);
        };
        match p {
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::ProjectSet { input, .. }
            | Plan::Agg { input, .. }
            | Plan::Window { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input, .. }
            | Plan::LockRows { input, .. }
            | Plan::Barrier { input, .. } | Plan::Context { input, .. } => visit(input),
            Plan::Join { left, right, .. } => {
                visit(left);
                visit(right);
            }
            // (Not into an inheritance tree's members: scans of the parent's columns.)
            Plan::Append { inputs, inh: None } => inputs.iter_mut().for_each(visit),
            // (Each arm's whole row: the set operation compares them.)
            Plan::SetOp { left, right, .. } => {
                for c in [left, right] {
                    let w = crate::sql::binder::plan_width(c);
                    self.drop_unused_outputs(take(c), &vec![true; w]);
                }
            }
            Plan::With { body, .. } => visit(body),
            _ => {}
        }
    }
}
