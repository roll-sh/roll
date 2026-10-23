//! pull_up_simple_subquery for a FROM item that is one row of constants (`(SELECT 123 AS
//! x) ss`, a subquery without FROM): an inner join with it is its other side, the
//! constants put in for its columns (in the join's clauses, which then restrict that
//! side, and in the row).

use super::*;
use crate::sql::binder::optimize::add_filter;

/// The constants a one-row FROM item without a FROM list returns; or a FROM-clause call
/// of an immutable non-set function over constants, made its value
/// (pull_up_constant_function).
fn const_rel(p: &Plan, ctx: &ExecCtx) -> Option<Vec<Expr>> {
    if let Plan::FunctionScan {
        calls,
        ordinality: false,
        ..
    } = p
        && let [c] = &calls[..]
        && c.ncols == 1
        && c.body.is_none()
        && c.func.srf.is_none()
        && c.func.volatility == 'i'
        && c.args.iter().all(|a| matches!(a, Expr::Const(..)))
        && !matches!(c.ty, DataType::Row(_) | DataType::Record)
    {
        let call = Expr::Call(c.func.clone(), c.args.clone(), c.ty.clone());
        let v = crate::sql::expr::eval(&call, &[], ctx).ok()?;
        return Some(vec![Expr::Const(v, c.ty.clone())]);
    }
    // (A one-row VALUES list is pulled up as that row: pull_up_simple_values.)
    // (A projection over such a row composes with it: a rule's `*RULE*` row over a
    // statement's VALUES.)
    let composed;
    let exprs = match p {
        Plan::Project { input, exprs } if matches!(&**input, Plan::One) => exprs,
        Plan::Values { rows, .. } if rows.len() == 1 => &rows[0],
        Plan::Project { input, exprs } if matches!(&**input, Plan::Values { rows, .. } if rows.len() == 1) => {
            let inner = const_rel(input, ctx)?;
            composed = exprs
                .iter()
                .map(|e| {
                    e.clone()
                        .transform(&mut |x| {
                            Ok(match x {
                                Expr::Col(i, t) => inner.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                                x => x,
                            })
                        })
                        .unwrap()
                })
                .collect::<Vec<_>>();
            &composed
        }
        _ => return None,
    };
    let ok = exprs.iter().all(|e| {
        let mut plain = e.columns().is_empty() && !e.is_volatile() && !e.has_outer_refs();
        e.walk(&mut |x| {
            if matches!(x, Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..)) {
                plain = false
            }
        });
        plain
    });
    ok.then(|| exprs.clone())
}

impl Planner<'_, '_> {
    pub(super) fn pull_up_consts(&self, p: &mut Plan) {
        match p {
            Plan::With { ctes, body, .. } => {
                for (_, c) in ctes.iter_mut() {
                    self.pull_up_consts(take(c));
                }
                self.pull_up_consts(take(body));
            }
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::ProjectSet { input, .. }
            | Plan::Agg { input, .. }
            | Plan::Window { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input, .. }
            | Plan::LockRows { input, .. }
            | Plan::Barrier { input, .. } | Plan::Context { input, .. } => self.pull_up_consts(take(input)),
            Plan::SetOp { left, right, .. } | Plan::Join { left, right, .. } => {
                self.pull_up_consts(take(left));
                self.pull_up_consts(take(right));
            }
            Plan::Append { inputs, .. } => {
                for i in inputs.iter_mut() {
                    self.pull_up_consts(take(i));
                }
            }
            Plan::Recursive {
                base, recursive, ..
            } => {
                self.pull_up_consts(take(base));
                self.pull_up_consts(take(recursive));
            }
            Plan::Insert(i) => self.pull_up_consts(take(&mut i.source)),
            Plan::Update(u) => self.pull_up_consts(take(&mut u.source)),
            Plan::Delete(d) => self.pull_up_consts(take(&mut d.source)),
            Plan::Merge(m) => self.pull_up_consts(take(&mut m.source)),
            _ => {}
        }
        // pull_up_constant_function: a function in FROM whose call folds to a scalar
        // constant is a one-row item of it.
        if let Plan::Join { meta, .. } = p
            && let Some(fc) = &meta.from_call
            && let Some(e @ Expr::Const(..)) = &fc.call
            && fc.cols.len() == 1
            && !matches!(e.ty(), DataType::Row(_) | DataType::Record)
        {
            *p = Plan::Project {
                input: Arc::new(Plan::One),
                exprs: vec![e.clone()],
            };
        }
        // A filter over a constant row is constant itself: the row, or no rows.
        if let Plan::Filter { input, pred } = p
            && let Some(consts) = const_rel(input, self.ctx)
        {
            let q = pred
                .clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(i, t) => consts.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                        x => x,
                    })
                })
                .unwrap();
            let mut plain = q.columns().is_empty() && !q.is_volatile() && !q.has_outer_refs();
            q.walk(&mut |x| plain &= !matches!(x, Expr::Subquery(_) | Expr::Param(..)));
            if plain && let Ok(v) = crate::sql::expr::eval(&q, &[], self.ctx) {
                *p = if matches!(v, Value::Bool(true)) {
                    (**input).clone()
                } else {
                    Plan::Empty {
                        ncols: consts.len(),
                        cols: None,
                        info: None,
                    }
                };
            } else if q.columns().is_empty() && !super::expr_volatile_deep(&q) && !q.has_outer_refs() && pred.columns() != q.columns() {
                // (One with a sub-select: the row's values put in for its columns, its
                // parameters too (ReplaceVarsFromTargetList), a clause of no variable left,
                // the sub-select uncorrelated: a gating qual, an InitPlan.)
                *pred = q;
            }
        }
        if let Some(out) = lateral_one_row(p) {
            *p = out;
            // (Its range table entry stays: EXPLAIN qualifies the columns.)
            self.removed.set(self.removed.get() + 1);
        }
        if let Some(out) = self.pulled_join(p) {
            *p = out;
        }
        flatten_union_all(p);
    }

    fn pulled_join(&self, p: &Plan) -> Option<Plan> {
        let Plan::Join {
            kind: kind @ (JoinKind::Inner | JoinKind::Left | JoinKind::Right),
            left,
            right,
            cond,
            hash,
            params,
            nleft,
            nright,
            meta,
        } = p
        else {
            return None;
        };
        if !params.is_empty() {
            return None;
        }
        let (nl, nr) = (*nleft, *nright);
        // (On an outer join's preserved side the constants are never nulled: they go into
        // its clauses all the same, the side staying as the row it is.)
        // (An inner join's constant side may be gated by a clause of no variable (a rule's
        // qualification over the statement's VALUES row): the gate goes over the join's
        // other side.)
        let gated = |s: &Plan| -> Option<(Vec<Expr>, Expr)> {
            let Plan::Filter { input, pred } = s else { return None };
            (*kind == JoinKind::Inner && pred.columns().is_empty() && !super::expr_volatile_deep(pred) && !pred.has_outer_refs())
                .then(|| const_rel(input, self.ctx).map(|c| (c, pred.clone())))
                .flatten()
        };
        let (consts, const_left, gate) = match (const_rel(left, self.ctx), const_rel(right, self.ctx)) {
            (_, Some(c)) => (c, false, None),
            (Some(c), _) => (c, true, None),
            _ => match (gated(left), gated(right)) {
                (_, Some((c, g))) => (c, false, Some(g)),
                (Some((c, g)), _) => (c, true, Some(g)),
                _ => return None,
            },
        };
        // (An outer join's nullable constants: in its own clauses only; above it they
        // go NULL with the row, so that side stays, a Result.)
        let outer = *kind != JoinKind::Inner;
        if outer && cond.is_none() && hash.is_none() {
            return None;
        }
        // The join's clauses over the other side's row.
        let subst = |e: Expr| -> Expr {
            e.transform(&mut |x| {
                Ok(match x {
                    Expr::Col(i, t) => {
                        if const_left {
                            if i < nl {
                                consts[i].clone()
                            } else if outer {
                                // (An outer join keeps its row.)
                                Expr::Col(i, t)
                            } else {
                                Expr::Col(i - nl, t)
                            }
                        } else if i >= nl {
                            consts[i - nl].clone()
                        } else {
                            Expr::Col(i, t)
                        }
                    }
                    x => x,
                })
            })
            .unwrap()
        };
        let mut conds = Vec::new();
        if let Some(c) = cond {
            conds.push(subst(c.clone()));
        }
        if let Some((lk, rk)) = hash {
            let mut b = crate::sql::binder::Binder::new(self.ctx, Vec::new());
            for (i, (a, k)) in lk.iter().zip(rk).enumerate() {
                let unwrap = |k: &Expr| match k {
                    Expr::Call(d, args, _) if d.name == crate::sql::binder::INDETERMINATE_HASH => {
                        args[0].clone()
                    }
                    k => k.clone(),
                };
                let (a, k) = (unwrap(a), unwrap(k).shift_cols(nl as isize));
                // (An outer join's clause as written; an inner join's from its equivalence
                // class, the column first.)
                let (a, k) = if (outer && meta.hash_swapped.get(i) == Some(&true))
                    || (!outer && const_left)
                {
                    (k, a)
                } else {
                    (a, k)
                };
                let eq = b.builtin_binop("=", subst(a), subst(k)).ok()?;
                conds.push(eq);
            }
        }
        if outer {
            let j = crate::sql::binder::optimize::join(
                &self.cat,
                *kind,
                (**left).clone(),
                (**right).clone(),
                Some(Expr::And(conds)),
                Vec::new(),
                nl,
                nr,
            );
            return Some(j);
        }
        let other = if const_left { right } else { left };
        let mut other = (**other).clone();
        if !conds.is_empty() {
            other = add_filter(&self.cat, other, Expr::And(conds));
        }
        let types = super::analyzejoins::col_types(&other);
        let n_other = if const_left { nr } else { nl };
        let cols = (0..n_other).map(|i| Expr::Col(i, types.get(i).cloned().unwrap_or(DataType::Unknown)));
        let exprs: Vec<Expr> = if const_left {
            consts.into_iter().chain(cols).collect()
        } else {
            cols.chain(consts).collect()
        };
        let merged = Plan::Project {
            input: Arc::new(other),
            exprs,
        };
        Some(match gate {
            Some(pred) => Plan::Filter {
                input: Arc::new(merged),
                pred,
            },
            None => merged,
        })
    }
}

/// An expression a UNION ALL member's output can be made of (pull_up_simple_union_all's
/// simple subquery target list): nothing volatile, set-returning or nested.
fn simple_target(e: &Expr) -> bool {
    let mut ok = !e.is_volatile();
    e.walk(&mut |x| {
        if matches!(
            x,
            Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..) | Expr::Param(..)
        ) {
            ok = false
        }
    });
    ok
}

/// Nested UNION ALLs as one (pull_up_simple_union_all's flattened append relation): a
/// member that is itself a UNION ALL, through a simple projection of its rows, gives its
/// members, each projected as the outer one says.
fn flatten_union_all(p: &mut Plan) {
    let Plan::Append { inputs, inh: None, .. } = p else {
        return;
    };
    let nested = |i: &Plan| match i {
        Plan::Append { inh: None, .. } => true,
        Plan::Project { input, exprs } => {
            matches!(&**input, Plan::Append { inh: None, .. }) && exprs.iter().all(simple_target)
        }
        _ => false,
    };
    if !inputs.iter().any(|i| nested(i)) {
        return;
    }
    let mut out: Vec<Arc<Plan>> = Vec::new();
    for i in std::mem::take(inputs) {
        if !nested(&i) {
            out.push(i);
            continue;
        }
        let (members, exprs) = match &*i {
            Plan::Append { inputs, .. } => (inputs.clone(), None),
            Plan::Project { input, exprs } => match &**input {
                Plan::Append { inputs, .. } => (inputs.clone(), Some(exprs.clone())),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let Some(exprs) = exprs else {
            out.extend(members);
            continue;
        };
        for m in members {
            out.push(Arc::new(match &*m {
                Plan::Empty { .. } => Plan::Empty {
                    ncols: exprs.len(),
                    cols: None,
                    info: None,
                },
                // (A member's own projection, composed with the outer one.)
                Plan::Project { input, exprs: inner } if inner.iter().all(simple_target) => {
                    let composed = exprs
                        .iter()
                        .map(|e| {
                            e.clone()
                                .transform(&mut |x| {
                                    Ok(match x {
                                        Expr::Col(k, t) => {
                                            inner.get(k).cloned().unwrap_or(Expr::Col(k, t))
                                        }
                                        x => x,
                                    })
                                })
                                .unwrap()
                        })
                        .collect();
                    Plan::Project {
                        input: input.clone(),
                        exprs: composed,
                    }
                }
                _ => Plan::Project {
                    input: m.clone(),
                    exprs: exprs.clone(),
                },
            }));
        }
    }
    *inputs = out;
}

/// pull_up_simple_subquery of a LATERAL subquery without FROM (an inlined SQL function's
/// body, `(SELECT f(t.x)) ss`): its one row's expressions, over the other side's row, in
/// place of the join.
fn lateral_one_row(p: &Plan) -> Option<Plan> {
    let Plan::Join {
        kind: JoinKind::Inner | JoinKind::Left,
        left,
        right,
        cond: None,
        hash: None,
        params,
        nleft,
        nright,
        meta,
    } = p
    else {
        return None;
    };
    if meta.from_call.is_some() {
        return None;
    }
    // (Without params the right side reads the enclosing frames directly.)
    let framed = !params.is_empty();
    if !framed && !matches!(&**left, Plan::One) {
        return None;
    }
    let exprs = &one_row_exprs(right)?;
    if exprs.len() != *nright {
        return None;
    }
    // (Nothing evaluated apart: sub-selects, set-returning or volatile calls.)
    let mut plain = true;
    for e in exprs {
        plain &= !e.is_volatile();
        e.walk(&mut |x| {
            plain &= !matches!(x, Expr::Subquery(_) | Expr::Srf(..) | Expr::Agg(..) | Expr::Window(..))
        });
    }
    if !plain {
        return None;
    }
    let mut out: Vec<Expr> = (0..*nleft)
        .map(|i| Expr::Col(i, DataType::Unknown))
        .collect();
    let types = super::analyzejoins::col_types(left);
    for (i, e) in out.iter_mut().enumerate() {
        if let Some(t) = types.get(i) {
            *e = Expr::Col(i, t.clone());
        }
    }
    for e in exprs {
        out.push(
            e.clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Outer(1, i, t) if framed => {
                            params.get(i).cloned().unwrap_or(Expr::Outer(1, i, t))
                        }
                        Expr::Outer(l, i, t) if framed && l > 1 => Expr::Outer(l - 1, i, t),
                        // (A field of a whole-row argument: its column.)
                        Expr::Field(r, i, t) => match *r {
                            Expr::Row(items, _) if i < items.len() && items[i].ty() == t => {
                                items.into_iter().nth(i).expect("checked")
                            }
                            r => Expr::Field(Box::new(r), i, t),
                        },
                        x => x,
                    })
                })
                .ok()?,
        );
    }
    Some(Plan::Project {
        input: Arc::new(crate::sql::plan::mark_phv_eval(left)),
        exprs: out,
    })
}

/// The expressions of a one-row plan of projections over no input (composed).
fn one_row_exprs(p: &Plan) -> Option<Vec<Expr>> {
    match p {
        Plan::Project { input, exprs } if matches!(&**input, Plan::One) => Some(exprs.clone()),
        Plan::Project { input, exprs } => {
            let below = one_row_exprs(input)?;
            exprs
                .iter()
                .map(|e| {
                    e.clone()
                        .transform(&mut |x| {
                            Ok(match x {
                                Expr::Col(i, t) => below.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                                x => x,
                            })
                        })
                        .ok()
                })
                .collect()
        }
        _ => None,
    }
}
